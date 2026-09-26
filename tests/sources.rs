use anyhow::Result;
use std::{collections::BTreeMap, fs, path::Path};
use weinav_forge::{
    build::Inputs,
    cache::{self, Manifest, Source},
    fetch,
    policy::{Flavor, Role, System},
};

fn rinex(clock: f64) -> Vec<u8> {
    let mut text = format!(
        "{:9}{:51}RINEX VERSION / TYPE\n{:60}END OF HEADER\nG01 2026 09 25 00 00 00{clock:19.12E}{:19.12E}{:19.12E}\n",
        "3.05", "", "", 0.0, 0.0
    );
    for _ in 0..7 {
        text.push_str(&format!(
            "    {:19.12E}{:19.12E}{:19.12E}{:19.12E}\n",
            1.0, 1.0, 1.0, 1.0
        ));
    }
    text.into_bytes()
}

fn antex() -> Vec<u8> {
    [
        (String::new(), "START OF ANTENNA"),
        (format!("{:20}{:20}", "", "G01"), "TYPE / SERIAL NO"),
        ("G01".into(), "START OF FREQUENCY"),
        ("0 0 1000".into(), "NORTH / EAST / UP"),
        ("G02".into(), "START OF FREQUENCY"),
        ("0 0 1000".into(), "NORTH / EAST / UP"),
        (String::new(), "END OF ANTENNA"),
    ]
    .into_iter()
    .map(|(data, label)| format!("{data:60}{label}\n"))
    .collect::<String>()
    .into_bytes()
}

fn load(root: &Path, local: &BTreeMap<Role, Vec<std::path::PathBuf>>) -> Result<Inputs> {
    Inputs::load(root, None, Flavor::Open, &[System::Gps], false, local)
}

#[test]
fn cached_sources_keep_validation_and_local_overrides() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let at = "2026-09-25T00:00:00Z";
    let gps = b"ID: 1\nEccentricity: 0.01\n";
    let galileo = b"<almanac/>";
    let mut manifest = Manifest {
        version: 1,
        flavor: Flavor::Open,
        systems: vec![System::Gps],
        agnss: false,
        created_at: at.into(),
        sources: Vec::new(),
        attempts: Vec::new(),
    };
    for (role, bytes) in [
        (Role::Broadcast, rinex(1.0)),
        (Role::Antex, antex()),
        (Role::GpsAlmanac, gps.to_vec()),
        (Role::GalileoAlmanac, galileo.to_vec()),
    ] {
        let source = Source {
            role,
            provider: fetch::provider(role).into(),
            url: format!("fixture:{role:?}"),
            sha256: cache::hash(&bytes),
            bytes: bytes.len(),
            fetched_at: at.into(),
            etag: None,
            last_modified: None,
            coverage: fetch::inspect(role, &bytes)?,
            version: None,
        };
        cache::store_source(root, &source, &bytes)?;
        manifest.sources.push(source);
    }
    let path = root.join(fetch::manifest_name(Flavor::Open));
    cache::atomic_write(&path, &serde_json::to_vec(&manifest)?)?;
    let inputs = load(root, &BTreeMap::new())?;
    let nav = &inputs.broadcast.records[&(System::Gps, 1)][0];
    assert_eq!(nav.get("af0")?, 1.0);
    assert_eq!(nav.get("i0")?, 1.0);
    assert_eq!(nav.semicircle_fields()["i0"], 1.0 / std::f64::consts::PI);
    assert_eq!(nav.get("i0")?, 1.0);
    assert!(
        (inputs
            .antex
            .as_ref()
            .unwrap()
            .radial((System::Gps, 1), 0.0)?
            - 1.0)
            .abs()
            < 1e-12
    );
    assert_eq!(inputs.raw.len(), 2);
    assert_eq!(inputs.raw[&Role::GpsAlmanac][0], gps);
    assert_eq!(inputs.raw[&Role::GalileoAlmanac][0], galileo);
    assert_eq!(inputs.provider_names("code5d"), ["brdc"]);

    manifest.sources.push(manifest.sources[1].clone());
    cache::atomic_write(&path, &serde_json::to_vec(&manifest)?)?;
    assert!(
        load(root, &BTreeMap::new())
            .err()
            .unwrap()
            .to_string()
            .contains("one ANTEX")
    );
    manifest.sources.pop();
    let almanac = manifest.sources.pop().unwrap();
    cache::atomic_write(&path, &serde_json::to_vec(&manifest)?)?;
    assert!(
        load(root, &BTreeMap::new())
            .err()
            .unwrap()
            .to_string()
            .contains("required source is absent")
    );
    manifest.sources.push(almanac);
    cache::atomic_write(&path, &serde_json::to_vec(&manifest)?)?;

    fs::write(
        cache::object_path(root, &manifest.sources[0].sha256)?,
        b"corrupt",
    )?;
    assert!(
        load(root, &BTreeMap::new())
            .err()
            .unwrap()
            .to_string()
            .contains("cached source hash mismatch")
    );
    let replacement = root.join("replacement.rnx");
    fs::write(&replacement, rinex(2.0))?;
    let inputs = load(
        root,
        &BTreeMap::from([(Role::Broadcast, vec![replacement])]),
    )?;
    assert_eq!(
        inputs.broadcast.records[&(System::Gps, 1)][0].get("af0")?,
        2.0
    );
    assert_eq!(inputs.sources.len(), 4);
    assert!(
        inputs
            .sources
            .iter()
            .find(|s| s.role == Role::Broadcast)
            .unwrap()
            .url
            .starts_with("local:")
    );
    Ok(())
}
