use crate::policy::System;
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;

#[path = "envelopes.rs"]
mod envelopes;
#[path = "record_fields.rs"]
mod schema;

pub struct Field {
    pub name: &'static str,
    pub offset: usize,
    pub bytes: usize,
    pub signed: bool,
    pub scale: f64,
}

pub fn decode(system: System, bytes: &[u8]) -> Result<BTreeMap<String, f64>> {
    ensure!(
        bytes.len() == system.record_size(),
        "wrong {} record size",
        system.name()
    );
    let mut used = vec![false; bytes.len()];
    let mut values = BTreeMap::new();
    for field in schema::fields(system) {
        let data = &bytes[field.offset..field.offset + field.bytes];
        let sign = field.signed && data[field.bytes - 1] & 0x80 != 0;
        let mut extended = [if sign { 0xff } else { 0 }; 8];
        extended[..field.bytes].copy_from_slice(data);
        let integer = i64::from_le_bytes(extended);
        values.insert(field.name.into(), integer as f64 * field.scale);
        used[field.offset..field.offset + field.bytes].fill(true);
    }
    for (i, (&byte, mapped)) in bytes.iter().zip(used).enumerate() {
        ensure!(
            mapped || byte == 0,
            "{} nonzero reserved byte at 0x{i:02x}",
            system.name()
        );
    }
    Ok(values)
}

pub fn encode(system: System, values: &BTreeMap<String, f64>) -> Result<Vec<u8>> {
    let mut bytes = vec![0; system.record_size()];
    for field in schema::fields(system) {
        let value = values
            .get(field.name)
            .with_context(|| format!("missing {} field {}", system.name(), field.name))?;
        let integer = (value / field.scale).round_ties_even();
        let width = field.bytes * 8;
        let (low, high) = if field.signed {
            (-(1_i64 << (width - 1)), (1_i64 << (width - 1)) - 1)
        } else {
            (0, (1_i64 << width) - 1)
        };
        ensure!(
            integer.is_finite() && integer >= low as f64 && integer <= high as f64,
            "{} field {} out of range",
            system.name(),
            field.name
        );
        bytes[field.offset..field.offset + field.bytes]
            .copy_from_slice(&(integer as i64).to_le_bytes()[..field.bytes]);
    }
    Ok(bytes)
}

pub fn zero_values(system: System) -> BTreeMap<String, f64> {
    schema::fields(system)
        .iter()
        .map(|f| (f.name.to_owned(), 0.0))
        .collect()
}

pub fn fit_bounds(system: System) -> Vec<(usize, f64, f64)> {
    crate::orbit::PARAMETER_NAMES
        .iter()
        .enumerate()
        .filter(|(index, _)| !matches!(index, 1 | 3..=5))
        .filter_map(|(index, name)| {
            envelopes::bounds(system)
                .iter()
                .find(|(field, _, _)| field == name)
                .map(|&(_, low, high)| {
                    let scale = if matches!(index, 2..=8) {
                        std::f64::consts::PI
                    } else {
                        1.0
                    };
                    let low = if system == System::Qzs && index == 6 {
                        low.max(0.0)
                    } else {
                        low
                    };
                    (index, low * scale, high * scale)
                })
        })
        .collect()
}

pub fn truncate(system: System, values: &mut BTreeMap<String, f64>) {
    for field in schema::fields(system) {
        if let Some(value) = values.get_mut(field.name) {
            *value = (*value / field.scale).trunc() * field.scale;
        }
    }
}

pub fn validate(system: System, values: &BTreeMap<String, f64>) -> Result<()> {
    let id = values[if system == System::Glonass {
        "slot"
    } else {
        "sv"
    }];
    ensure!(
        id >= 0.0 && id < system.slots() as f64,
        "satellite index outside capacity"
    );
    if system == System::Glonass {
        let radius = ["x", "y", "z"]
            .iter()
            .map(|k| values[*k].powi(2))
            .sum::<f64>()
            .sqrt();
        let velocity = ["vx", "vy", "vz"]
            .iter()
            .map(|k| values[*k].powi(2))
            .sum::<f64>()
            .sqrt();
        ensure!(
            (24000.0..27000.0).contains(&radius) && (2.5..4.5).contains(&velocity),
            "GLONASS state outside physical bounds"
        );
        ensure!(
            values["t_b"] < 86400.0 && values["flag"] == 1.0,
            "invalid GLONASS time or flag"
        );
        for name in ["ax", "ay", "az"] {
            ensure!(
                values[name].abs() <= 15.0 * 2_f64.powi(-30),
                "GLONASS acceleration exceeds ICD width"
            );
        }
    } else {
        ensure!(
            (5e6..6e7).contains(&values["sqrt_a"].powi(2)) && (0.0..=0.25).contains(&values["ecc"]),
            "Kepler orbit outside physical bounds"
        );
        ensure!(
            values["toe"] < 604800.0 && values["toc"] < 604800.0,
            "time outside week"
        );
        if system == System::Qzs {
            ensure!(
                values["delta_n"] >= 0.0,
                "QZSS negative mean-motion correction"
            );
        }
        for field in schema::fields(system) {
            let width = match field.name {
                "delta_n" => 16,
                "omegadot" => 24,
                "idot" => 14,
                "cuc" | "cus" | "cic" | "cis" | "crc" | "crs" => {
                    if system == System::Bds {
                        18
                    } else {
                        16
                    }
                }
                _ => continue,
            };
            let raw = (values[field.name] / field.scale).round_ties_even();
            ensure!(
                raw >= -(1_i64 << (width - 1)) as f64 && raw < (1_i64 << (width - 1)) as f64,
                "{} exceeds vendor width",
                field.name
            );
        }
    }
    Ok(())
}

pub fn validate_envelope(system: System, values: &BTreeMap<String, f64>) -> Result<()> {
    for &(name, low, high) in envelopes::bounds(system) {
        let value = values[name];
        ensure!(
            value >= low && value <= high,
            "{name}={value:e} outside measured envelope [{low:e}, {high:e}]"
        );
    }
    Ok(())
}

pub struct Epoch {
    pub time: u32,
    pub blocks: Vec<Vec<Vec<u8>>>,
}

pub const INDEX_BYTES: usize = 1008;

pub fn container(system: System, epochs: &[Epoch]) -> Result<Vec<u8>> {
    ensure!(
        !epochs.is_empty() && epochs.len() <= 84,
        "invalid epoch count"
    );
    let block_size = 4 + system.slots() * system.record_size();
    let payload_size = block_size * system.subblocks();
    let mut result = vec![0; INDEX_BYTES + epochs.len() * payload_size];
    for (i, epoch) in epochs.iter().enumerate() {
        ensure!(
            epoch.blocks.len() == system.subblocks(),
            "invalid subblock count"
        );
        if i > 0 {
            ensure!(
                epoch.time.checked_sub(epochs[i - 1].time) == Some(7200),
                "invalid epoch step"
            );
        }
        let offset = INDEX_BYTES + i * payload_size;
        for (j, value) in [epoch.time, offset as u32, payload_size as u32]
            .into_iter()
            .enumerate()
        {
            result[i * 12 + j * 4..i * 12 + j * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (j, records) in epoch.blocks.iter().enumerate() {
            ensure!(records.len() <= system.slots(), "too many records");
            let base = offset + j * block_size;
            result[base..base + 4].copy_from_slice(&(records.len() as u32).to_le_bytes());
            for (k, record) in records.iter().enumerate() {
                decode(system, record)?;
                let start = base + 4 + k * system.record_size();
                result[start..start + system.record_size()].copy_from_slice(record);
            }
        }
    }
    Ok(result)
}

pub fn parse_container(system: System, data: &[u8]) -> Result<Vec<Epoch>> {
    ensure!(data.len() >= INDEX_BYTES, "truncated PGNSS index");
    let u32_at = |offset| u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    if u32_at(4) == 0 {
        ensure!(
            data[..INDEX_BYTES].iter().all(|&b| b == 0),
            "invalid empty index"
        );
        return Ok(Vec::new());
    }
    ensure!(
        u32_at(4) as usize == INDEX_BYTES,
        "expected 84 PGNSS index slots"
    );
    let block_size = 4 + system.slots() * system.record_size();
    let length = block_size * system.subblocks();
    let mut epochs: Vec<Epoch> = Vec::new();
    let mut end = INDEX_BYTES;
    let mut terminated = false;
    for i in 0..84 {
        let (time, offset, size) = (
            u32_at(i * 12),
            u32_at(i * 12 + 4) as usize,
            u32_at(i * 12 + 8) as usize,
        );
        if (time, offset, size) == (0, 0, 0) {
            terminated = true;
            continue;
        }
        ensure!(!terminated, "live PGNSS entry after index terminator");
        ensure!(
            size == length && offset == end,
            "invalid PGNSS payload geometry"
        );
        end = offset.checked_add(size).context("PGNSS offset overflow")?;
        ensure!(end <= data.len(), "truncated PGNSS payload");
        if let Some(previous) = epochs.last() {
            ensure!(
                time.checked_sub(previous.time) == Some(7200),
                "invalid PGNSS epoch step"
            );
        }
        let mut blocks = Vec::new();
        for j in 0..system.subblocks() {
            let base = offset + j * block_size;
            let count = u32_at(base) as usize;
            ensure!(count <= system.slots(), "PGNSS count exceeds capacity");
            let records = (0..count)
                .map(|k| {
                    let start = base + 4 + k * system.record_size();
                    data[start..start + system.record_size()].to_vec()
                })
                .collect();
            ensure!(
                data[base + 4 + count * system.record_size()..base + block_size]
                    .iter()
                    .all(|&b| b == 0),
                "nonzero unused PGNSS slot"
            );
            blocks.push(records);
        }
        epochs.push(Epoch { time, blocks });
    }
    ensure!(end == data.len(), "unindexed trailing PGNSS bytes");
    Ok(epochs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_curvature_fields_must_be_zero_for_every_provider() {
        for (system, name, value) in [
            (System::Glonass, "gamma_n", 2_f64.powi(-40)),
            (System::Bds, "af2", 2_f64.powi(-66)),
        ] {
            let mut values: BTreeMap<String, f64> = envelopes::bounds(system)
                .iter()
                .map(|&(field, low, high)| (field.to_owned(), (low + high) / 2.0))
                .collect();
            assert!(validate_envelope(system, &values).is_ok());
            values.insert(name.into(), value);
            assert!(validate_envelope(system, &values).is_err());
        }
    }

    #[test]
    fn qzss_negative_delay_has_zero_pad() -> Result<()> {
        let mut values = zero_values(System::Qzs);
        values.insert("tgd".into(), -50.0 * 2_f64.powi(-31));
        let mut bytes = encode(System::Qzs, &values)?;
        assert_eq!(&bytes[0x14..0x16], &[0xce, 0]);
        bytes[0x15] = 0xff;
        assert!(decode(System::Qzs, &bytes).is_err());
        Ok(())
    }

    #[test]
    fn empty_slots_keep_fixed_allocation() -> Result<()> {
        let epochs = vec![
            Epoch {
                time: 7200,
                blocks: vec![vec![]],
            },
            Epoch {
                time: 14400,
                blocks: vec![vec![]],
            },
        ];
        let mut bytes = container(System::Qzs, &epochs)?;
        assert_eq!(bytes.len(), 1008 + 2 * 764);
        assert_eq!(parse_container(System::Qzs, &bytes)?.len(), 2);
        bytes.pop();
        assert!(parse_container(System::Qzs, &bytes).is_err());
        Ok(())
    }
}
