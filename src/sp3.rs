use crate::{
    policy::System,
    rinex::{calendar, number},
    seed::{State, decompress},
    time::{GPS_EPOCH_UNIX, Instant},
};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct Sample {
    pub time: f64,
    pub position: [f64; 3],
    pub clock: Option<f64>,
    pub predicted: bool,
}

#[derive(Default)]
pub struct Sp3 {
    pub satellites: BTreeMap<(System, u8), Vec<Sample>>,
}

impl Sp3 {
    pub fn add(&mut self, input: &[u8]) -> Result<()> {
        let bytes = decompress(input)?;
        let text = std::str::from_utf8(&bytes)?;
        ensure!(text.is_ascii() && text.starts_with('#'), "invalid SP3 file");
        for samples in self.satellites.values_mut() {
            samples.sort_by(|a, b| a.time.total_cmp(&b.time));
        }
        let mut time = None;
        let mut time_system = "GPS";
        let mut count = 0;
        for line in text.lines() {
            if line.starts_with("%c") {
                if let Some(scale) = line
                    .get(9..12)
                    .map(str::trim)
                    .filter(|s| matches!(*s, "GPS" | "UTC" | "GAL" | "QZS" | "BDT" | "TAI"))
                {
                    time_system = scale;
                }
            } else if let Some(date) = line.strip_prefix('*') {
                let date = calendar(&date.split_whitespace().collect::<Vec<_>>())?;
                let naive = (date.timestamp() - GPS_EPOCH_UNIX) as f64
                    + f64::from(date.timestamp_subsec_nanos()) / 1e9;
                time = Some(match time_system {
                    "UTC" => Instant(date).gps() as f64,
                    "BDT" => naive + 14.0,
                    "TAI" => naive - 19.0,
                    _ => naive,
                });
            } else if line.starts_with('P') {
                let Some(system) = line.chars().nth(1).and_then(System::from_code) else {
                    continue;
                };
                let svid: u8 = line.get(2..4).context("short SP3 id")?.trim().parse()?;
                let time = time.context("SP3 position precedes epoch")?;
                let mut values = [0.0; 4];
                for (i, value) in values.iter_mut().enumerate() {
                    *value = number(
                        line.get(4 + i * 14..18 + i * 14)
                            .context("short SP3 position")?,
                    )?;
                }
                if values[..3].iter().all(|&v| v == 0.0)
                    || values[..3].iter().any(|v| v.abs() >= 999_999.0)
                {
                    continue;
                }
                let sample = Sample {
                    time,
                    position: [values[0] * 1000.0, values[1] * 1000.0, values[2] * 1000.0],
                    clock: if values[3].abs() >= 999_999.0 {
                        None
                    } else {
                        Some(values[3] * 1e-6)
                    },
                    predicted: line.as_bytes().get(79) == Some(&b'P'),
                };
                let samples = self.satellites.entry((system, svid)).or_default();
                if samples.last().is_none_or(|last| last.time < time) {
                    samples.push(sample);
                } else {
                    match samples.binary_search_by(|old| old.time.total_cmp(&time)) {
                        Ok(index) => samples[index] = sample,
                        Err(index) => samples.insert(index, sample),
                    }
                }
                count += 1;
            }
        }
        ensure!(count > 0, "SP3 has no usable satellite positions");
        Ok(())
    }

    pub fn window(&self, key: (System, u8)) -> Option<(f64, f64)> {
        let samples = self.satellites.get(&key)?;
        if samples.len() < 9 {
            return None;
        }
        Some((samples[4].time, samples[samples.len() - 5].time))
    }

    pub fn state(&self, key: (System, u8), time: f64) -> Result<State> {
        let samples = self
            .satellites
            .get(&key)
            .context("satellite absent from SP3")?;
        let (start, end) = self.window(key).context("too few SP3 samples")?;
        ensure!(
            time >= start && time <= end,
            "SP3 interpolation reaches file edge"
        );
        let nearest = samples.partition_point(|s| s.time < time);
        let center = nearest.min(samples.len() - 5).max(4);
        let stencil = &samples[center - 4..center + 5];
        let interval = stencil[1].time - stencil[0].time;
        ensure!(
            interval > 0.0
                && stencil
                    .windows(2)
                    .all(|p| (p[1].time - p[0].time - interval).abs() < 1e-3),
            "gap in SP3 interpolation samples"
        );
        let evaluate = |t: f64| -> Result<([f64; 3], f64)> {
            let mut xyz = [0.0; 3];
            let mut clock = 0.0;
            for (i, sample) in stencil.iter().enumerate() {
                let weight = stencil
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, s)| (t - s.time) / (sample.time - s.time))
                    .product::<f64>();
                for (axis, value) in xyz.iter_mut().enumerate() {
                    *value += weight * sample.position[axis];
                }
                clock += weight * sample.clock.context("missing SP3 clock")?;
            }
            Ok((xyz, clock))
        };
        let (position, clock) = evaluate(time)?;
        let (left, cl) = evaluate(time - 0.5)?;
        let (right, cr) = evaluate(time + 0.5)?;
        Ok(State {
            position,
            velocity: std::array::from_fn(|i| right[i] - left[i]),
            acceleration: std::array::from_fn(|i| 4.0 * (right[i] - 2.0 * position[i] + left[i])),
            clock,
            drift: cr - cl,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(minute: u8, x: f64, clock: f64, predicted: bool) -> String {
        format!(
            "* 2026 09 25 00 {minute:02} 00.0\nPG01{x:14.6}{:14.6}{:14.6}{clock:14.6}{:19}{}\n",
            20_000.0,
            30_000.0,
            "",
            if predicted { 'P' } else { ' ' }
        )
    }

    #[test]
    fn merges_unordered_and_overlapping_epochs_with_last_sample_winning() -> Result<()> {
        let mut sp3 = Sp3::default();
        let mut first = String::from("#cP\n");
        for minute in (0..=10).step_by(2) {
            first.push_str(&sample(minute, 10_000.0 + f64::from(minute), 1.0, false));
        }
        sp3.add(first.as_bytes())?;
        let mut second = String::from("#cP\n");
        for minute in [9, 1, 7, 3, 5] {
            second.push_str(&sample(minute, 10_000.0 + f64::from(minute), 1.0, false));
        }
        second.push_str(&sample(4, 50_000.0, 2.0, false));
        second.push_str(&sample(4, 60_000.0, 999_999.0, true));
        sp3.add(second.as_bytes())?;
        let key = (System::Gps, 1);
        let samples = &sp3.satellites[&key];
        assert_eq!(samples.len(), 11);
        for (minute, entry) in samples.iter().enumerate() {
            assert_eq!(entry.time - samples[0].time, minute as f64 * 60.0);
            if minute != 4 {
                assert_eq!(entry.position[0], (10_000.0 + minute as f64) * 1000.0);
                assert_eq!(entry.clock, Some(1e-6));
                assert!(!entry.predicted);
            }
        }
        assert_eq!(samples[4].position[0], 60_000_000.0);
        assert!(samples[4].clock.is_none());
        assert!(samples[4].predicted);
        let time = samples[5].time;
        assert_eq!(sp3.window(key), Some((samples[4].time, samples[6].time)));
        assert!(sp3.state(key, time).is_err());
        sp3.add(format!("#cP\n{}", sample(4, 10_004.0, 1.0, false)).as_bytes())?;
        let state = sp3.state(key, time)?;
        assert_eq!(state.position[0], 10_005_000.0);
        assert!((state.velocity[0] - 1000.0 / 60.0).abs() < 1e-7);
        Ok(())
    }
}

#[derive(Default)]
pub struct Antex {
    offsets: BTreeMap<(System, u8), Vec<Antenna>>,
}

struct Antenna {
    start: f64,
    end: f64,
    frequencies: BTreeMap<String, [f64; 3]>,
}

impl Antex {
    pub fn parse(input: &[u8]) -> Result<Self> {
        let bytes = decompress(input)?;
        let text = std::str::from_utf8(&bytes)?;
        ensure!(text.is_ascii(), "non-ASCII ANTEX");
        let mut result = Self::default();
        let mut key = None;
        let mut antenna = Antenna {
            start: f64::NEG_INFINITY,
            end: f64::INFINITY,
            frequencies: BTreeMap::new(),
        };
        let mut frequency = String::new();
        for line in text.lines() {
            let label = line.get(60..).unwrap_or("").trim();
            let data = line.get(..60).unwrap_or(line);
            match label {
                "START OF ANTENNA" => {
                    key = None;
                    antenna = Antenna {
                        start: f64::NEG_INFINITY,
                        end: f64::INFINITY,
                        frequencies: BTreeMap::new(),
                    };
                }
                "TYPE / SERIAL NO" => {
                    let serial = data.get(20..40).unwrap_or("").trim();
                    if serial.len() == 3
                        && let Some(system) = serial.chars().next().and_then(System::from_code)
                    {
                        key = serial[1..].parse::<u8>().ok().map(|id| (system, id));
                    }
                }
                "VALID FROM" | "VALID UNTIL" => {
                    let parts: Vec<_> = data.split_whitespace().collect();
                    if parts.len() == 6 {
                        let t = calendar(&parts)?;
                        let gps = (t.timestamp() - GPS_EPOCH_UNIX) as f64;
                        if label == "VALID FROM" {
                            antenna.start = gps;
                        } else {
                            antenna.end = gps;
                        }
                    }
                }
                "START OF FREQUENCY" => frequency = data.trim().to_owned(),
                "NORTH / EAST / UP" => {
                    let values: Vec<_> =
                        data.split_whitespace().map(number).collect::<Result<_>>()?;
                    ensure!(values.len() == 3, "invalid ANTEX offset");
                    antenna.frequencies.insert(
                        frequency.clone(),
                        [values[0] / 1000.0, values[1] / 1000.0, values[2] / 1000.0],
                    );
                }
                "END OF ANTENNA" => {
                    if let Some(key) = key {
                        result.offsets.entry(key).or_default().push(antenna);
                    }
                    antenna = Antenna {
                        start: f64::NEG_INFINITY,
                        end: f64::INFINITY,
                        frequencies: BTreeMap::new(),
                    };
                }
                _ => {}
            }
        }
        ensure!(!result.offsets.is_empty(), "ANTEX has no satellite offsets");
        Ok(result)
    }

    pub fn radial(&self, key: (System, u8), time: f64) -> Result<f64> {
        let antennas = self
            .offsets
            .get(&key)
            .context("satellite has no ANTEX entry")?;
        let antenna = antennas
            .iter()
            .rev()
            .find(|a| time >= a.start && time <= a.end)
            .context("ANTEX validity does not cover epoch")?;
        let (one, two, f1, f2) = match key.0 {
            System::Gps => ("G01", "G02", 1575.42_f64, 1227.60_f64),
            System::Glonass => ("R01", "R02", 1602.0, 1246.0),
            System::Galileo => ("E01", "E05", 1575.42, 1176.45),
            System::Bds => ("C02", "C06", 1561.098, 1268.52),
            System::Qzs => ("J01", "J05", 1575.42, 1176.45),
        };
        let a = antenna
            .frequencies
            .get(one)
            .context("ANTEX primary frequency missing")?;
        let b = antenna
            .frequencies
            .get(two)
            .context("ANTEX secondary frequency missing")?;
        Ok((f1 * f1 * a[2] - f2 * f2 * b[2]) / (f1 * f1 - f2 * f2))
    }

    pub fn correct(&self, key: (System, u8), time: f64, mut state: State) -> Result<State> {
        let z = self.radial(key, time)?;
        let radius = state.position.iter().map(|v| v * v).sum::<f64>().sqrt();
        let radial_velocity = state
            .position
            .iter()
            .zip(state.velocity)
            .map(|(p, v)| p * v)
            .sum::<f64>()
            / radius;
        for i in 0..3 {
            state.velocity[i] -= z
                * (state.velocity[i] / radius
                    - state.position[i] * radial_velocity / radius.powi(2));
            state.position[i] *= 1.0 - z / radius;
        }
        Ok(state)
    }
}
