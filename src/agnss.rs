use crate::{
    policy::System,
    rinex::{Broadcast, Navigation},
    rtcm::{self, Message},
    time::Instant,
};
use anyhow::{Context, Result, ensure};
use chrono::{Datelike, TimeZone, Timelike, Utc};
use std::collections::BTreeMap;

fn ura(value: f64) -> f64 {
    let nominal = [
        2.4, 3.4, 4.85, 6.85, 9.65, 13.65, 24.0, 48.0, 96.0, 192.0, 384.0, 768.0, 1536.0, 3072.0,
        6144.0,
    ];
    let rtcm = [
        2.0, 2.8, 4.0, 5.7, 8.0, 11.3, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1024.0, 2048.0,
        4096.0,
    ];
    if value < 0.0 {
        return 15.0;
    }
    nominal
        .iter()
        .position(|v| (v - value).abs() < 0.05)
        .or_else(|| rtcm.iter().position(|v| (v - value).abs() < 0.05))
        .or_else(|| nominal.iter().position(|v| value <= *v))
        .unwrap_or(15) as f64
}

fn sisa(v: f64) -> f64 {
    if v < 0.0 {
        255.0
    } else if v <= 0.49 {
        (v / 0.01).round()
    } else if v <= 0.98 {
        50.0 + ((v - 0.5) / 0.02).round()
    } else if v <= 1.96 {
        75.0 + ((v - 1.0) / 0.04).round()
    } else if v <= 6.0 {
        100.0 + ((v - 2.0) / 0.16).round()
    } else {
        255.0
    }
}

pub fn from_navigation(nav: &Navigation) -> Result<Message> {
    let mut v = nav.semicircle_fields();
    let number = match nav.system {
        System::Gps => 1019,
        System::Glonass => 1020,
        System::Bds => 1042,
        System::Galileo => 1046,
        System::Qzs => anyhow::bail!("QZSS RTCM is not part of this format"),
    };
    v.insert("prn".into(), f64::from(nav.svid));
    v.insert("toc".into(), nav.system_epoch.rem_euclid(604800.0));
    match nav.system {
        System::Gps => {
            for (key, value) in [
                ("week", (nav.toe()? / 604800.0).floor().rem_euclid(1024.0)),
                ("code_l2", 1.0),
                ("ura", 0.0),
                ("health", nav.get("sv_health")?),
                (
                    "fit",
                    f64::from(nav.values.get("fit_interval").is_some_and(|v| *v > 4.0)),
                ),
            ] {
                v.insert(key.into(), value);
            }
        }
        System::Galileo => {
            let health = nav.get("sv_health")? as u32;
            for (key, value) in [
                (
                    "week",
                    ((nav.toe()? / 604800.0).floor() - 1024.0).rem_euclid(4096.0),
                ),
                ("sisa", sisa(nav.get("sisa")?)),
                ("bgd_e5b", 0.0),
                ("e5b_health", ((health >> 7) & 3) as f64),
                ("e5b_valid", ((health >> 6) & 1) as f64),
                ("e1b_health", ((health >> 1) & 3) as f64),
                ("e1b_valid", (health & 1) as f64),
            ] {
                v.insert(key.into(), value);
            }
        }
        System::Bds => {
            for (key, value) in [
                ("week", ((nav.toe()? - 14.0) / 604800.0).floor() - 1356.0),
                ("urai", ura(nav.get("sv_accuracy")?)),
                ("aode", nav.get("aode")?.rem_euclid(32.0)),
                ("aodc", nav.get("aodc")?.rem_euclid(32.0)),
                ("tgd2", 0.0),
                ("health", nav.get("sath1")?),
            ] {
                v.insert(key.into(), value);
            }
        }
        System::Glonass => {
            let date = Instant::from_gps(nav.epoch as i64)?.0 + chrono::Duration::hours(3);
            let base_year = 1996 + (date.year() - 1996).div_euclid(4) * 4;
            let base = Utc
                .with_ymd_and_hms(base_year, 1, 1, 0, 0, 0)
                .single()
                .context("GLONASS date out of range")?;
            let nt = (date.date_naive() - base.date_naive()).num_days() + 1;
            for (key, value) in [
                ("slot", f64::from(nav.svid)),
                ("alm_health", 0.0),
                ("alm_health_avail", 0.0),
                ("p1", 1.0),
                ("tk", 0.0),
                ("bn_msb", nav.get("health")?),
                ("p2", 1.0),
                ("tb", date.num_seconds_from_midnight() as f64),
                ("p3", 1.0),
                ("p", 0.0),
                ("ln3", nav.get("health")?),
                ("tau_n", -nav.get("minus_tau_n")?),
                ("dtau_n", *nav.values.get("dtau_n").unwrap_or(&0.0)),
                ("en", nav.get("age")?),
                ("p4", 0.0),
                ("ft", *nav.values.get("urai").unwrap_or(&0.0)),
                ("nt", nt as f64),
                ("m", 1.0),
                ("add_avail", 1.0),
                ("na", (nt - 1).max(1) as f64),
                ("tau_c", 0.0),
                ("n4", ((base_year - 1996) / 4 + 1) as f64),
                ("tau_gps", 0.0),
                ("ln5", nav.get("health")?),
            ] {
                v.insert(key.into(), value);
            }
        }
        System::Qzs => unreachable!(),
    }
    Ok(Message { number, values: v })
}

pub fn ionosphere(broadcast: &Broadcast) -> Option<Message> {
    let klobuchar = broadcast.klobuchar?;
    let mut values = BTreeMap::from([("tag".into(), 6.0)]);
    for (prefix, coefficients) in [("alpha", klobuchar.alpha), ("beta", klobuchar.beta)] {
        for (i, value) in coefficients.into_iter().enumerate() {
            values.insert(format!("{prefix}{i}"), value);
        }
    }
    Some(Message {
        number: 4056,
        values,
    })
}

pub fn build(
    broadcast: &Broadcast,
    systems: &[System],
    at: Instant,
) -> Result<(Vec<u8>, Vec<String>)> {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    let now = at.gps() as f64;
    for system in [System::Gps, System::Glonass, System::Bds, System::Galileo] {
        if !systems.contains(&system) {
            continue;
        }
        let mut ages = Vec::new();
        for id in 1..=system.slots() as u8 {
            if let Some(nav) = broadcast.nearest(system, id, now).filter(|n| n.healthy()) {
                out.extend(rtcm::frame(&from_navigation(nav)?.encode()?)?);
                ages.push(now - nav.epoch);
            }
        }
        ensure!(
            !ages.is_empty(),
            "no fresh healthy {} broadcast ephemerides",
            system.name()
        );
        if system == System::Glonass {
            let (newest, oldest) = ages
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), &age| {
                    (low.min(age), high.max(age))
                });
            notes.push(format!(
                "AGNSS: GLONASS t_b age at build is {newest:.0} s to {oldest:.0} s"
            ));
        }
    }
    match ionosphere(broadcast) {
        Some(message) => out.extend(rtcm::frame(&message.encode()?)?),
        None => notes.push("AGNSS: GPS ionosphere coefficients are absent".into()),
    }
    Ok((out, notes))
}

pub fn validate_fresh(bytes: &[u8], at: Instant) -> Result<()> {
    for payload in rtcm::payloads(bytes)? {
        let m = Message::decode(payload)?;
        let now = at.gps() as f64;
        let dt = match m.number {
            1019 | 1046 => {
                (m.values["toe"] - now.rem_euclid(604800.0) + 302400.0).rem_euclid(604800.0)
                    - 302400.0
            }
            1042 => {
                (m.values["toe"] - (now - 14.0).rem_euclid(604800.0) + 302400.0)
                    .rem_euclid(604800.0)
                    - 302400.0
            }
            1020 => {
                (m.values["tb"]
                    - f64::from((at.0 + chrono::Duration::hours(3)).num_seconds_from_midnight())
                    + 43200.0)
                    .rem_euclid(86400.0)
                    - 43200.0
            }
            _ => continue,
        };
        ensure!(dt.abs() <= 7200.0, "stale AGNSS message {}", m.number);
        if m.number == 1019 || m.number == 1046 || m.number == 1042 {
            let (offset, modulus) = match m.number {
                1019 => (0.0, 1024.0),
                1046 => (1024.0, 4096.0),
                _ => (1356.0, 8192.0),
            };
            let current_week =
                ((now - if m.number == 1042 { 14.0 } else { 0.0 }) / 604800.0).floor() - offset;
            let week = m.values["week"];
            let difference =
                (week - current_week + modulus / 2.0).rem_euclid(modulus) - modulus / 2.0;
            let system_now = now - if m.number == 1042 { 14.0 } else { 0.0 };
            ensure!(
                (difference * 604800.0 + m.values["toe"] - system_now.rem_euclid(604800.0)).abs()
                    <= 7200.0,
                "stale AGNSS week or epoch"
            );
        } else if m.number == 1020 {
            let base_year = 1996 + 4 * (m.values["n4"] as i32 - 1);
            let base = Utc
                .with_ymd_and_hms(base_year, 1, 1, 0, 0, 0)
                .single()
                .context("invalid RTCM GLONASS cycle")?;
            let epoch = base
                + chrono::Duration::days(m.values["nt"] as i64 - 1)
                + chrono::Duration::seconds(m.values["tb"] as i64)
                - chrono::Duration::hours(3);
            ensure!(
                (epoch - at.0).num_seconds().abs() <= 7200,
                "stale GLONASS RTCM day"
            );
        }
    }
    Ok(())
}
