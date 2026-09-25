use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::PathBuf};
use weinav_forge::{extra, policy::System, record, rtcm, seed::Seed};

fn fixture(name: &str) -> Result<Vec<u8>> {
    let root = std::env::var_os("WEINAV_FIXTURES")
        .context("set WEINAV_FIXTURES to the extracted archive fixtures directory")?;
    Ok(fs::read(PathBuf::from(root).join(name))?)
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
