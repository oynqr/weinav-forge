use anyhow::Result;
use serde_json::Value;
use std::{fs, process::Command};

#[test]
fn every_plan_is_available_without_sources() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for command in ["fetch", "process"] {
        for flavor in ["huawei", "huawei-plus", "open-plus", "open"] {
            let result = Command::new(env!("CARGO_BIN_EXE_weinav-forge"))
                .current_dir(directory.path())
                .args([command, "--flavor", flavor, "--plan"])
                .output()?;
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let plan: Value = serde_json::from_slice(&result.stdout)?;
            assert_eq!(plan["flavor"], flavor);
            assert_eq!(plan["epoch_count"], 36);
        }
    }
    assert_eq!(fs::read_dir(directory.path())?.count(), 0);
    Ok(())
}

#[test]
fn source_failure_removes_only_the_requested_staging_zip() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let output = directory.path().join("staging");
    fs::create_dir(&output)?;
    fs::write(output.join("ephemeris.zip"), b"previous staging output")?;
    let published = directory.path().join("published.zip");
    fs::write(&published, b"keep the published generation")?;
    let result = Command::new(env!("CARGO_BIN_EXE_weinav-forge"))
        .current_dir(directory.path())
        .args([
            "process",
            "--flavor",
            "huawei",
            "--source",
            "seed=missing.dat",
            "--output",
        ])
        .arg(&output)
        .output()?;
    assert_eq!(result.status.code(), Some(3));
    assert!(!output.join("ephemeris.zip").exists());
    assert_eq!(fs::read(published)?, b"keep the published generation");
    let report: Value = serde_json::from_slice(&fs::read(output.join("report.json"))?)?;
    assert_eq!(report["status"], "source-unavailable");
    Ok(())
}

#[test]
fn bad_flavor_is_an_invocation_error() -> Result<()> {
    let result = Command::new(env!("CARGO_BIN_EXE_weinav-forge"))
        .args(["fetch", "--flavor", "unknown"])
        .output()?;
    assert_eq!(result.status.code(), Some(2));
    Ok(())
}
