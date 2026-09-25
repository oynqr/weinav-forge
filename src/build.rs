use crate::{
    cache::{self, Manifest, Source},
    orbit,
    policy::{Flavor, Plan, Role, System},
    record::{self, Epoch},
    rinex::Broadcast,
    seed::{Acceleration, Satellite, Seed, State},
    sp3::{Antex, Sp3},
    time::Instant,
};
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Default)]
pub struct Inputs {
    pub sources: Vec<Source>,
    pub seed: Option<Seed>,
    pub satellites: BTreeMap<System, BTreeMap<u8, Satellite>>,
    pub acceleration: Option<Acceleration>,
    pub broadcast: Broadcast,
    pub predictions: BTreeMap<Role, Sp3>,
    pub antex: Option<Antex>,
    pub raw: BTreeMap<Role, Vec<Vec<u8>>>,
}

impl Inputs {
    pub fn provider_names(&self, provider: &str) -> Vec<String> {
        if self.uses_broadcast(provider) {
            return vec!["brdc".into()];
        }
        if let Some(role) = Self::role(provider) {
            self.sources
                .iter()
                .filter(|s| s.role == role)
                .map(|s| s.provider.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            vec![provider.into()]
        }
    }
    pub fn load(
        cache_root: &Path,
        manifest: Option<&Path>,
        flavor: Flavor,
        systems: &[System],
        agnss: bool,
        local: &BTreeMap<Role, Vec<PathBuf>>,
    ) -> Result<Self> {
        let mut inputs = Self::default();
        let required = Plan::new(flavor, systems, agnss).sources;
        let path = manifest
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cache_root.join(crate::fetch::manifest_name(flavor)));
        if path.exists() {
            let m: Manifest = serde_json::from_slice(&cache::read(&path)?)?;
            ensure!(
                m.version == 1 && m.flavor == flavor,
                "manifest version or flavor differs from request"
            );
            for source in m.sources {
                if local.contains_key(&source.role) || !required.contains(&source.role) {
                    continue;
                }
                inputs.add_source(source.role, cache::load_source(cache_root, &source)?)?;
                inputs.sources.push(source);
            }
        } else {
            ensure!(manifest.is_none(), "explicit manifest does not exist");
        }
        for (&role, paths) in local {
            ensure!(
                required.contains(&role),
                "source override is not used by this plan: {role:?}"
            );
            for path in paths {
                let bytes = cache::read(path)?;
                inputs.sources.push(Source {
                    role,
                    provider: crate::fetch::provider(role).into(),
                    url: format!("local:{}", path.display()),
                    sha256: cache::hash(&bytes),
                    bytes: bytes.len(),
                    fetched_at: Instant::now().0.to_rfc3339(),
                    etag: None,
                    last_modified: None,
                    coverage: crate::fetch::inspect(role, &bytes)?,
                });
                inputs.add_source(role, bytes)?;
            }
        }
        for role in required {
            ensure!(
                inputs.sources.iter().any(|source| source.role == role)
                    || matches!(
                        role,
                        Role::Prediction | Role::BdsPrediction | Role::QzsPrediction
                    ),
                "required source is absent: {role:?}"
            );
        }
        if let Some(seed) = &inputs.seed {
            for &system in systems {
                inputs.satellites.insert(system, seed.nav(system)?);
            }
            if systems.contains(&System::Glonass) {
                inputs.acceleration = Some(Acceleration::parse(seed)?);
            }
        }
        Ok(inputs)
    }

    fn add_source(&mut self, role: Role, bytes: Vec<u8>) -> Result<()> {
        match role {
            Role::Seed => {
                ensure!(self.seed.is_none(), "supply one seed");
                self.seed = Some(Seed::parse(&bytes)?);
            }
            Role::Broadcast => self.broadcast.add(&bytes)?,
            Role::Prediction | Role::BdsPrediction | Role::QzsPrediction => {
                self.predictions.entry(role).or_default().add(&bytes)?;
            }
            Role::Antex => {
                ensure!(self.antex.is_none(), "supply one ANTEX file");
                self.antex = Some(Antex::parse(&bytes)?);
            }
            Role::Agnss | Role::GpsAlmanac | Role::GalileoAlmanac | Role::QzsAlmanac => {
                self.raw.entry(role).or_default().push(bytes);
            }
        }
        Ok(())
    }

    pub fn role(provider: &str) -> Option<Role> {
        match provider {
            "code5d" => Some(Role::Prediction),
            "wum-nrt" => Some(Role::BdsPrediction),
            "qzu" => Some(Role::QzsPrediction),
            _ => None,
        }
    }

    fn uses_broadcast(&self, provider: &str) -> bool {
        provider == "code5d" && !self.predictions.contains_key(&Role::Prediction)
    }

    fn ids(&self, system: System, provider: &str) -> BTreeSet<u8> {
        if self.uses_broadcast(provider) {
            self.broadcast
                .records
                .keys()
                .filter(|k| k.0 == system)
                .map(|k| k.1)
                .collect()
        } else if provider == "hiee" {
            self.satellites
                .get(&system)
                .map(|s| s.keys().copied().collect())
                .unwrap_or_default()
        } else if let Some(sp3) = Self::role(provider).and_then(|r| self.predictions.get(&r)) {
            sp3.satellites
                .keys()
                .filter(|k| k.0 == system)
                .map(|k| k.1)
                .collect()
        } else {
            BTreeSet::new()
        }
    }

    fn seed_state(&self, system: System, id: u8, time: f64) -> Result<State> {
        let satellite = self
            .satellites
            .get(&system)
            .and_then(|s| s.get(&id))
            .context("satellite absent from seed")?;
        let arc = satellite
            .arc_at(time)
            .context("seed does not cover sample")?;
        ensure!(
            satellite.health == 0 && arc.flag == 0,
            "seed satellite or arc is unhealthy"
        );
        arc.evaluate(time)
    }

    fn state(&self, system: System, id: u8, time: f64, provider: &str) -> Result<State> {
        if self.uses_broadcast(provider) {
            let nav = self
                .broadcast
                .nearest(system, id, time)
                .context("broadcast fallback has no fresh epoch")?;
            ensure!(nav.healthy(), "broadcast fallback is unhealthy");
            nav.state(time)
        } else if provider == "hiee" {
            self.seed_state(system, id, time)
        } else {
            let role = Self::role(provider).context("unknown orbit provider")?;
            let state = self
                .predictions
                .get(&role)
                .context("prediction source absent")?
                .state((system, id), time)?;
            self.antex
                .as_ref()
                .context("ANTEX absent")?
                .correct((system, id), time, state)
        }
    }

    fn covers(&self, system: System, provider: &str, start: f64, end: f64, at: Instant) -> bool {
        let ids: Vec<_> = self
            .ids(system, provider)
            .into_iter()
            .filter(|&id| {
                self.broadcast
                    .nearest(system, id, at.gps() as f64)
                    .is_some_and(|n| n.healthy())
                    && !(system == System::Bds && (id <= 5 || id >= 59))
            })
            .collect();
        !ids.is_empty()
            && ids.into_iter().all(|id| {
                self.predictions
                    .get(&Self::role(provider).unwrap())
                    .and_then(|p| p.window((system, id)))
                    .is_some_and(|(a, b)| a <= start && b >= end)
            })
    }
}

#[derive(Serialize)]
pub struct EpochReport {
    pub system: System,
    pub time: i64,
    pub orbit: String,
    pub clock: String,
    pub actual_orbit_providers: Vec<String>,
    pub actual_clock_providers: Vec<String>,
    pub source_hashes: Vec<String>,
    pub counts: Vec<usize>,
    pub fit_rms_max_m: f64,
    pub quantized_rms_max_m: f64,
    pub removals: Vec<String>,
}

pub struct Products {
    pub files: BTreeMap<String, Vec<u8>>,
    pub epochs: Vec<EpochReport>,
    pub notes: Vec<String>,
    pub screened: usize,
}

fn clock_fit(samples: &[(f64, State)], quadratic: bool) -> Result<[f64; 3]> {
    let columns = if quadratic { 3 } else { 2 };
    let matrix = nalgebra::DMatrix::from_fn(samples.len(), columns, |i, j| {
        (samples[i].0 / 3600.0).powi(j as i32)
    });
    let clocks =
        nalgebra::DVector::from_iterator(samples.len(), samples.iter().map(|(_, s)| s.clock));
    let solution = (matrix.transpose() * &matrix)
        .lu()
        .solve(&(matrix.transpose() * clocks))
        .context("singular clock fit")?;
    Ok([
        solution[0],
        solution[1] / 3600.0,
        if quadratic {
            solution[2] / 3600_f64.powi(2)
        } else {
            0.0
        },
    ])
}

fn kepler(
    inputs: &Inputs,
    system: System,
    id: u8,
    time: f64,
    provider: &str,
    fit_limit: f64,
    at: Instant,
) -> Result<(Vec<u8>, f64, f64)> {
    let toe = (time - if system == System::Bds { 14.0 } else { 0.0 }).rem_euclid(604800.0);
    let center = inputs.state(system, id, time, provider)?;
    let start = orbit::initial(&center, toe, system)?;
    let samples: Vec<_> = (-24..=24)
        .map(|i| {
            let dt = f64::from(i) * 150.0;
            Ok((dt, inputs.state(system, id, time + dt, provider)?))
        })
        .collect::<Result<_>>()?;
    let positions: Vec<_> = samples.iter().map(|(dt, s)| (*dt, s.position)).collect();
    let pinned = if system == System::Qzs {
        let wide: Vec<_> = (-24..=24)
            .map(|i| {
                let dt = f64::from(i) * 300.0;
                Ok((dt, inputs.state(system, id, time + dt, provider)?.position))
            })
            .collect::<Result<_>>()?;
        Some(orbit::fit(&wide, toe, system, start, None)?.parameters[6])
    } else {
        None
    };
    let fit = orbit::fit(&positions, toe, system, start, pinned)?;
    ensure!(
        fit.rms <= fit_limit,
        "fit RMS {:.3} m exceeds limit",
        fit.rms
    );
    let delay = if provider == "hiee" {
        [inputs.satellites[&system][&id].tgd, 0.0]
    } else {
        let nav = inputs
            .broadcast
            .nearest(system, id, at.gps() as f64)
            .context("broadcast delay missing")?;
        [
            nav.get(match system {
                System::Bds => "tgd1",
                System::Galileo => "bgd_e5a",
                _ => "tgd",
            })?,
            0.0,
        ]
    };
    let mut clock = clock_fit(&samples, system == System::Bds && provider != "hiee")?;
    if system == System::Bds && provider != "hiee" {
        let gamma = (1561.098_f64 / 1268.52).powi(2);
        clock[0] += gamma / (gamma - 1.0) * delay[0];
    }
    let values = orbit::record_values(system, id, time, &fit.parameters, clock, delay);
    let bytes = record::encode(system, &values)?;
    let values = record::decode(system, &bytes)?;
    record::validate(system, &values)?;
    record::validate_envelope(system, &values, provider == "hiee")?;
    let p = orbit::parameters(&values)?;
    let mut residual = 0.0;
    for (dt, state) in &samples {
        let xyz = orbit::position(&p, *dt, toe, system, false)?;
        residual += (0..3)
            .map(|i| (xyz[i] - state.position[i]).powi(2))
            .sum::<f64>();
    }
    let quantized = (residual / samples.len() as f64).sqrt();
    ensure!(
        quantized <= fit_limit + 0.5,
        "quantized RMS {quantized:.3} m exceeds limit plus 0.5 m"
    );
    Ok((bytes, fit.rms, quantized))
}

fn glonass(
    inputs: &Inputs,
    id: u8,
    index: i64,
    subblock: usize,
    provider: &str,
    clock: &str,
    at: Instant,
) -> Result<Vec<u8>> {
    let leap = inputs
        .seed
        .as_ref()
        .map(|s| i64::from(s.leap))
        .unwrap_or(at.gps() - (at.0.timestamp() - crate::time::GPS_EPOCH_UNIX));
    let time = (index + 900 * subblock as i64 - 3600 + leap) as f64;
    let state = inputs.state(System::Glonass, id, time, provider)?;
    let clock_state = if clock == provider {
        state.clone()
    } else {
        inputs.seed_state(System::Glonass, id, time)?
    };
    let acceleration = if provider == "hiee" {
        inputs.acceleration.as_ref().and_then(|a| a.at(id, time))
    } else {
        None
    };
    let acceleration = acceleration.unwrap_or_else(|| {
        let model = orbit::glonass_acceleration(state.position, state.velocity);
        std::array::from_fn(|i| state.acceleration[i] - model[i])
    });
    let mut values = record::zero_values(System::Glonass);
    for (name, value) in [
        ("slot", f64::from(id - 1)),
        ("t_b", (time - leap as f64 + 10800.0).rem_euclid(86400.0)),
        ("tau_n", -clock_state.clock),
        (
            "gamma_n",
            if clock == "hiee" {
                0.0
            } else {
                clock_state.drift
            },
        ),
        ("flag", 1.0),
    ] {
        values.insert(name.into(), value);
    }
    for (i, axis) in ["x", "y", "z"].into_iter().enumerate() {
        values.insert(axis.into(), state.position[i] / 1000.0);
        values.insert(format!("v{axis}"), state.velocity[i] / 1000.0);
        values.insert(format!("a{axis}"), acceleration[i] / 1000.0);
    }
    record::truncate(System::Glonass, &mut values);
    record::validate(System::Glonass, &values)?;
    record::encode(System::Glonass, &values)
}

pub fn assemble(
    inputs: &Inputs,
    flavor: Flavor,
    systems: &[System],
    agnss: bool,
    at: Instant,
    fit_limit: f64,
) -> Result<Products> {
    ensure!(
        fit_limit.is_finite() && fit_limit > 0.0,
        "fit limit must be positive"
    );
    let plan = Plan::new(flavor, systems, agnss);
    let mut product = Products {
        files: BTreeMap::new(),
        epochs: Vec::new(),
        notes: Vec::new(),
        screened: 0,
    };
    if let Some(seed) = &inputs.seed {
        ensure!(
            seed.brackets(at.gps() as f64, (at.gps() + 43200) as f64),
            "seed is stale or has less than 12 hours left"
        );
        for &system in systems {
            let grid = at.grid(system);
            ensure!(
                seed.brackets((grid[0] - 7200) as f64, (grid[35] + 7200) as f64),
                "seed does not bracket the complete sample grid"
            );
        }
        let (extra, notes) = crate::extra::build(seed)?;
        ensure!(
            !notes.iter().any(|n| n.contains("inconsistent")),
            "seed EXTRA has inconsistent counts: {}",
            notes.join("; ")
        );
        product.notes.extend(notes);
        product.files.insert("HW_PGNSS_EXTRA".into(), extra);
        if inputs
            .satellites
            .values()
            .flat_map(|s| s.values())
            .flat_map(|s| &s.arcs)
            .any(|a| a.padded_coefficient)
        {
            product
                .notes
                .push("QZSS: final seed coefficient uses four documented zero padding bits".into());
        }
    } else {
        let (extra, notes) = crate::almanac::build(
            &inputs.raw[&Role::GpsAlmanac][0],
            &inputs.raw[&Role::GalileoAlmanac][0],
            &inputs.broadcast,
            at,
        )?;
        product.files.insert("HW_PGNSS_EXTRA".into(), extra);
        product.notes.extend(notes);
    }
    if agnss {
        let bytes = if matches!(flavor, Flavor::Huawei | Flavor::HuaweiPlus) {
            crate::seed::decompress(&inputs.raw[&Role::Agnss][0])?.into_owned()
        } else {
            crate::agnss::build(&inputs.broadcast, systems, at)?
        };
        crate::agnss::validate_fresh(&bytes, at)?;
        product.files.insert("HW_AGNSS_RTCM_33".into(), bytes);
    }
    for &system in systems {
        let name = format!("HW_PGNSS_{}", system.name());
        let policy = &plan.products[&name];
        let mut tail = false;
        let schedule: Vec<_> = at
            .grid(system)
            .into_iter()
            .map(|time| {
                let width = if system == System::Qzs {
                    7200.0
                } else {
                    3600.0
                };
                let orbit = if let Some(fallback) = policy.beyond_horizon {
                    if tail
                        || !inputs.covers(
                            system,
                            policy.orbit,
                            time as f64 - width,
                            time as f64 + width,
                            at,
                        )
                    {
                        tail = true;
                        fallback
                    } else {
                        policy.orbit
                    }
                } else {
                    policy.orbit
                };
                let clock = if orbit != policy.orbit {
                    orbit
                } else {
                    policy.clock
                };
                (time, orbit, clock)
            })
            .collect();
        let results: Vec<Result<_>> = schedule
            .par_iter()
            .map(|&(time, orbit, clock)| {
                let hashes = inputs
                    .sources
                    .iter()
                    .filter(|s| {
                        s.role == Role::Broadcast
                            || s.role == Role::Antex
                            || Some(s.role) == Inputs::role(orbit)
                            || s.role == Role::Seed && (orbit == "hiee" || clock == "hiee")
                    })
                    .map(|s| s.sha256.clone())
                    .collect();
                let mut report = EpochReport {
                    system,
                    time,
                    orbit: orbit.into(),
                    clock: clock.into(),
                    actual_orbit_providers: inputs.provider_names(orbit),
                    actual_clock_providers: inputs.provider_names(clock),
                    source_hashes: hashes,
                    counts: Vec::new(),
                    fit_rms_max_m: 0.0,
                    quantized_rms_max_m: 0.0,
                    removals: Vec::new(),
                };
                let mut blocks = Vec::new();
                let mut screened_ids = BTreeSet::new();
                for id in inputs.ids(system, orbit) {
                    if system == System::Bds && (id <= 5 || id >= 59) {
                        report.removals.push(format!("{id}: GEO excluded"));
                        continue;
                    }
                    let screen = (|| -> Result<()> {
                        let nav = inputs
                            .broadcast
                            .nearest(system, id, at.gps() as f64)
                            .context("absent from fresh broadcast")?;
                        ensure!(nav.healthy(), "broadcast satellite unhealthy");
                        let reference = nav.state(at.gps() as f64)?;
                        let candidate = inputs.state(system, id, at.gps() as f64, orbit)?;
                        let distance = (0..3)
                            .map(|i| (reference.position[i] - candidate.position[i]).powi(2))
                            .sum::<f64>()
                            .sqrt();
                        ensure!(
                            distance <= 200.0,
                            "independent broadcast position differs by {distance:.1} m"
                        );
                        Ok(())
                    })();
                    match screen {
                        Ok(()) => {
                            screened_ids.insert(id);
                        }
                        Err(e) => report.removals.push(format!("{id}: {e:#}")),
                    }
                }
                for block in 0..system.subblocks() {
                    let mut records = Vec::new();
                    for &id in &screened_ids {
                        let result = if system == System::Glonass {
                            glonass(inputs, id, time, block, orbit, clock, at)
                                .map(|b| (b, 0.0, 0.0))
                        } else {
                            kepler(inputs, system, id, time as f64, orbit, fit_limit, at)
                        };
                        match result {
                            Ok((bytes, rms, quantized)) => {
                                records.push(bytes);
                                report.fit_rms_max_m = report.fit_rms_max_m.max(rms);
                                report.quantized_rms_max_m =
                                    report.quantized_rms_max_m.max(quantized);
                            }
                            Err(e) => report.removals.push(format!("{id} block {block}: {e:#}")),
                        }
                    }
                    report.counts.push(records.len());
                    blocks.push(records);
                }
                Ok((
                    Epoch {
                        time: u32::try_from(time)?,
                        blocks,
                    },
                    report,
                    screened_ids.len(),
                ))
            })
            .collect();
        let mut epochs = Vec::with_capacity(results.len());
        for result in results {
            let (epoch, report, screened) = result?;
            epochs.push(epoch);
            product.epochs.push(report);
            product.screened += screened;
        }
        product
            .files
            .insert(name, record::container(system, &epochs)?);
    }
    Ok(product)
}
