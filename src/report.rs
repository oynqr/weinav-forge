use crate::{
    build::{EpochReport, Inputs, Products},
    cache::Source,
    orbit,
    policy::{Flavor, Plan, Role, System},
    time::Instant,
};
use serde_json::{Map, Value, json};

pub const FORMAT: &str = "gb-gnss-zipbuilder/report/1";

pub fn published_name(source: &Source) -> String {
    let name: String = source
        .url
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    if name.trim_matches('.').is_empty() || matches!(name.as_str(), "report.json" | "ephemeris.zip")
    {
        "broadcast.rnx".into()
    } else {
        name
    }
}

fn provider(name: &str) -> &str {
    if name == "empty" { "none" } else { name }
}

fn constellation(system: System, epochs: &[&EpochReport]) -> Value {
    let mut cells = Map::new();
    let mut start = 0;
    for index in 1..=epochs.len() {
        if index == epochs.len() || epochs[index].orbit != epochs[start].orbit {
            cells.insert(
                format!("epochs {start}-{}", index - 1),
                provider(&epochs[start].orbit).into(),
            );
            start = index;
        }
    }
    if system == System::Glonass
        && !epochs.is_empty()
        && epochs
            .iter()
            .all(|e| e.clock == "hiee" && e.orbit != "hiee")
    {
        cells.insert("clock".into(), "hiee".into());
    }
    Value::Object(cells)
}

pub fn gate_fields(
    inputs: &Inputs,
    products: &Products,
    plan: &Plan,
    at: Instant,
    allow_degraded: bool,
) -> Map<String, Value> {
    let mut built = Map::new();
    let mut systems: Vec<System> = Vec::new();
    for epoch in &products.epochs {
        if !systems.contains(&epoch.system) {
            systems.push(epoch.system);
        }
    }
    for system in systems {
        let epochs: Vec<_> = products
            .epochs
            .iter()
            .filter(|e| e.system == system)
            .collect();
        built.insert(system.name().into(), constellation(system, &epochs));
    }
    if let Some(extra) = plan.products.get("HW_PGNSS_EXTRA") {
        built.insert(
            "EXTRA".into(),
            if extra.orbit == "hiee" {
                "hiee"
            } else {
                "partial"
            }
            .into(),
        );
    }
    if let Some(agnss) = plan.products.get("HW_AGNSS_RTCM_33") {
        built.insert("AGNSS".into(), agnss.orbit.into());
    }
    let seed = inputs
        .seed
        .as_ref()
        .and(inputs.sources.iter().find(|s| s.role == Role::Seed))
        .map(|s| json!({"ver": s.version, "sha256": s.sha256}));
    let health = inputs
        .health_source()
        .map(|s| json!({"source": published_name(s), "sha256": s.sha256}));
    let gravity: Map<String, Value> = System::ALL
        .into_iter()
        .map(|system| (system.name().into(), orbit::record_gravity(system).into()))
        .collect();
    let fields = json!({
        "format": FORMAT,
        "flavor": plan.flavor,
        "built_at_unix": at.0.timestamp(),
        "seed": seed,
        "health": health,
        "plan": built,
        "degraded": {"permitted": plan.flavor == Flavor::Open && allow_degraded},
        "record_gravity_m3_s2": gravity,
    });
    match fields {
        Value::Object(map) => map,
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(system: System, orbit: &str, clock: &str) -> EpochReport {
        EpochReport {
            system,
            time: 0,
            orbit: orbit.into(),
            clock: clock.into(),
            actual_orbit_providers: vec![],
            actual_clock_providers: vec![],
            source_hashes: vec![],
            counts: vec![],
            fit_rms_max_m: 0.0,
            quantized_rms_max_m: 0.0,
            clock_alignment_ns: None,
            removals: vec![],
        }
    }

    fn source(role: Role, url: &str) -> Source {
        Source {
            role,
            provider: "test".into(),
            url: url.into(),
            sha256: format!("{role:?}"),
            bytes: 0,
            fetched_at: String::new(),
            etag: None,
            last_modified: None,
            coverage: vec![],
            version: None,
        }
    }

    #[test]
    fn gate_fields_describe_what_was_built() {
        let mut epochs = Vec::new();
        for index in 0..36 {
            epochs.push(epoch(
                System::Bds,
                if index < 11 { "wum-nrt" } else { "hiee" },
                "unused",
            ));
            epochs.push(epoch(System::Glonass, "code5d", "hiee"));
            epochs.push(epoch(
                System::Qzs,
                if index < 8 { "qzu" } else { "empty" },
                "unused",
            ));
        }
        let products = Products {
            files: Default::default(),
            epochs,
            notes: vec![],
            screened: 0,
        };
        let inputs = Inputs {
            sources: vec![
                source(
                    Role::Broadcast,
                    "https://example.org/daily/BRDC00WRD_S.rnx.gz",
                ),
                source(
                    Role::Broadcast,
                    "https://example.org/NTRIP/BRDC/brdc_last.rnx.Z",
                ),
            ],
            ..Default::default()
        };
        let at = Instant::parse("2026-09-26T05:43:07Z").unwrap();
        let systems = [System::Gps, System::Glonass, System::Bds, System::Qzs];
        for (flavor, allow_degraded, permitted, extra) in [
            (Flavor::Open, true, true, "partial"),
            (Flavor::OpenPlus, true, false, "hiee"),
        ] {
            let plan = Plan::new(flavor, &systems, true);
            let fields = Value::Object(gate_fields(&inputs, &products, &plan, at, allow_degraded));
            assert_eq!(fields["format"], FORMAT);
            assert_eq!(fields["built_at_unix"], 1_790_401_387);
            assert_eq!(fields["seed"], Value::Null);
            assert_eq!(fields["health"]["source"], "brdc_last.rnx.Z");
            assert_eq!(fields["health"]["sha256"], "Broadcast");
            assert_eq!(fields["degraded"]["permitted"], permitted);
            assert_eq!(fields["plan"]["EXTRA"], extra);
            assert_eq!(fields["plan"]["AGNSS"], "brdc");
            assert_eq!(
                fields["plan"]["BDS"],
                json!({"epochs 0-10": "wum-nrt", "epochs 11-35": "hiee"})
            );
            assert_eq!(
                fields["plan"]["GLONASS"],
                json!({"epochs 0-35": "code5d", "clock": "hiee"})
            );
            assert_eq!(
                fields["plan"]["QZS"],
                json!({"epochs 0-7": "qzu", "epochs 8-35": "none"})
            );
        }
    }

    #[test]
    fn published_names_are_plain_file_names() {
        for (url, name) in [
            (
                "https://example.org/NTRIP/BRDC/brdc_last.rnx.Z",
                "brdc_last.rnx.Z",
            ),
            ("local:/tmp/BRDC00WRD_R.rnx.gz", "BRDC00WRD_R.rnx.gz"),
            ("local:/tmp/..", "broadcast.rnx"),
            ("https://example.org/report.json", "broadcast.rnx"),
        ] {
            assert_eq!(published_name(&source(Role::Broadcast, url)), name);
        }
    }
}
