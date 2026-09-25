use crate::{rtcm, time::Instant};
use anyhow::{Result, ensure};
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::{Cursor, Read, Write},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

pub const PGNSS_TAG: &str = "com.huawei.higeo_dataModule_PGNSSConfig?type=HW_EEV2/HW_PGNSS_PRED;";
pub const AGNSS_TAG: &str = "higeo/v1/gnssinfo?type=0x0024/HW_AGNSS";

pub fn zip(files: &BTreeMap<String, Vec<u8>>, at: Instant) -> Result<Vec<u8>> {
    let mut entries = BTreeMap::new();
    let mut config = BTreeMap::new();
    for (tag, agnss) in [(AGNSS_TAG, true), (PGNSS_TAG, false)] {
        let selected: Vec<_> = files
            .keys()
            .filter(|n| n.starts_with("HW_AGNSS") == agnss)
            .collect();
        if selected.is_empty() {
            continue;
        }
        let uuid = uuid::Uuid::new_v4().to_string();
        for name in &selected {
            ensure!(
                name.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "invalid archive payload name"
            );
            let bytes = &files[*name];
            ensure!(
                !bytes.is_empty() && rtcm::crc16(bytes) != 0,
                "empty or CRC16-zero payload"
            );
            entries.insert(format!("{uuid}/{name}"), bytes.clone());
        }
        config.insert(tag, json!({"ver":3,"uuid":uuid,"files":selected}));
    }
    entries.insert(
        "time".into(),
        at.0.timestamp_millis().to_string().into_bytes(),
    );
    entries.insert("ephemeris_config.json".into(), serde_json::to_vec(&config)?);
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o644);
    for (name, bytes) in &entries {
        writer.start_file(name, options)?;
        writer.write_all(bytes)?;
    }
    let bytes = writer.finish()?.into_inner();
    let mut archive = ZipArchive::new(Cursor::new(&bytes))?;
    ensure!(
        archive.len() == entries.len(),
        "archive entry count changed"
    );
    for (name, expected) in &entries {
        let mut entry = archive.by_name(name)?;
        ensure!(
            entry.compression() == CompressionMethod::Stored,
            "compressed ZIP entry"
        );
        let mut actual = Vec::new();
        entry.read_to_end(&mut actual)?;
        ensure!(&actual == expected, "archive differs from gated bytes");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_stored_payloads_and_exact_tags() -> Result<()> {
        let files = BTreeMap::from([
            ("HW_PGNSS_GPS".into(), vec![1, 2, 3]),
            ("HW_AGNSS_RTCM_33".into(), vec![4, 5, 6]),
        ]);
        let bytes = zip(&files, Instant::parse("2026-09-23T12:00:00Z")?)?;
        let mut archive = ZipArchive::new(Cursor::new(bytes))?;
        let config: serde_json::Value =
            serde_json::from_reader(archive.by_name("ephemeris_config.json")?)?;
        assert_eq!(config[PGNSS_TAG]["ver"], 3);
        assert_eq!(config[AGNSS_TAG]["files"][0], "HW_AGNSS_RTCM_33");
        assert!(
            zip(
                &BTreeMap::from([("HW_PGNSS_GPS".into(), vec![0, 0])]),
                Instant::now()
            )
            .is_err()
        );
        Ok(())
    }
}
