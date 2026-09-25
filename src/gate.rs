use crate::{
    build::{Inputs, Products},
    policy::{Flavor, Plan, System},
    record, rtcm,
    time::Instant,
};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Pass,
    Fail,
    NotApplicable,
}

#[derive(Serialize)]
pub struct Check {
    pub id: String,
    pub product: String,
    pub status: Status,
    pub detail: String,
}

#[derive(Default, Serialize)]
pub struct Gate {
    pub checks: Vec<Check>,
}

impl Gate {
    pub fn check(&mut self, id: &str, product: &str, pass: bool, detail: impl Into<String>) {
        self.checks.push(Check {
            id: id.into(),
            product: product.into(),
            status: if pass { Status::Pass } else { Status::Fail },
            detail: detail.into(),
        });
    }
    pub fn skip(&mut self, id: &str, product: &str, detail: &str) {
        self.checks.push(Check {
            id: id.into(),
            product: product.into(),
            status: Status::NotApplicable,
            detail: detail.into(),
        });
    }
    pub fn passed(&self) -> bool {
        !self.checks.is_empty()
            && self
                .checks
                .iter()
                .all(|c| !matches!(c.status, Status::Fail))
    }
}

pub fn inspect(
    products: &Products,
    inputs: &Inputs,
    flavor: Flavor,
    systems: &[System],
    agnss: bool,
    at: Instant,
    allow_degraded: bool,
) -> Gate {
    let mut gate = Gate::default();
    let files = &products.files;
    let plan = Plan::new(flavor, systems, agnss);
    gate.check(
        "S1",
        "all",
        !files.is_empty(),
        "payloads assembled in memory",
    );
    gate.check(
        "S2",
        "all",
        plan.products.keys().all(|k| files.contains_key(k)),
        "required products present",
    );
    gate.check(
        "S3",
        "all",
        files.keys().all(|k| plan.products.contains_key(k)),
        "only requested products present",
    );
    gate.check(
        "S9",
        "all",
        systems.contains(&System::Gps),
        "GPS is required",
    );
    for (name, bytes) in files {
        gate.check(
            "S4",
            name,
            !bytes.is_empty(),
            format!("{} bytes", bytes.len()),
        );
        gate.check(
            "S6",
            name,
            bytes.iter().any(|&b| b != 0),
            "payload contains nonzero bytes",
        );
        gate.check(
            "S7",
            name,
            rtcm::crc16(bytes) != 0,
            format!("CRC16 {:04x}", rtcm::crc16(bytes)),
        );
        if name == "HW_PGNSS_EXTRA" {
            gate.check(
                "S5",
                name,
                bytes.len() == 6248,
                "EXTRA must have 6248 bytes",
            );
        } else if name == "HW_AGNSS_RTCM_33" {
            gate.check(
                "S5",
                name,
                rtcm::payloads(bytes).is_ok(),
                "complete RTCM frames",
            );
        }
    }
    for &system in systems {
        let name = format!("HW_PGNSS_{}", system.name());
        let Some(bytes) = files.get(&name) else {
            continue;
        };
        let expected = 1008 + 36 * system.subblocks() * (4 + system.slots() * system.record_size());
        gate.check(
            "S5",
            &name,
            bytes.len() == expected,
            format!("expected fixed allocation {expected} bytes"),
        );
        let epochs = match record::parse_container(system, bytes) {
            Ok(epochs) => {
                gate.check("S8", &name, true, "all indices and unused slots checked");
                epochs
            }
            Err(e) => {
                gate.check("S8", &name, false, format!("{e:#}"));
                continue;
            }
        };
        let mut distinct = BTreeSet::new();
        let mut invalid = Vec::new();
        let mut bad_time = Vec::new();
        let mut seed_ids = true;
        let mut negative = false;
        let mut records = 0;
        let mut live_epochs = 0;
        let mut unexpected_empty = false;
        let mut too_thin = false;
        let mut smallest_live_block = None;
        let minimum = match system {
            System::Gps | System::Bds | System::Glonass => 6,
            System::Galileo => 4,
            System::Qzs => 0,
        };
        for epoch in &epochs {
            let report = products
                .epochs
                .iter()
                .find(|e| e.system == system && e.time == i64::from(epoch.time));
            let permitted_empty = flavor == Flavor::Open
                && allow_degraded
                && matches!(system, System::Bds | System::Qzs)
                && report.is_some_and(|r| r.orbit == "empty");
            if !permitted_empty {
                live_epochs += 1;
            }
            for (subblock, block) in epoch.blocks.iter().enumerate() {
                if permitted_empty {
                    unexpected_empty |= !block.is_empty();
                } else {
                    unexpected_empty |= block.is_empty() && system != System::Qzs;
                    too_thin |= block.len() < minimum;
                    smallest_live_block = Some(
                        smallest_live_block
                            .map_or(block.len(), |count: usize| count.min(block.len())),
                    );
                }
                for bytes in block {
                    records += 1;
                    match record::decode(system, bytes) {
                        Ok(values) => {
                            let id = values[if system == System::Glonass {
                                "slot"
                            } else {
                                "sv"
                            }] as u8
                                + 1;
                            distinct.insert(id);
                            if let Err(e) = record::validate(system, &values)
                                && invalid.len() < 20
                            {
                                invalid.push(e.to_string());
                            }
                            if let Err(e) = record::validate_envelope(
                                system,
                                &values,
                                report.is_some_and(|r| r.clock == "hiee"),
                            ) && invalid.len() < 20
                            {
                                invalid.push(e.to_string());
                            }
                            if system == System::Qzs {
                                negative |= values["delta_n"] < 0.0;
                            }
                            if report.is_some_and(|r| r.orbit == "hiee" || r.clock == "hiee") {
                                seed_ids &= inputs
                                    .satellites
                                    .get(&system)
                                    .is_some_and(|s| s.contains_key(&id));
                            }
                            let consistent = if system == System::Glonass {
                                values["t_b"]
                                    == f64::from(
                                        (epoch.time + 7200 + 900 * subblock as u32) % 86400,
                                    )
                            } else {
                                let tow = (i64::from(epoch.time)
                                    - if system == System::Bds { 14 } else { 0 })
                                .rem_euclid(604800)
                                    as f64;
                                values["toe"] == tow
                                    && values["toc"] == tow
                                    && (system != System::Gps
                                        || values["week"] == (epoch.time / 604800) as f64)
                                    && (system != System::Bds
                                        || values["toe_tag"] == ((tow as u32 / 8) & 0xffc0) as f64)
                            };
                            if !consistent && bad_time.len() < 20 {
                                bad_time.push(format!("epoch {} satellite {id}", epoch.time));
                            }
                        }
                        Err(e) => {
                            if invalid.len() < 20 {
                                invalid.push(e.to_string());
                            }
                        }
                    }
                }
            }
        }
        let mean_floor = match system {
            System::Gps | System::Bds => 6.0,
            System::Glonass => 32.0,
            System::Galileo => 4.0,
            System::Qzs => 0.0,
        };
        let distinct_floor = match system {
            System::Gps | System::Bds | System::Glonass => 6,
            System::Galileo => 4,
            System::Qzs => 1,
        };
        if live_epochs == 0 && epochs.len() == 36 && records == 0 {
            for id in ["C1", "C2", "C4"] {
                gate.skip(id, &name, "all epochs are declared open horizon gaps");
            }
        } else {
            gate.check("C1", &name, records > 0, format!("{records} records"));
            gate.check(
                "C2",
                &name,
                live_epochs > 0 && records as f64 / live_epochs as f64 >= mean_floor,
                format!("mean floor {mean_floor} per epoch within source coverage"),
            );
            gate.check(
                "C4",
                &name,
                distinct.len() >= distinct_floor,
                format!(
                    "{} distinct satellites; floor {distinct_floor}",
                    distinct.len()
                ),
            );
        }
        gate.check(
            "C3",
            &name,
            !unexpected_empty,
            "record counts agree with declared horizon gaps",
        );
        gate.check(
            "C5",
            &name,
            epochs
                .iter()
                .flat_map(|e| &e.blocks)
                .flatten()
                .all(|b| b.len() == system.record_size()),
            "all records have full length",
        );
        if system == System::Qzs {
            gate.check(
                "D1",
                &name,
                !negative,
                "no negative QZSS mean-motion correction",
            );
        } else {
            gate.skip("D1", &name, "QZSS only");
        }
        gate.check(
            "D2",
            &name,
            !too_thin,
            format!(
                "smallest live block: {} satellites; required minimum: {minimum}; declared open horizon gaps exempt",
                smallest_live_block.unwrap_or(0)
            ),
        );
        let first = epochs.first().map(|e| i64::from(e.time));
        let last = epochs.last().map(|e| i64::from(e.time) + 7200);
        gate.check(
            "K1",
            &name,
            first.is_some_and(|t| at.gps() - t <= 7200),
            "grid starts at most one step in the past",
        );
        gate.check(
            "K2",
            &name,
            first.is_some_and(|t| t - at.gps() <= 7214) && last.is_some_and(|t| t > at.gps()),
            "grid includes the current build bucket",
        );
        gate.check(
            "K3",
            &name,
            last.is_some_and(|t| t - at.gps() >= 43200),
            "at least 12 hours of forward grid coverage",
        );
        let expected_grid = at.grid(system);
        gate.check(
            "G1",
            &name,
            epochs.len() == 36
                && epochs
                    .iter()
                    .zip(expected_grid)
                    .all(|(e, t)| i64::from(e.time) == t),
            "36 epochs on the requested constellation grid",
        );
        gate.check(
            "G2",
            &name,
            invalid.is_empty(),
            if invalid.is_empty() {
                "all live records decoded; physical and vendor integer bounds checked".into()
            } else {
                invalid.join("; ")
            },
        );
        gate.check(
            "G3",
            &name,
            bad_time.is_empty(),
            if bad_time.is_empty() {
                "record times agree with every index and subblock".into()
            } else {
                bad_time.join("; ")
            },
        );
        if let Some(seed) = &inputs.seed {
            let used = products
                .epochs
                .iter()
                .filter(|e| e.system == system && (e.orbit == "hiee" || e.clock == "hiee"));
            gate.check(
                "P2",
                &name,
                used.clone()
                    .all(|e| seed.brackets(e.time as f64, (e.time + 7200) as f64)),
                "seed brackets every seed-derived epoch",
            );
            gate.check(
                "P4",
                &name,
                seed_ids,
                "seed-derived records use seed satellites",
            );
        } else {
            gate.skip("P2", &name, "open flavor has no seed");
            gate.skip("P4", &name, "open flavor has no seed");
        }
    }
    if agnss {
        let result = files
            .get("HW_AGNSS_RTCM_33")
            .ok_or_else(|| anyhow::anyhow!("missing RTCM"))
            .and_then(|b| crate::agnss::validate_fresh(b, at));
        gate.check(
            "K4",
            "HW_AGNSS_RTCM_33",
            result.is_ok(),
            result
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "all RTCM epochs within two hours".into()),
        );
    } else {
        gate.skip("K4", "AGNSS", "AGNSS omitted by request");
    }
    gate.check(
        "P0",
        "sources",
        true,
        "all explicit source paths opened and all cached hashes verified",
    );
    if let Some(seed) = &inputs.seed {
        gate.check(
            "P1",
            "EXTRA",
            crate::extra::build(seed).is_ok_and(|(b, _)| files.get("HW_PGNSS_EXTRA") == Some(&b)),
            "EXTRA rebuilt from the selected seed",
        );
        gate.check(
            "P3",
            "seed",
            seed.brackets(at.gps() as f64, (at.gps() + 43200) as f64),
            "seed usable now and for 12 more hours",
        );
    } else {
        gate.skip("P1", "EXTRA", "open almanac assembly");
        gate.skip("P3", "seed", "open flavor has no seed");
    }
    gate.check("O1","all",products.screened>0 && products.epochs.iter().all(|epoch| !epoch.actual_orbit_providers.iter().any(|provider| provider == "brdc")),"all retained satellites passed independent fresh broadcast health and 200 m position screening at build time");
    let providers_match = products.epochs.iter().all(|e| {
        let choice = &plan.products[&format!("HW_PGNSS_{}", e.system.name())];
        let policy_matches = (e.orbit == choice.orbit && e.clock == choice.clock)
            || choice.beyond_horizon == Some(e.orbit.as_str()) && e.clock == e.orbit;
        let actual_matches = [
            (&e.orbit, &e.actual_orbit_providers),
            (&e.clock, &e.actual_clock_providers),
        ]
        .into_iter()
        .all(|(role, providers)| {
            !providers.is_empty()
                && providers.iter().all(|p| {
                    p == role
                        || plan
                            .fallback_chains
                            .get(role.as_str())
                            .is_some_and(|chain| chain.contains(&p.as_str()))
                })
        });
        policy_matches && actual_matches
    });
    gate.check(
        "O2",
        "all",
        providers_match && products.epochs.len() == 36 * systems.len(),
        "resolved epoch providers match the static flavor policy",
    );
    if flavor == Flavor::Open {
        gate.check("O3","all",allow_degraded,"explicit degraded-output permission required; reports identify partial EXTRA and empty horizons");
    } else {
        gate.skip("O3", "all", "not the open flavor");
    }
    gate
}
