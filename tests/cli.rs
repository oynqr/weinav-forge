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

#[test]
fn batch_plans_and_failures_keep_variants_separate() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let variants = directory.path().join("variants.json");
    fs::write(
        &variants,
        r#"[
        {"name":"first","flavor":"huawei","systems":["gps"],"agnss":false},
        {"name":"second","flavor":"open","allow_degraded":true}
    ]"#,
    )?;
    let invoke = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_weinav-forge"))
            .current_dir(directory.path())
            .args(["process", "--variants", "variants.json"])
            .args(extra)
            .output()
    };
    let plan = invoke(&["--plan"])?;
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let plans: Value = serde_json::from_slice(&plan.stdout)?;
    assert_eq!(plans[0]["flavor"], "huawei");
    assert_eq!(plans[1]["flavor"], "open");
    assert!(!directory.path().join("output").exists());
    for threads in ["1", "2"] {
        let result = invoke(&["--threads", threads])?;
        assert_eq!(result.status.code(), Some(3));
        for name in ["first", "second"] {
            let path = directory.path().join("output").join(name);
            let report: Value = serde_json::from_slice(&fs::read(path.join("report.json"))?)?;
            assert_eq!(report["status"], "source-unavailable");
            assert!(!path.join("ephemeris.zip").exists());
        }
    }
    fs::remove_dir_all(directory.path().join("output/first"))?;
    fs::write(directory.path().join("output/first"), b"not a directory")?;
    fs::remove_file(directory.path().join("output/second/report.json"))?;
    assert_eq!(invoke(&[])?.status.code(), Some(1));
    assert!(directory.path().join("output/second/report.json").exists());
    for extra in [
        &["--flavor", "huawei"][..],
        &["--systems", "gps"],
        &["--fit-rms", "1"],
        &["--source", "seed=x"],
        &["--threads", "0"],
    ] {
        assert_eq!(invoke(extra)?.status.code(), Some(2));
    }
    Ok(())
}

#[test]
fn invalid_batches_fail_before_writing_output() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for variants in [
        serde_json::json!([]),
        serde_json::json!([{"name":"../escape","flavor":"huawei"}]),
        serde_json::json!([{"name":"same","flavor":"huawei"},{"name":"same","flavor":"open"}]),
        serde_json::json!([{"name":"first","flavor":"huawei"},{"name":"bad","flavor":"open","systems":["galileo"]}]),
        serde_json::json!([{"name":"bad","flavor":"huawei","systems":["gps","gps"]}]),
        serde_json::json!([{"name":"bad","flavor":"huawei","fit_rms":0}]),
        serde_json::json!([{"name":"bad","flavor":"huawei","typo":true}]),
    ] {
        fs::write(
            directory.path().join("variants.json"),
            serde_json::to_vec(&variants)?,
        )?;
        let result = Command::new(env!("CARGO_BIN_EXE_weinav-forge"))
            .current_dir(directory.path())
            .args(["process", "--variants", "variants.json"])
            .output()?;
        assert_eq!(result.status.code(), Some(1));
        assert!(!directory.path().join("output").exists());
    }
    Ok(())
}

#[test]
#[ignore = "requires WEINAV_PROCESS_CACHE and WEINAV_PROCESS_AT with usable Huawei sources"]
fn parallel_batches_preserve_payloads_reports_and_partial_success() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = std::env::var("WEINAV_PROCESS_CACHE")?;
    let at = std::env::var("WEINAV_PROCESS_AT")?;
    let variants = serde_json::json!([
        {"name":"three","flavor":"huawei-plus","systems":["gps","galileo","qzs"],"agnss":false},
        {"name":"five","flavor":"huawei-plus","agnss":false},
        {"name":"seed","flavor":"huawei","systems":["gps","galileo","qzs"],"agnss":false}
    ]);
    fs::write(
        directory.path().join("variants.json"),
        serde_json::to_vec(&variants)?,
    )?;
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_weinav-forge"))
            .current_dir(directory.path())
            .args(["process", "--cache", &cache, "--at", &at])
            .args(args)
            .output()
    };
    let normalize = |name: &str| -> Result<Value> {
        let mut report: Value =
            serde_json::from_slice(&fs::read(directory.path().join(name).join("report.json"))?)?;
        let object = report.as_object_mut().unwrap();
        object.remove("elapsed_seconds");
        object.remove("zip_sha256");
        Ok(report)
    };
    let mut expected_status = 0;
    for (name, flavor, systems) in [
        ("three", "huawei-plus", "gps,galileo,qzs"),
        ("five", "huawei-plus", "gps,glonass,galileo,bds,qzs"),
        ("seed", "huawei", "gps,galileo,qzs"),
    ] {
        let result = invoke(&[
            "--flavor",
            flavor,
            "--systems",
            systems,
            "--no-agnss",
            "--threads",
            "1",
            "--output",
            name,
        ])?;
        let code = result.status.code().unwrap();
        assert!(
            [0, 4].contains(&code),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        expected_status = expected_status.max(code);
        let report = normalize(name)?;
        assert!(
            report["epochs"]
                .as_array()
                .is_some_and(|epochs| !epochs.is_empty())
        );
        if name == "three" {
            assert_eq!(code, 0, "three-system fixture must pass its gates");
        }
    }
    for threads in ["1", "2", "4"] {
        let result = invoke(&[
            "--variants",
            "variants.json",
            "--threads",
            threads,
            "--output",
            "batch",
        ])?;
        assert_eq!(result.status.code(), Some(expected_status));
        for name in ["three", "five", "seed"] {
            assert_eq!(normalize(name)?, normalize(&format!("batch/{name}"))?);
            assert_eq!(
                directory.path().join(name).join("ephemeris.zip").exists(),
                directory
                    .path()
                    .join("batch")
                    .join(name)
                    .join("ephemeris.zip")
                    .exists()
            );
        }
    }
    Ok(())
}
