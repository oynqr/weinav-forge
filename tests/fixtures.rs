use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::PathBuf};
use weinav_forge::{extra, orbit, policy::System, record, rtcm, seed::Seed};

#[test]
#[ignore = "requires the author's reviewed seed and missing-record CSV"]
fn reviewed_missing_seed_records_fit_freely_inside_the_envelope() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("WEINAV_REVIEW_FIXTURES")
            .context("set WEINAV_REVIEW_FIXTURES to the gen3-evidence directory")?,
    );
    let seed = Seed::parse(&fs::read(root.join("HiEE_V2.dat"))?)?;
    let csv = fs::read_to_string(root.join("missing_vs_huawei.csv"))?;
    let mut checked = 0;
    for row in csv.lines().skip(1) {
        let fields: Vec<_> = row.split(',').collect();
        if fields[6] != "dropped healthy record (fix)" {
            continue;
        }
        let system = System::from_code(fields[2].chars().next().unwrap()).unwrap();
        let id: u8 = fields[2][1..].parse()?;
        let time: f64 = fields[4].parse()?;
        let nav = seed.nav(system)?;
        let sat = &nav[&id];
        let state = sat.arc_at(time).unwrap().evaluate(time)?;
        let toe = (time - if system == System::Bds { 14.0 } else { 0.0 }).rem_euclid(604800.0);
        let samples = (-24..=24)
            .map(|i| {
                let dt = f64::from(i) * 300.0;
                Ok((
                    dt,
                    sat.arc_at(time + dt).unwrap().evaluate(time + dt)?.position,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let fitted = orbit::fit(
            &samples,
            toe,
            system,
            orbit::initial(&state, toe, system)?,
            false,
        )?;
        assert!(
            fitted.sigma <= 1.0,
            "{} e{}: {}",
            fields[2],
            fields[3],
            fitted.sigma
        );
        let values = orbit::record_values(
            system,
            id,
            time,
            &fitted.parameters,
            [state.clock, state.drift, 0.0],
            [sat.tgd, 0.0],
        );
        let bytes = record::encode(system, &values)?;
        let values = record::decode(system, &bytes)?;
        record::validate(system, &values)?;
        record::validate_envelope(system, &values)
            .with_context(|| format!("{} e{}", fields[2], fields[3]))?;
        checked += 1;
    }
    assert_eq!(checked, 47);
    Ok(())
}

fn fixture(name: &str) -> Result<Vec<u8>> {
    let root = std::env::var_os("WEINAV_FIXTURES")
        .context("set WEINAV_FIXTURES to the extracted archive fixtures directory")?;
    Ok(fs::read(PathBuf::from(root).join(name))?)
}

type Cells = BTreeMap<(u8, usize), BTreeMap<String, f64>>;

fn kepler_cells(system: System, container: &[u8]) -> Result<Cells> {
    let mut cells = BTreeMap::new();
    for (index, epoch) in record::parse_container(system, container)?
        .iter()
        .enumerate()
    {
        for bytes in &epoch.blocks[0] {
            let values = record::decode(system, bytes)?;
            record::validate(system, &values)?;
            record::validate_envelope(system, &values)?;
            cells.insert((values["sv"] as u8 + 1, index), values);
        }
    }
    Ok(cells)
}

type GlonassRecords = BTreeMap<(u8, usize, usize), Vec<u8>>;

fn glonass_records(container: &[u8]) -> Result<GlonassRecords> {
    let mut records = BTreeMap::new();
    for (index, epoch) in record::parse_container(System::Glonass, container)?
        .iter()
        .enumerate()
    {
        for (block, bytes) in epoch.blocks.iter().enumerate() {
            for bytes in bytes {
                let slot = record::decode(System::Glonass, bytes)?["slot"] as u8 + 1;
                records.insert((slot, index, block), bytes.clone());
            }
        }
    }
    Ok(records)
}

fn gps_node_drift_error(container: &[u8]) -> Result<f64> {
    use std::f64::consts::PI;
    const GM: f64 = 3.986005e14;
    const EARTH_ROTATION: f64 = 7.2921151467e-5;
    const WEEK: f64 = 604800.0;
    const TOE_STEP: f64 = 16.0;
    const NOMINAL_AXIS_KM: f64 = 26560.0;
    const NOMINAL_INCLINATION_DEG: f64 = 55.0;
    let wrap = |semicircles: f64| (semicircles + 1.0).rem_euclid(2.0) - 1.0;
    let mut epochs = Vec::new();
    for epoch in record::parse_container(System::Gps, container)? {
        let mut satellites = BTreeMap::new();
        for bytes in &epoch.blocks[0] {
            let values = record::decode(System::Gps, bytes)?;
            satellites.insert(values["sv"] as u8, values);
        }
        epochs.push((f64::from(epoch.time), satellites));
    }
    let (first, last) = (&epochs[0].1, &epochs[epochs.len() - 1].1);
    let prns: Vec<_> = first.keys().filter(|p| last.contains_key(p)).collect();
    anyhow::ensure!(
        prns.len() >= 4,
        "only {} satellites in every epoch",
        prns.len()
    );
    let (mut advance_error, mut advance_bound, mut node_error) = (0.0_f64, 0.0_f64, 0.0_f64);
    let (mut axis_change, mut inclination_change) = (0.0_f64, 0.0_f64);
    for prn in prns {
        for pair in epochs.windows(2) {
            let ((ta, ra), (tb, rb)) = (&pair[0], &pair[1]);
            let (Some(a), Some(b)) = (ra.get(prn), rb.get(prn)) else {
                continue;
            };
            let axis = a["sqrt_a"].powi(2);
            anyhow::ensure!(
                (axis / 1000.0 - NOMINAL_AXIS_KM).abs() <= 0.02 * NOMINAL_AXIS_KM,
                "G{prn} semi-major axis"
            );
            anyhow::ensure!(
                ((a["i0"] * 180.0).abs() - NOMINAL_INCLINATION_DEG).abs() <= 5.0,
                "G{prn} inclination"
            );
            anyhow::ensure!((0.0..0.05).contains(&a["ecc"]), "G{prn} eccentricity");
            let motion = (GM / axis.powi(3)).sqrt();
            let dt = tb - ta;
            let expected = (motion * dt + PI).rem_euclid(2.0 * PI) - PI;
            advance_bound = advance_bound.max(motion * TOE_STEP);
            let advance = wrap((b["m0"] + b["omega"]) - (a["m0"] + a["omega"])) * PI;
            advance_error = advance_error.max(wrap((advance - expected) / PI).abs() * PI);
            let node = wrap(b["omega0"] - a["omega0"]) * PI;
            let expected_node =
                a["omegadot"] * PI * dt - EARTH_ROTATION * WEEK * (b["week"] - a["week"]);
            node_error =
                node_error.max(((node - expected_node + PI).rem_euclid(2.0 * PI) - PI).abs());
            axis_change = axis_change.max((b["sqrt_a"] - a["sqrt_a"]).abs() / a["sqrt_a"]);
            inclination_change = inclination_change.max((b["i0"] - a["i0"]).abs() / a["i0"].abs());
        }
    }
    anyhow::ensure!(
        advance_error <= advance_bound,
        "argument of latitude error {advance_error:e} rad"
    );
    anyhow::ensure!(node_error <= 1e-5, "node drift error {node_error:e} rad");
    anyhow::ensure!(
        axis_change <= 1e-4,
        "semi-major axis change {axis_change:e}"
    );
    anyhow::ensure!(
        inclination_change <= 1e-3,
        "inclination change {inclination_change:e}"
    );
    Ok(node_error)
}

fn geo(values: &BTreeMap<String, f64>) -> bool {
    orbit::is_geo(System::Bds, &orbit::parameters(values).unwrap())
}

fn largest_separation(
    system: System,
    ours: &BTreeMap<String, f64>,
    huawei: &BTreeMap<String, f64>,
) -> Result<f64> {
    let geo = system == System::Bds && geo(ours);
    let (p, q) = (orbit::parameters(ours)?, orbit::parameters(huawei)?);
    let mut largest: f64 = 0.0;
    for step in -24..=24 {
        let dt = f64::from(step) * 300.0;
        let a = orbit::position(&p, dt, ours["toe"], system, geo)?;
        let b = orbit::position(&q, dt, huawei["toe"], system, geo)?;
        largest = largest.max((0..3).map(|i| (a[i] - b[i]).powi(2)).sum::<f64>().sqrt());
    }
    Ok(largest)
}

#[test]
#[ignore = "requires the author's reviewed seed, broadcast snapshot and Huawei output"]
fn reviewed_kepler_records_match_huawei() -> Result<()> {
    use weinav_forge::{
        build,
        policy::{Flavor, Role},
        time::Instant,
    };
    let root = PathBuf::from(
        std::env::var_os("WEINAV_REVIEW_FIXTURES")
            .context("set WEINAV_REVIEW_FIXTURES to the gen3-evidence directory")?,
    );
    let local = BTreeMap::from([
        (Role::Seed, vec![root.join("HiEE_V2.dat")]),
        (
            Role::Broadcast,
            vec![root.join("agent_satdrops/brdc/BRDC00WRD_S_20262690000_01D_MN.rnx.gz")],
        ),
    ]);
    let systems = System::ALL;
    let cache = tempfile::tempdir()?;
    let inputs = build::Inputs::load(cache.path(), None, Flavor::Huawei, &systems, false, &local)?;
    let products = build::assemble(
        &inputs,
        Flavor::Huawei,
        &systems,
        false,
        Instant::parse("2026-09-26T05:43:07Z")?,
        1.0,
    )?;
    let ours = glonass_records(&products.files["HW_PGNSS_GLONASS"])?;
    let mut huawei = glonass_records(&fs::read(root.join("oracle/HW_PGNSS_GLONASS"))?)?;
    let unhealthy_in_broadcast = [13, 20];
    huawei.retain(|(slot, _, _), _| !unhealthy_in_broadcast.contains(slot));
    assert!(
        ours == huawei,
        "{} of {} GLONASS records",
        ours.len(),
        huawei.len()
    );
    let huawei_gps = fs::read(root.join("oracle/HW_PGNSS_GPS"))?;
    gps_node_drift_error(&huawei_gps)?;
    let node_error = gps_node_drift_error(&products.files["HW_PGNSS_GPS"])?;
    assert!(node_error <= 1e-5, "{node_error:e}");
    for system in [System::Gps, System::Galileo, System::Bds, System::Qzs] {
        let name = format!("HW_PGNSS_{}", system.name());
        let ours = kepler_cells(system, &products.files[&name])?;
        let huawei = kepler_cells(system, &fs::read(root.join("oracle").join(&name))?)?;
        for (cell, values) in &ours {
            let expected = huawei
                .get(cell)
                .map_or(Vec::new(), |v| record::fields_on_limit(system, v));
            assert_eq!(
                record::fields_on_limit(system, values),
                expected,
                "{system:?} {cell:?}"
            );
        }
        let keys = |cells: &Cells, only_geo: bool| -> Vec<(u8, usize)> {
            cells
                .iter()
                .filter(|(_, v)| !only_geo || geo(v))
                .map(|(k, _)| *k)
                .collect()
        };
        assert_eq!(keys(&ours, false), keys(&huawei, false), "{system:?}");
        match system {
            System::Gps => {
                let mut compared = 0;
                for (cell, values) in &ours {
                    if let Some(theirs) = huawei.get(cell) {
                        let separation = largest_separation(system, values, theirs)?;
                        assert!(
                            separation <= 0.15,
                            "G{:02} e{}: {separation:.3} m",
                            cell.0,
                            cell.1
                        );
                        compared += 1;
                    }
                }
                assert!(compared >= 1000, "{compared}");
            }
            System::Bds => assert_eq!(keys(&huawei, true).len(), 139),
            System::Qzs => assert_eq!(huawei.len(), 47),
            _ => {}
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires the author's reviewed seed, broadcast snapshot and Huawei output"]
fn reviewed_satellites_flagged_at_build_time_match_huawei() -> Result<()> {
    use weinav_forge::{
        build,
        policy::{Flavor, Role},
        time::Instant,
    };
    let root = PathBuf::from(
        std::env::var_os("WEINAV_REVIEW_FIXTURES")
            .context("set WEINAV_REVIEW_FIXTURES to the gen3-evidence directory")?,
    );
    let run = root.join("agent_flagged_p18h");
    let local = BTreeMap::from([
        (Role::Seed, vec![root.join("HiEE_V2.dat")]),
        (Role::Broadcast, vec![run.join("brdc_until_at.rnx.gz")]),
    ]);
    let cache = tempfile::tempdir()?;
    let inputs = build::Inputs::load(
        cache.path(),
        None,
        Flavor::Huawei,
        &System::ALL,
        false,
        &local,
    )?;
    let at = Instant::parse("2026-09-26T14:59:42Z")?;
    let flagged = |system: System, id: u8| {
        inputs.satellites[&system][&id]
            .arc_at(at.gps() as f64)
            .is_some_and(|arc| arc.flag != 0)
    };
    assert!(flagged(System::Glonass, 15) && flagged(System::Bds, 6) && flagged(System::Bds, 9));
    let products = build::assemble(&inputs, Flavor::Huawei, &System::ALL, false, at, 1.0)?;
    for system in [System::Gps, System::Galileo, System::Bds, System::Qzs] {
        let name = format!("HW_PGNSS_{}", system.name());
        let ours = kepler_cells(system, &products.files[&name])?;
        let huawei = kepler_cells(system, &fs::read(run.join("oracle").join(&name))?)?;
        assert!(ours.keys().eq(huawei.keys()), "{system:?}");
    }
    let ours = glonass_records(&products.files["HW_PGNSS_GLONASS"])?;
    let mut huawei = glonass_records(&fs::read(run.join("oracle/HW_PGNSS_GLONASS"))?)?;
    let unhealthy_in_broadcast = 20;
    huawei.retain(|(slot, _, _), _| *slot != unhealthy_in_broadcast);
    assert!(ours.keys().any(|(slot, _, _)| *slot == 15));
    assert!(
        ours == huawei,
        "{} of {} GLONASS records",
        ours.len(),
        huawei.len()
    );
    Ok(())
}

#[test]
#[ignore = "requires the author's reviewed Huawei AGNSS"]
fn reviewed_huawei_agnss_is_not_refused_for_its_dates() -> Result<()> {
    use weinav_forge::{agnss, time::Instant};
    let root = PathBuf::from(
        std::env::var_os("WEINAV_REVIEW_FIXTURES")
            .context("set WEINAV_REVIEW_FIXTURES to the gen3-evidence directory")?,
    );
    let bytes =
        fs::read(root.join("x/huawei/659dea01-0fbb-41e4-8876-e5eb5eeecc95/HW_AGNSS_RTCM_33"))?;
    let at = Instant::parse("2026-09-26T05:43:07Z")?;
    type FieldChange = (u16, &'static str, fn(f64) -> f64);
    let edit = |number: u16, field: &str, change: fn(f64) -> f64| -> Result<Vec<u8>> {
        let mut edited = Vec::new();
        for payload in rtcm::payloads(&bytes)? {
            let mut message = rtcm::Message::decode(payload)?;
            if message.number == number {
                let value = message.values.get_mut(field).context("edited field")?;
                *value = change(*value);
            }
            edited.extend(rtcm::frame(&message.encode()?)?);
        }
        Ok(edited)
    };
    assert!(agnss::validate_fresh(&bytes, at, true)?.is_empty());
    let observed: [FieldChange; 3] = [
        (1046, "week", |v| v + 1.0),
        (1020, "nt", |v| v + 1.0),
        (1020, "nt", |v| v - 8.0),
    ];
    for (number, field, change) in observed {
        let edited = edit(number, field, change)?;
        let notes = agnss::validate_fresh(&edited, at, false)?;
        assert_eq!(notes.len(), 1, "{number} {field}: {notes:?}");
        assert!(
            agnss::validate_fresh(&edited, at, true).is_err(),
            "{number} {field}"
        );
    }
    let refused: [FieldChange; 10] = [
        (1046, "week", |v| v + 2.0),
        (1046, "week", |v| v - 1.0),
        (1020, "nt", |v| v + 2.0),
        (1020, "nt", |v| v - 9.0),
        (1020, "n4", |v| v - 1.0),
        (1046, "toe", |v| v - 5.0 * 3600.0),
        (1020, "tb", |v| v - 2700.0),
        (1020, "tb", |_| 127.0 * 900.0),
        (1019, "week", |v| v - 1.0),
        (1042, "week", |v| v - 1.0),
    ];
    for (number, field, change) in refused {
        assert!(
            agnss::validate_fresh(&edit(number, field, change)?, at, false).is_err(),
            "{number} {field}"
        );
    }
    let mut without_gps = Vec::new();
    for payload in rtcm::payloads(&bytes)? {
        if rtcm::Message::decode(payload)?.number != 1019 {
            without_gps.extend(rtcm::frame(payload)?);
        }
    }
    assert!(agnss::validate_fresh(&without_gps, at, false).is_err());
    Ok(())
}

#[test]
#[ignore = "requires the author's reviewed seed and broadcast snapshot"]
fn reviewed_flagged_arc_at_build_time_removes_only_nearby_records() -> Result<()> {
    use weinav_forge::{
        build,
        policy::{Flavor, Role},
        time::Instant,
    };
    let root = PathBuf::from(
        std::env::var_os("WEINAV_REVIEW_FIXTURES")
            .context("set WEINAV_REVIEW_FIXTURES to the gen3-evidence directory")?,
    );
    let local = BTreeMap::from([
        (Role::Seed, vec![root.join("HiEE_V2.dat")]),
        (
            Role::Broadcast,
            vec![root.join("agent_satdrops/brdc/BRDC00WRD_S_20262690000_01D_MN.rnx.gz")],
        ),
    ]);
    let cache = tempfile::tempdir()?;
    let mut inputs = build::Inputs::load(
        cache.path(),
        None,
        Flavor::Huawei,
        &[System::Bds],
        false,
        &local,
    )?;
    let at = Instant::parse("2026-09-26T05:43:07Z")?;
    let now = at.gps() as f64;
    let arc = inputs
        .satellites
        .get_mut(&System::Bds)
        .and_then(|s| s.get_mut(&7))
        .and_then(|s| s.arcs.iter_mut().find(|a| a.start <= now && now < a.end))
        .context("C07 arc at the build time")?;
    arc.flag = 1;
    let (start, end) = (arc.start, arc.end);
    let products = build::assemble(&inputs, Flavor::Huawei, &[System::Bds], false, at, 1.0)?;
    let mut near = 0;
    let mut far = 0;
    for epoch in record::parse_container(System::Bds, &products.files["HW_PGNSS_BDS"])? {
        let time = f64::from(epoch.time);
        let shipped = epoch.blocks[0]
            .iter()
            .any(|bytes| record::decode(System::Bds, bytes).is_ok_and(|v| v["sv"] == 6.0));
        if time + 7200.0 >= start && time - 7200.0 < end {
            assert!(!shipped, "C07 shipped at {time} near the flagged arc");
            near += 1;
        } else if shipped {
            far += 1;
        }
    }
    assert!(near > 0 && far >= 20, "near {near}, far {far}");
    Ok(())
}

#[test]
#[ignore = "requires the author's reviewed broadcast snapshot"]
fn reviewed_open_agnss_ages_out_with_the_glonass_broadcast() -> Result<()> {
    use weinav_forge::{agnss, rinex::Broadcast, time::Instant};
    let root = PathBuf::from(
        std::env::var_os("WEINAV_REVIEW_FIXTURES")
            .context("set WEINAV_REVIEW_FIXTURES to the gen3-evidence directory")?,
    );
    let mut broadcast = Broadcast::default();
    broadcast.add(&fs::read(
        root.join("agent_satdrops/brdc/BRDC00WRD_S_20262690000_01D_MN.rnx.gz"),
    )?)?;
    let at = Instant::parse("2026-09-26T05:43:07Z")?;
    let (bytes, _, _) = agnss::build(&broadcast, &System::ALL, at)?;
    agnss::validate_fresh(&bytes, at, true)?;
    agnss::validate_fresh(&bytes, Instant::parse("2026-09-26T06:14:00Z")?, true)?;
    let late = Instant::parse("2026-09-26T06:16:00Z")?;
    let error = agnss::validate_fresh(&bytes, late, true).unwrap_err();
    assert!(error.to_string().contains("1020"), "{error:#}");
    Ok(())
}

#[test]
#[ignore = "requires the author's reviewed broadcast snapshot"]
fn reviewed_open_agnss_omits_only_stale_glonass() -> Result<()> {
    use weinav_forge::{agnss, rinex::Broadcast, time::Instant};
    let root = PathBuf::from(
        std::env::var_os("WEINAV_REVIEW_FIXTURES")
            .context("set WEINAV_REVIEW_FIXTURES to the gen3-evidence directory")?,
    );
    let mut broadcast = Broadcast::default();
    broadcast.add(&fs::read(
        root.join("agent_satdrops/brdc/BRDC00WRD_S_20262690000_01D_MN.rnx.gz"),
    )?)?;
    let count = |bytes: &[u8], number: u16| -> Result<usize> {
        let mut count = 0;
        for payload in rtcm::payloads(bytes)? {
            count += usize::from(rtcm::Message::decode(payload)?.number == number);
        }
        Ok(count)
    };
    let fresh = Instant::parse("2026-09-26T06:14:00Z")?;
    let (bytes, notes, omitted) = agnss::build(&broadcast, &System::ALL, fresh)?;
    assert!(count(&bytes, 1020)? > 0);
    assert_eq!(omitted, 0);
    assert!(!notes.iter().any(|n| n.contains("omitted")), "{notes:?}");
    let late = Instant::parse("2026-09-26T06:16:00Z")?;
    let (bytes, notes, omitted) = agnss::build(&broadcast, &System::ALL, late)?;
    agnss::validate_fresh(&bytes, late, true)?;
    assert_eq!(count(&bytes, 1020)?, 0);
    assert!(omitted > 0);
    assert!(count(&bytes, 1019)? > 0 && count(&bytes, 1042)? > 0 && count(&bytes, 1046)? > 0);
    assert!(
        notes
            .iter()
            .any(|n| n.contains("GLONASS") && n.contains("omitted")),
        "{notes:?}"
    );
    Ok(())
}

#[test]
#[ignore = "requires local Huawei fixtures"]
fn seed_and_extra_match_captured_bytes() -> Result<()> {
    let seed = Seed::parse(&fixture("HiEE_V2.expired-20260909.dat.gz")?)?;
    assert_eq!((seed.start, seed.end), (1_472_353_200, 1_472_958_000));
    assert!(!seed.brackets(seed.end as f64, seed.end as f64 + 1.0));
    let (extra, notes) = extra::build(&seed)?;
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        format!("{:x}", Sha256::digest(&extra)),
        "f2df9226c8265d76e2685c2388a9b4e7c859c1c830ad264db84e0fe624d0bdc8"
    );
    let other_seed_extra = fixture("HW_PGNSS_EXTRA.sample")?;
    assert_ne!(&extra[0x1858..0x1860], &other_seed_extra[0x1858..0x1860]);
    let mut padded = 0;
    for system in System::ALL {
        let nav = seed.nav(system)?;
        for sat in nav.values() {
            assert_eq!(sat.arcs.len(), 14);
            for arc in &sat.arcs {
                padded += usize::from(arc.padded_coefficient);
                let state = arc.evaluate((arc.start + arc.end) / 2.0)?;
                let radius = state.position.iter().map(|x| x * x).sum::<f64>().sqrt();
                assert!((2.0e7..5.0e7).contains(&radius));
            }
        }
    }
    assert_eq!(padded, 1);
    Ok(())
}

#[test]
#[ignore = "requires local Huawei fixtures"]
fn rtcm_roundtrips_every_field_and_frame() -> Result<()> {
    let bytes = fixture("HW_AGNSS_RTCM_33.20260902")?;
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        "538892071e29a42eef7c01e63a1918c7c92c05ea59f355a087a7382a5a7cc6d3"
    );
    let mut histogram = BTreeMap::new();
    let mut encoded = Vec::new();
    for payload in rtcm::payloads(&bytes)? {
        let message = rtcm::Message::decode(payload)?;
        *histogram.entry(message.number).or_insert(0) += 1;
        encoded.extend(rtcm::frame(&message.encode()?)?);
    }
    assert_eq!(
        histogram,
        BTreeMap::from([(1019, 31), (1020, 23), (1042, 31), (1046, 28), (4056, 1)])
    );
    assert_eq!(encoded, bytes);
    let seed = Seed::parse(&fixture("HiEE_V2.expired-20260909.dat.gz")?)?;
    assert_eq!(
        &rtcm::payloads(&bytes)?.last().unwrap()[3..],
        &seed.fields["gpsIon"]
    );
    Ok(())
}

#[test]
#[ignore = "requires local Huawei fixtures"]
fn known_bad_qzss_is_rejected_without_losing_container_geometry() -> Result<()> {
    let bytes = fixture("HW_PGNSS_QZS.sample")?;
    let epochs = record::parse_container(System::Qzs, &bytes)?;
    assert_eq!(epochs.len(), 36);
    assert_eq!(epochs[0].time, 1_471_644_000);
    let mut rejected = 0;
    for (i, epoch) in epochs.iter().enumerate() {
        assert_eq!(epoch.blocks[0].len(), if i < 10 { 2 } else { 0 });
        for record in &epoch.blocks[0] {
            rejected += usize::from(record::decode(System::Qzs, record).is_err());
        }
    }
    assert_eq!(rejected, 10);
    Ok(())
}

#[test]
#[ignore = "requires the external gate's g2_envelopes.json"]
fn envelopes_match_the_gate() -> Result<()> {
    let path = std::env::var_os("WEINAV_GATE_ENVELOPES")
        .context("set WEINAV_GATE_ENVELOPES to the external gate's g2_envelopes.json")?;
    let bytes = fs::read(path)?;
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        record::envelopes::GATE_SHA256
    );
    let gate: serde_json::Value = serde_json::from_slice(&bytes)?;
    for system in System::ALL {
        let fields = gate["constellations"][system.name()]["fields"]
            .as_object()
            .context("gate constellation")?;
        let bounds = |name: &str| {
            let field = &fields[name];
            let scale = field["scale"].as_f64()?;
            record::envelopes::bounds(system)
                .iter()
                .find(|&&(n, _, _)| n == name)
                .map(|&(_, low, high)| ((low / scale).round(), (high / scale).round()))
        };
        let limits = |name: &str, low: &str, high: &str| {
            Some((fields[name][low].as_f64()?, fields[name][high].as_f64()?))
        };
        for name in fields.keys() {
            if let Some(envelope) = limits(name, "env_min", "env_max") {
                assert_eq!(bounds(name), Some(envelope), "{system:?} {name}");
            }
        }
        for &(name, _, _) in record::envelopes::bounds(system) {
            if fields[name]["env_min"].is_null() {
                assert_eq!(
                    bounds(name),
                    limits(name, "store_min", "store_max"),
                    "{system:?} {name}"
                );
            }
        }
    }
    Ok(())
}
