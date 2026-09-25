use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};

use crate::{bits::Reader, policy::System, time::WEEK};

pub const KEY_ORDER: [&str; 20] = [
    "seedInfo", "gpsNav", "bdsNav", "gloNav", "galNav", "qzsNav", "gloAcc", "gpsAlm", "gpsIon",
    "gpsUtc", "gpsRti", "gloAlm", "gloRti", "gloAux", "bdsAlm", "bdsRti", "galAlm", "galRti",
    "qzsRti", "qzsAlm",
];
pub const COEFFICIENT_BITS: [usize; 20] = [
    38, 38, 37, 37, 36, 34, 33, 32, 30, 29, 27, 25, 23, 22, 20, 19, 17, 16, 14, 13,
];
pub const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;

pub fn decompress(bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        bytes.len() as u64 <= MAX_DECODED_BYTES,
        "input exceeds size limit"
    );
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut result = Vec::new();
        flate2::read::MultiGzDecoder::new(bytes)
            .take(MAX_DECODED_BYTES + 1)
            .read_to_end(&mut result)
            .context("invalid gzip product")?;
        ensure!(
            result.len() as u64 <= MAX_DECODED_BYTES,
            "decoded product exceeds size limit"
        );
        Ok(result)
    } else if bytes.starts_with(&[0x1f, 0x9d]) {
        let mut result = Vec::new();
        lzw_z::Decoder::new(bytes)
            .take(MAX_DECODED_BYTES + 1)
            .read_to_end(&mut result)
            .context("invalid Unix compress product")?;
        ensure!(
            result.len() as u64 <= MAX_DECODED_BYTES,
            "decoded product exceeds size limit"
        );
        Ok(result)
    } else {
        Ok(bytes.to_vec())
    }
}

#[derive(Debug)]
pub struct Seed {
    pub fields: BTreeMap<String, Vec<u8>>,
    pub start: i64,
    pub end: i64,
    pub leap: u8,
}

impl Seed {
    pub fn parse(input: &[u8]) -> Result<Self> {
        let plain = decompress(input)?;
        let document: serde_json::Value =
            serde_json::from_slice(&plain).context("invalid HiEE JSON")?;
        let entries = document["HiEE"]
            .as_array()
            .context("HiEE array is missing")?;
        ensure!(
            entries.len() == KEY_ORDER.len(),
            "HiEE must contain 20 positional keys"
        );
        let mut fields = BTreeMap::new();
        for (entry, key) in entries.iter().zip(KEY_ORDER) {
            let object = entry.as_object().context("HiEE entry is not an object")?;
            ensure!(object.len() == 1, "HiEE entry must contain one key");
            let encoded = object
                .get(key)
                .and_then(|v| v.as_str())
                .with_context(|| format!("missing positional key {key}"))?;
            fields.insert(
                key.to_owned(),
                STANDARD
                    .decode(encoded)
                    .with_context(|| format!("invalid base64 in {key}"))?,
            );
        }
        let info = &fields["seedInfo"];
        ensure!(info.len() >= 9, "short seedInfo");
        let start = i64::from(u32::from_be_bytes(info[0..4].try_into()?));
        let end = i64::from(u32::from_be_bytes(info[4..8].try_into()?));
        let leap = info[8];
        ensure!(end > start, "invalid seed validity window");
        Ok(Self {
            fields,
            start,
            end,
            leap,
        })
    }

    pub fn brackets(&self, start: f64, end: f64) -> bool {
        start.is_finite()
            && end.is_finite()
            && start <= end
            && start >= self.start as f64
            && end <= self.end as f64
    }

    pub fn nav(&self, system: System) -> Result<BTreeMap<u8, Satellite>> {
        decode_nav(&self.fields[system.seed_key()], system, self.leap)
    }
}

#[derive(Clone, Debug)]
pub struct Arc {
    pub start: f64,
    pub end: f64,
    pub flag: u8,
    pub af0: f64,
    pub af1: f64,
    pub coefficients: [[f64; 20]; 3],
    pub padded_coefficient: bool,
}

#[derive(Clone, Debug)]
pub struct State {
    pub position: [f64; 3],
    pub velocity: [f64; 3],
    pub acceleration: [f64; 3],
    pub clock: f64,
    pub drift: f64,
}

impl Arc {
    pub fn evaluate(&self, time: f64) -> Result<State> {
        ensure!(
            time.is_finite() && time >= self.start && time <= self.end,
            "time outside seed arc"
        );
        let span = self.end - self.start;
        ensure!(span > 0.0, "empty seed arc");
        let tau = 2.0 * (time - self.start) / span - 1.0;
        let mut basis = [0.0; 20];
        let mut first = [0.0; 20];
        let mut second = [0.0; 20];
        basis[0] = 1.0;
        basis[1] = tau;
        first[1] = 1.0;
        for k in 2..20 {
            basis[k] = 2.0 * tau * basis[k - 1] - basis[k - 2];
            first[k] = 2.0 * basis[k - 1] + 2.0 * tau * first[k - 1] - first[k - 2];
            second[k] = 4.0 * first[k - 1] + 2.0 * tau * second[k - 1] - second[k - 2];
        }
        let dot = |axis: usize, values: &[f64; 20]| {
            self.coefficients[axis]
                .iter()
                .zip(values)
                .map(|(a, b)| a * b)
                .sum::<f64>()
        };
        Ok(State {
            position: std::array::from_fn(|i| dot(i, &basis)),
            velocity: std::array::from_fn(|i| dot(i, &first) * 2.0 / span),
            acceleration: std::array::from_fn(|i| dot(i, &second) * 4.0 / span.powi(2)),
            clock: self.af0 + self.af1 * (time - self.start),
            drift: self.af1,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Satellite {
    pub svid: u8,
    pub health: u8,
    pub tgd: f64,
    pub isc: [f64; 3],
    pub fit_type: u8,
    pub arcs: Vec<Arc>,
}

pub struct Acceleration {
    start: i64,
    step: i64,
    satellites: BTreeMap<u8, Vec<[f64; 3]>>,
}

impl Acceleration {
    pub fn parse(seed: &Seed) -> Result<Self> {
        let mut r = Reader::new(&seed.fields["gloAcc"]);
        let count = r.unsigned(8)?;
        ensure!(count <= 24, "too many GLONASS acceleration satellites");
        let week0 = r.unsigned(13)? as i64;
        let tow0 = r.unsigned(16)? as i64 * 16;
        let step = r.unsigned(6)? as i64 * 60;
        let week1 = r.unsigned(13)? as i64;
        let tow1 = r.unsigned(16)? as i64 * 16;
        let start = week0 * WEEK + tow0 + i64::from(seed.leap);
        let end = week1 * WEEK + tow1 + i64::from(seed.leap);
        ensure!(
            step > 0 && end > start && (end - start) % step == 0,
            "invalid acceleration grid"
        );
        let n = ((end - start) / step) as usize;
        ensure!(n <= 20160, "acceleration grid exceeds bound");
        let mut satellites = BTreeMap::new();
        for _ in 0..count {
            let id = r.unsigned(8)? as u8;
            ensure!((1..=24).contains(&id), "invalid acceleration satellite");
            let mut values = vec![[0.0; 3]; n];
            for axis in 0..3 {
                for value in &mut values {
                    value[axis] = r.signed(5)? as f64 * 1000.0 * 2_f64.powi(-30);
                }
            }
            ensure!(
                satellites.insert(id, values).is_none(),
                "duplicate acceleration satellite"
            );
        }
        ensure!(r.remaining() < 8, "extra acceleration bytes");
        if r.remaining() > 0 {
            ensure!(
                r.unsigned(r.remaining())? == 0,
                "nonzero acceleration padding"
            );
        }
        Ok(Self {
            start,
            step,
            satellites,
        })
    }

    pub fn at(&self, id: u8, time: f64) -> Option<[f64; 3]> {
        let dt = time - self.start as f64;
        if dt < 0.0 || dt % self.step as f64 != 0.0 {
            return None;
        }
        self.satellites
            .get(&id)?
            .get((dt / self.step as f64) as usize)
            .copied()
    }
}

impl Satellite {
    pub fn arc_at(&self, time: f64) -> Option<&Arc> {
        self.arcs
            .iter()
            .find(|a| time >= a.start && time < a.end)
            .or_else(|| self.arcs.last().filter(|a| time == a.end))
    }
}

pub fn decode_nav(bytes: &[u8], system: System, leap: u8) -> Result<BTreeMap<u8, Satellite>> {
    let mut reader = Reader::new(bytes);
    let count = reader.unsigned(8)? as usize;
    ensure!(count <= system.slots(), "too many seed satellites");
    let mut satellites = BTreeMap::new();
    for satellite_index in 0..count {
        let svid = reader.unsigned(8)? as u8;
        ensure!(
            svid > 0 && svid as usize <= system.slots(),
            "invalid seed satellite {svid}"
        );
        let arc_count = reader.unsigned(6)? as usize;
        ensure!((1..=14).contains(&arc_count), "invalid seed arc count");
        let fit_type = reader.unsigned(2)? as u8;
        let health = reader.unsigned(8)? as u8;
        let tgd = reader.signed(16)? as f64 * 2_f64.powi(-32);
        let isc = [
            reader.signed(16)? as f64,
            reader.signed(16)? as f64,
            reader.unsigned(16)? as f64,
        ]
        .map(|v| v * 2_f64.powi(-32));
        reader.unsigned(32)?;
        let mut arcs = Vec::with_capacity(arc_count);
        for arc_index in 0..arc_count {
            let week = reader.unsigned(13)? as i64;
            let flag = reader.unsigned(8)? as u8;
            let t0 = reader.unsigned(16)? as i64 * 16;
            let t1 = reader.unsigned(16)? as i64 * 16;
            let span = (t1 - t0).rem_euclid(WEEK);
            ensure!(span == 43_200, "unexpected seed arc span {span}");
            let af0 = reader.signed(31)? as f64 * 2_f64.powi(-34);
            let af1 = reader.signed(25)? as f64 * 2_f64.powi(-50);
            let mut coefficients = [[0.0; 20]; 3];
            let mut padded_coefficient = false;
            for (axis, values) in coefficients.iter_mut().enumerate() {
                for (index, (&width, value)) in COEFFICIENT_BITS.iter().zip(values).enumerate() {
                    let final_coefficient = system == System::Qzs
                        && satellite_index + 1 == count
                        && arc_index + 1 == arc_count
                        && axis == 2
                        && index == 19;
                    let integer = if final_coefficient && reader.remaining() == width - 4 {
                        padded_coefficient = true;
                        reader.signed(width - 4)? << 4
                    } else {
                        reader.signed(width)?
                    };
                    *value = integer as f64 / 1024.0;
                }
            }
            let start = (week * WEEK
                + t0
                + if system == System::Glonass {
                    i64::from(leap)
                } else {
                    0
                }) as f64;
            if let Some(previous) = arcs.last() {
                let previous: &Arc = previous;
                ensure!(previous.end == start, "non-contiguous seed arcs");
            }
            arcs.push(Arc {
                start,
                end: start + span as f64,
                flag,
                af0,
                af1,
                coefficients,
                padded_coefficient,
            });
        }
        ensure!(
            satellites
                .insert(
                    svid,
                    Satellite {
                        svid,
                        health,
                        tgd,
                        isc,
                        fit_type,
                        arcs
                    }
                )
                .is_none(),
            "duplicate seed satellite {svid}"
        );
    }
    ensure!(reader.remaining() < 8, "unexpected trailing seed bytes");
    if reader.remaining() > 0 {
        ensure!(
            reader.unsigned(reader.remaining())? == 0,
            "nonzero seed padding"
        );
    }
    Ok(satellites)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chebyshev_derivatives_match_finite_differences() -> Result<()> {
        let mut arc = Arc {
            start: 0.0,
            end: 43_200.0,
            flag: 0,
            af0: 0.0,
            af1: 0.0,
            coefficients: [[0.0; 20]; 3],
            padded_coefficient: false,
        };
        arc.coefficients[0][3] = 2.0e7;
        let t = 15_000.0;
        let a = arc.evaluate(t - 1.0)?;
        let b = arc.evaluate(t)?;
        let c = arc.evaluate(t + 1.0)?;
        assert!((b.velocity[0] - (c.position[0] - a.position[0]) / 2.0).abs() < 1e-5);
        assert!((b.acceleration[0] - (c.velocity[0] - a.velocity[0]) / 2.0).abs() < 1e-8);
        assert!(arc.evaluate(-1.0).is_err());
        Ok(())
    }
}
