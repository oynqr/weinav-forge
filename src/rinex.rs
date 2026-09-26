use crate::{
    orbit,
    policy::System,
    seed::{State, decompress},
    time::{GPS_EPOCH_UNIX, Instant},
};
use anyhow::{Context, Result, ensure};
use chrono::{TimeZone, Utc};
use std::{borrow::Cow, collections::BTreeMap, f64::consts::PI};

#[path = "rinex_fields.rs"]
mod schema;

const MAX_NAVIGATION_FIELDS: usize = 31;

#[derive(Clone, Debug)]
pub struct Navigation {
    pub system: System,
    pub svid: u8,
    pub epoch: f64,
    pub system_epoch: f64,
    pub values: Values,
}

#[derive(Clone, Debug)]
pub struct Values {
    names: &'static [&'static str],
    data: Box<[f64]>,
    present: u32,
}

impl Values {
    fn new(names: &'static [&'static str], fields: &[Option<f64>; MAX_NAVIGATION_FIELDS]) -> Self {
        let mut present = 0;
        let data = names
            .iter()
            .zip(fields)
            .enumerate()
            .map(|(index, (_, value))| {
                if let Some(value) = value {
                    present |= 1 << index;
                    *value
                } else {
                    0.0
                }
            })
            .collect();
        Self {
            names,
            data,
            present,
        }
    }

    pub fn get(&self, key: &str) -> Option<&f64> {
        let index = self.names.iter().position(|&name| name == key)?;
        if self.present & (1 << index) == 0 {
            None
        } else {
            self.data.get(index)
        }
    }

    fn iter(&self) -> impl Iterator<Item = (&'static str, f64)> + '_ {
        self.names
            .iter()
            .zip(&self.data)
            .enumerate()
            .filter(|(index, _)| self.present & (1 << index) != 0)
            .map(|(_, (&name, &value))| (name, value))
    }
}

#[derive(Default)]
pub struct Broadcast {
    pub records: BTreeMap<(System, u8), Vec<Navigation>>,
    pub klobuchar: Option<Klobuchar>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Klobuchar {
    pub time: f64,
    pub alpha: [f64; 4],
    pub beta: [f64; 4],
}

pub fn number(text: &str) -> Result<f64> {
    let text = text.trim();
    let normalized = if text.contains(['D', 'd']) {
        Cow::Owned(text.replace(['D', 'd'], "e"))
    } else {
        Cow::Borrowed(text)
    };
    let value: f64 = normalized.parse().context("invalid numeric field")?;
    ensure!(value.is_finite(), "nonfinite numeric field");
    Ok(value)
}

pub fn calendar(parts: &[&str]) -> Result<chrono::DateTime<Utc>> {
    ensure!(parts.len() == 6, "invalid calendar date");
    let seconds = number(parts[5])?;
    ensure!((0.0..60.0).contains(&seconds), "invalid calendar seconds");
    let date = Utc
        .with_ymd_and_hms(
            parts[0].parse()?,
            parts[1].parse()?,
            parts[2].parse()?,
            parts[3].parse()?,
            parts[4].parse()?,
            seconds.trunc() as u32,
        )
        .single()
        .context("invalid calendar date")?;
    Ok(date + chrono::Duration::nanoseconds((seconds.fract() * 1e9).round() as i64))
}

fn klobuchar_record(block: &[&str]) -> Result<Klobuchar> {
    let date = calendar(
        &block[0]
            .get(4..23)
            .context("short RINEX ionosphere epoch")?
            .split_whitespace()
            .collect::<Vec<_>>(),
    )?;
    let field = |line: &str, i: usize| {
        number(
            line.get(4 + i * 19..23 + i * 19)
                .context("short RINEX ionosphere coefficient")?,
        )
    };
    Ok(Klobuchar {
        time: (date.timestamp() - GPS_EPOCH_UNIX) as f64,
        alpha: [
            field(block[0], 1)?,
            field(block[0], 2)?,
            field(block[0], 3)?,
            field(block[1], 0)?,
        ],
        beta: [
            field(block[1], 1)?,
            field(block[1], 2)?,
            field(block[1], 3)?,
            field(block[2], 0)?,
        ],
    })
}

impl Broadcast {
    pub fn add(&mut self, input: &[u8]) -> Result<()> {
        let bytes = decompress(input)?;
        let text = std::str::from_utf8(&bytes).context("RINEX is not UTF-8")?;
        ensure!(text.is_ascii(), "non-ASCII RINEX");
        let lines: Vec<_> = text.lines().collect();
        let first = lines.first().context("empty RINEX")?;
        let version = number(first.get(..9).context("short RINEX header")?)?;
        ensure!(
            (3.0..5.0).contains(&version),
            "RINEX version must be 3 or 4"
        );
        let end = lines
            .iter()
            .position(|l| l.get(60..).unwrap_or("").trim() == "END OF HEADER")
            .context("missing RINEX header terminator")?;
        let (mut header_alpha, mut header_beta) = (None, None);
        for line in &lines[..end] {
            if line.get(60..).unwrap_or("").trim() == "IONOSPHERIC CORR" {
                let key = line.get(..4).context("short ionosphere header")?.trim();
                let mut values = [0.0; 4];
                for (i, value) in values.iter_mut().enumerate() {
                    *value = number(
                        line.get(5 + i * 12..17 + i * 12)
                            .context("short ionosphere coefficient")?,
                    )?;
                }
                match key {
                    "GPSA" => header_alpha = Some(values),
                    "GPSB" => header_beta = Some(values),
                    _ => {}
                }
            }
        }
        let mut index = end + 1;
        let mut wanted = true;
        let mut parsed = 0;
        let mut first_epoch = f64::INFINITY;
        while index < lines.len() {
            let line = lines[index];
            if let Some(header) = line.strip_prefix('>') {
                let parts: Vec<_> = header.split_whitespace().collect();
                if parts.first() == Some(&"ION")
                    && parts.get(1).is_some_and(|id| id.starts_with('G'))
                    && parts.get(2) == Some(&"LNAV")
                {
                    let block = lines
                        .get(index + 1..index + 4)
                        .context("truncated RINEX ionosphere record")?;
                    self.update_klobuchar(klobuchar_record(block)?);
                    wanted = false;
                    index += 4;
                    continue;
                }
                wanted = parts.first() == Some(&"EPH")
                    && parts.get(2).is_some_and(|t| {
                        matches!(*t, "LNAV" | "FDMA" | "INAV" | "FNAV" | "D1" | "D2")
                    });
                index += 1;
                continue;
            }
            if !wanted || line.trim().is_empty() {
                index += 1;
                continue;
            }
            let Some(code) = line.chars().next() else {
                index += 1;
                continue;
            };
            let Some(system) = System::from_code(code) else {
                index += 1;
                continue;
            };
            let svid: u8 = line
                .get(1..3)
                .context("short RINEX satellite")?
                .trim()
                .parse()?;
            ensure!(svid > 0 && svid <= 99, "invalid RINEX satellite");
            let date = calendar(
                &line
                    .get(4..23)
                    .context("short RINEX epoch")?
                    .split_whitespace()
                    .collect::<Vec<_>>(),
            )?;
            let system_epoch = (date.timestamp() - GPS_EPOCH_UNIX) as f64;
            let epoch = match system {
                System::Glonass => Instant(date).gps() as f64,
                System::Bds => system_epoch + 14.0,
                _ => system_epoch,
            };
            let mut values = [None; MAX_NAVIGATION_FIELDS];
            for (i, value) in values.iter_mut().take(3).enumerate() {
                *value = Some(number(
                    line.get(23 + i * 19..42 + i * 19)
                        .context("short RINEX clock")?,
                )?);
            }
            index += 1;
            let max_lines = if system == System::Glonass { 4 } else { 7 };
            let mut body = 0;
            while body < max_lines && index < lines.len() && lines[index].starts_with("    ") {
                for i in 0..4 {
                    let field = lines[index]
                        .get(4 + i * 19..(23 + i * 19).min(lines[index].len()))
                        .unwrap_or("")
                        .trim();
                    values[3 + body * 4 + i] = if field.is_empty() {
                        None
                    } else {
                        Some(number(field)?)
                    };
                }
                body += 1;
                index += 1;
            }
            ensure!(
                body >= if system == System::Glonass { 3 } else { 7 },
                "truncated RINEX orbit"
            );
            let names = schema::names(code).context("unsupported RINEX system")?;
            let values = Values::new(names, &values);
            let nav = Navigation {
                system,
                svid,
                epoch,
                system_epoch,
                values,
            };
            first_epoch = first_epoch.min(epoch);
            let records = self.records.entry((system, svid)).or_default();
            if let Some(old) = records.iter_mut().find(|old| {
                old.epoch == epoch
                    && old.values.get("data_sources") == nav.values.get("data_sources")
            }) {
                *old = nav;
            } else {
                records.push(nav);
            }
            parsed += 1;
        }
        ensure!(parsed > 0, "RINEX has no supported navigation records");
        if let (Some(alpha), Some(beta)) = (header_alpha, header_beta) {
            self.update_klobuchar(Klobuchar {
                time: first_epoch,
                alpha,
                beta,
            });
        }
        for records in self.records.values_mut() {
            records.sort_by(|a, b| a.epoch.total_cmp(&b.epoch));
        }
        Ok(())
    }

    fn update_klobuchar(&mut self, candidate: Klobuchar) {
        if self
            .klobuchar
            .is_none_or(|current| candidate.time >= current.time)
        {
            self.klobuchar = Some(candidate);
        }
    }

    fn fresh(&self, system: System, svid: u8, time: f64) -> impl Iterator<Item = &Navigation> {
        self.records
            .get(&(system, svid))
            .into_iter()
            .flatten()
            .filter(move |r| {
                (r.epoch - time).abs() <= 7200.0
                    && (system != System::Galileo
                        || r.values
                            .get("data_sources")
                            .is_some_and(|v| (*v as u32) & 2 != 0))
            })
    }

    pub fn nearest(&self, system: System, svid: u8, time: f64) -> Option<&Navigation> {
        self.fresh(system, svid, time)
            .min_by(|a, b| (a.epoch - time).abs().total_cmp(&(b.epoch - time).abs()))
    }

    pub fn newest(&self, system: System, svid: u8, time: f64) -> Option<&Navigation> {
        self.fresh(system, svid, time)
            .max_by(|a, b| a.epoch.total_cmp(&b.epoch))
    }
}

impl Navigation {
    pub fn get(&self, key: &str) -> Result<f64> {
        self.values.get(key).copied().with_context(|| {
            format!(
                "{}{:02} missing broadcast field {key}",
                self.system.code(),
                self.svid
            )
        })
    }

    pub fn healthy(&self) -> bool {
        self.values
            .get(match self.system {
                System::Glonass => "health",
                System::Bds => "sath1",
                _ => "sv_health",
            })
            .is_some_and(|v| {
                if self.system == System::Qzs {
                    (*v as u32) & 0x3e == 0
                } else {
                    *v == 0.0
                }
            })
    }

    pub fn toe(&self) -> Result<f64> {
        if self.system == System::Glonass {
            return Ok(self.epoch);
        }
        let tow = self.get("toe")?;
        let delta = (tow - self.system_epoch.rem_euclid(604800.0) + 302400.0).rem_euclid(604800.0)
            - 302400.0;
        Ok(self.epoch + delta)
    }

    pub fn parameters(&self) -> Result<orbit::Parameters> {
        let mut p = [0.0; 15];
        for (i, key) in orbit::PARAMETER_NAMES.iter().enumerate() {
            p[i] = self.get(key)?;
        }
        Ok(p)
    }

    pub fn state(&self, time: f64) -> Result<State> {
        ensure!(
            (time - self.epoch).abs() <= 14_400.0,
            "broadcast extrapolation exceeds four hours"
        );
        if self.system == System::Glonass {
            return self.glonass_state(time);
        }
        let p = self.parameters()?;
        let toe = self.toe()?;
        let tow = self.get("toe")?;
        let geo = orbit::is_geo(self.system, self.svid);
        let pos = |t| orbit::broadcast_position(&p, t - toe, tow, self.system, geo);
        let position = pos(time)?;
        let left = pos(time - 0.5)?;
        let right = pos(time + 0.5)?;
        let dt = time - self.epoch;
        Ok(State {
            position,
            velocity: std::array::from_fn(|i| right[i] - left[i]),
            acceleration: std::array::from_fn(|i| (right[i] - 2.0 * position[i] + left[i]) * 4.0),
            clock: self.get("af0")? + self.get("af1")? * dt + self.get("af2")? * dt * dt,
            drift: self.get("af1")? + 2.0 * self.get("af2")? * dt,
        })
    }

    fn glonass_state(&self, time: f64) -> Result<State> {
        let mut state = [
            self.get("x")? * 1000.0,
            self.get("y")? * 1000.0,
            self.get("z")? * 1000.0,
            self.get("vx")? * 1000.0,
            self.get("vy")? * 1000.0,
            self.get("vz")? * 1000.0,
        ];
        let external = [
            self.get("ax")? * 1000.0,
            self.get("ay")? * 1000.0,
            self.get("az")? * 1000.0,
        ];
        let derivative = |x: [f64; 6]| {
            let a = orbit::glonass_acceleration([x[0], x[1], x[2]], [x[3], x[4], x[5]]);
            [
                x[3],
                x[4],
                x[5],
                a[0] + external[0],
                a[1] + external[1],
                a[2] + external[2],
            ]
        };
        let mut remaining = time - self.epoch;
        while remaining.abs() > 1e-9 {
            let h = remaining.clamp(-30.0, 30.0);
            let k1 = derivative(state);
            let k2 = derivative(std::array::from_fn(|i| state[i] + h * k1[i] / 2.0));
            let k3 = derivative(std::array::from_fn(|i| state[i] + h * k2[i] / 2.0));
            let k4 = derivative(std::array::from_fn(|i| state[i] + h * k3[i]));
            for i in 0..6 {
                state[i] += h * (k1[i] + 2.0 * k2[i] + 2.0 * k3[i] + k4[i]) / 6.0;
            }
            remaining -= h;
        }
        let d = derivative(state);
        Ok(State {
            position: [state[0], state[1], state[2]],
            velocity: [state[3], state[4], state[5]],
            acceleration: [d[3], d[4], d[5]],
            clock: self.get("minus_tau_n")? + self.get("gamma_n")? * (time - self.epoch),
            drift: self.get("gamma_n")?,
        })
    }

    pub fn semicircle_fields(&self) -> BTreeMap<String, f64> {
        let mut values: BTreeMap<String, f64> = self
            .values
            .iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        for key in ["idot", "delta_n", "m0", "omega0", "i0", "omega", "omegadot"] {
            if let Some(value) = values.get_mut(key) {
                *value /= PI;
            }
        }
        values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_fields_preserve_missing_values_and_float_bits() {
        for system in ['G', 'J', 'E', 'C', 'R'] {
            let names = schema::names(system).unwrap();
            let fields = std::array::from_fn(|index| match index % 4 {
                0 => None,
                1 => Some(0.0),
                2 => Some(-0.0),
                _ => Some(index as f64),
            });
            let values = Values::new(names, &fields);
            for (&name, expected) in names.iter().zip(fields) {
                assert_eq!(
                    values.get(name).copied().map(f64::to_bits),
                    expected.map(f64::to_bits)
                );
            }
            assert!(values.get("unknown").is_none());
            let expected: BTreeMap<_, _> = names
                .iter()
                .zip(fields)
                .filter_map(|(&name, value)| value.map(|value| (name, value.to_bits())))
                .collect();
            let actual: BTreeMap<_, _> = values
                .clone()
                .iter()
                .map(|(name, value)| (name, value.to_bits()))
                .collect();
            assert_eq!(actual, expected);
        }
    }

    fn navigation(version: &str, header: &str, body: &str, hour: u32) -> Vec<u8> {
        let mut text = format!(
            "{version:>9}{:51}RINEX VERSION / TYPE\n{header}{:60}END OF HEADER\n{body}",
            "", ""
        );
        if version.starts_with('4') {
            text.push_str("> EPH G01 LNAV\n");
        }
        text.push_str(&format!(
            "G01 2026 09 26 {hour:02} 00 00{:19.12E}{:19.12E}{:19.12E}\n",
            0.0, 0.0, 0.0
        ));
        for _ in 0..7 {
            text.push_str(&format!(
                "    {:19.12E}{:19.12E}{:19.12E}{:19.12E}\n",
                1.0, 1.0, 1.0, 1.0
            ));
        }
        text.into_bytes()
    }

    #[test]
    fn newest_prefers_the_latest_fresh_record_over_the_nearest() -> Result<()> {
        let mut broadcast = Broadcast::default();
        for hour in [5, 6, 9] {
            broadcast.add(&navigation("3.05", "", "", hour))?;
        }
        let at = (Utc
            .with_ymd_and_hms(2026, 9, 26, 5, 20, 0)
            .unwrap()
            .timestamp()
            - GPS_EPOCH_UNIX) as f64;
        let epoch = |nav: Option<&Navigation>| nav.map(|n| n.epoch - at);
        assert_eq!(epoch(broadcast.nearest(System::Gps, 1, at)), Some(-1200.0));
        assert_eq!(epoch(broadcast.newest(System::Gps, 1, at)), Some(2400.0));
        Ok(())
    }

    #[test]
    fn klobuchar_comes_from_the_newest_header_or_rinex_4_record() -> Result<()> {
        let alpha = [1.8626e-8, 1.4901e-8, -1.1921e-7, -1.1921e-7];
        let beta = [1.1469e8, 1.6384e4, -2.6214e5, 6.5536e4];
        let record = format!(
            "> ION G10 LNAV\n    2026 09 26 04 00 00{:19.12E}{:19.12E}{:19.12E}\n    {:19.12E}{:19.12E}{:19.12E}{:19.12E}\n    {:19.12E}{:19.12E}\n",
            alpha[0], alpha[1], alpha[2], alpha[3], beta[0], beta[1], beta[2], beta[3], 0.0
        );
        let header = format!(
            "GPSA {:12.4E}{:12.4E}{:12.4E}{:12.4E}{:7}IONOSPHERIC CORR\nGPSB {:12.4E}{:12.4E}{:12.4E}{:12.4E}{:7}IONOSPHERIC CORR\n",
            1e-8, 0.0, 0.0, 0.0, "", 9e4, 0.0, 0.0, 0.0, ""
        );
        let mut broadcast = Broadcast::default();
        broadcast.add(&navigation("4.01", "", &record, 4))?;
        let rinex_4 = broadcast.klobuchar.context("RINEX 4 ionosphere missing")?;
        assert_eq!(rinex_4.alpha, alpha);
        assert_eq!(rinex_4.beta, beta);
        broadcast.add(&navigation("3.05", &header, "", 0))?;
        assert_eq!(broadcast.klobuchar, Some(rinex_4));
        broadcast.add(&navigation("3.05", &header, "", 6))?;
        let newer = broadcast.klobuchar.context("header ionosphere missing")?;
        assert_eq!(newer.alpha, [1e-8, 0.0, 0.0, 0.0]);
        assert_eq!(newer.beta, [9e4, 0.0, 0.0, 0.0]);
        assert!(newer.time > rinex_4.time);
        Ok(())
    }

    #[test]
    fn numeric_fields_accept_fortran_exponents_and_reject_nonfinite_values() -> Result<()> {
        for field in [" 1.25E+01 ", " 1.25D+01 ", " 1.25d+01 ", " 12.5 "] {
            assert_eq!(number(field)?, 12.5);
        }
        for field in ["NaN", "inf", "-inf", "1e999", "", "1D", "1.2.3"] {
            assert!(number(field).is_err(), "accepted {field:?}");
        }
        Ok(())
    }
}
