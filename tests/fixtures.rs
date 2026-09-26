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
    let systems = [System::Gps, System::Galileo, System::Bds, System::Qzs];
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
    for system in systems {
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
