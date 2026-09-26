use anyhow::Result;
use std::collections::BTreeMap;
use weinav_forge::{
    build::{EpochReport, Inputs, Products},
    gate,
    policy::{Flavor, System},
    record::{self, Epoch},
    rinex::Broadcast,
    time::Instant,
};

#[test]
fn a_full_length_zero_census_is_refused() -> Result<()> {
    let at = Instant::parse("2026-09-23T12:00:00Z")?;
    let epochs: Vec<_> = at
        .grid(System::Gps)
        .into_iter()
        .map(|time| Epoch {
            time: time as u32,
            blocks: vec![vec![]],
        })
        .collect();
    let bytes = record::container(System::Gps, &epochs)?;
    assert_eq!(record::parse_container(System::Gps, &bytes)?.len(), 36);
    let inputs = empty_inputs();
    let products = Products {
        files: BTreeMap::from([
            ("HW_PGNSS_GPS".into(), bytes),
            ("HW_PGNSS_EXTRA".into(), vec![1; 6248]),
        ]),
        epochs: vec![],
        notes: vec![],
        screened: 0,
    };
    let gate = gate::inspect(
        &products,
        &inputs,
        Flavor::Open,
        &[System::Gps],
        false,
        at,
        true,
    );
    assert!(!gate.passed());
    for id in ["C1", "C2", "C3", "C4", "D2", "O1", "O2"] {
        assert!(
            gate.checks
                .iter()
                .any(|c| c.id == id && matches!(c.status, gate::Status::Fail)),
            "{id} did not refuse"
        );
    }
    Ok(())
}

#[test]
fn unused_slots_and_truncated_indices_are_rejected() -> Result<()> {
    let mut bytes = record::container(
        System::Gps,
        &[Epoch {
            time: 7200,
            blocks: vec![vec![]],
        }],
    )?;
    bytes[1100] = 1;
    assert!(record::parse_container(System::Gps, &bytes).is_err());
    assert!(record::parse_container(System::Gps, &bytes[..1007]).is_err());
    bytes[1100] = 0;
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(record::parse_container(System::Gps, &bytes).is_err());
    Ok(())
}

fn empty_inputs() -> Inputs {
    Inputs {
        sources: vec![],
        seed: None,
        satellites: BTreeMap::new(),
        acceleration: None,
        broadcast: Broadcast::default(),
        health: Broadcast::default(),
        predictions: BTreeMap::new(),
        antex: None,
        raw: BTreeMap::new(),
    }
}

#[test]
fn live_block_census_allows_screening_but_rejects_sparse_output() -> Result<()> {
    let at = Instant::parse("2026-09-25T19:00:00Z")?;
    for (system, screened_count, minimum) in [
        (System::Gps, 28, 6),
        (System::Glonass, 17, 6),
        (System::Galileo, 15, 4),
        (System::Bds, 27, 6),
    ] {
        for block_count in [screened_count, minimum, minimum - 1, 0] {
            let epochs = at
                .grid(system)
                .into_iter()
                .enumerate()
                .map(|(index, time)| {
                    let blocks = (0..system.subblocks())
                        .map(|block| {
                            let count = if index == 18 && block == 0 {
                                block_count
                            } else {
                                screened_count
                            };
                            (0..count)
                                .map(|id| {
                                    let mut values = record::zero_values(system);
                                    values.insert(
                                        if system == System::Glonass {
                                            "slot"
                                        } else {
                                            "sv"
                                        }
                                        .into(),
                                        f64::from(id),
                                    );
                                    record::encode(system, &values)
                                })
                                .collect::<Result<Vec<_>>>()
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok(Epoch {
                        time: time as u32,
                        blocks,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let name = format!("HW_PGNSS_{}", system.name());
            let products = Products {
                files: BTreeMap::from([(name, record::container(system, &epochs)?)]),
                epochs: vec![],
                notes: vec![],
                screened: 0,
            };
            let gate = gate::inspect(
                &products,
                &empty_inputs(),
                Flavor::Huawei,
                &[system],
                false,
                at,
                false,
            );
            let check = gate.checks.iter().find(|check| check.id == "D2").unwrap();
            assert_eq!(
                matches!(check.status, gate::Status::Pass),
                block_count >= minimum
            );
            assert!(
                check
                    .detail
                    .contains(&format!("smallest live block: {block_count}"))
            );
        }
    }
    Ok(())
}

#[test]
fn declared_whole_grid_gaps_require_open_permission() -> Result<()> {
    let at = Instant::parse("2026-09-23T12:00:00Z")?;
    for system in [System::Bds, System::Qzs] {
        let times = at.grid(system);
        let epochs = times
            .iter()
            .map(|&time| Epoch {
                time: time as u32,
                blocks: vec![vec![]],
            })
            .collect::<Vec<_>>();
        let reports = times
            .into_iter()
            .map(|time| EpochReport {
                system,
                time,
                orbit: "empty".into(),
                clock: "empty".into(),
                actual_orbit_providers: vec!["empty".into()],
                actual_clock_providers: vec!["empty".into()],
                source_hashes: vec![],
                counts: vec![0],
                fit_rms_max_m: 0.0,
                quantized_rms_max_m: 0.0,
                clock_alignment_ns: None,
                removals: vec![],
            })
            .collect();
        let name = format!("HW_PGNSS_{}", system.name());
        let products = Products {
            files: BTreeMap::from([(name.clone(), record::container(system, &epochs)?)]),
            epochs: reports,
            notes: vec![],
            screened: 0,
        };
        for permission in [false, true] {
            let gate = gate::inspect(
                &products,
                &empty_inputs(),
                Flavor::Open,
                &[system],
                false,
                at,
                permission,
            );
            for id in ["C1", "C4"] {
                let check = gate
                    .checks
                    .iter()
                    .find(|check| check.id == id && check.product == name)
                    .unwrap();
                assert_eq!(
                    matches!(check.status, gate::Status::NotApplicable),
                    permission
                );
                assert_eq!(matches!(check.status, gate::Status::Fail), !permission);
            }
        }
    }
    Ok(())
}
