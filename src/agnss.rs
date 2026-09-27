use crate::{
    policy::System,
    rinex::{Broadcast, Klobuchar, Navigation},
    rtcm::{self, Message},
    time::Instant,
};
use anyhow::{Context, Result, ensure};
use chrono::{Datelike, TimeZone, Timelike, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

const KLOBUCHAR_MESSAGE_HEADER: [u8; 3] = [0xfd, 0x80, 0x06];

#[derive(Clone, Debug, Serialize)]
pub struct KlobucharSource {
    pub provider: String,
    pub url: String,
    pub sha256: String,
    pub day: Option<String>,
    pub raw: [i8; 8],
}

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

pub fn ionosphere(klobuchar: &Klobuchar) -> Message {
    let mut values = BTreeMap::from([("tag".into(), 6.0)]);
    for (prefix, coefficients) in [("alpha", klobuchar.alpha), ("beta", klobuchar.beta)] {
        for (i, value) in coefficients.into_iter().enumerate() {
            values.insert(format!("{prefix}{i}"), value);
        }
    }
    Message {
        number: 4056,
        values,
    }
}

pub fn seed_ionosphere(gps_ion: &[u8]) -> Result<Message> {
    let mut payload = KLOBUCHAR_MESSAGE_HEADER.to_vec();
    payload.extend_from_slice(gps_ion.get(..8).context("short gpsIon")?);
    Message::decode(&payload)
}

pub fn raw_coefficients(message: &Message) -> Result<[i8; 8]> {
    let payload = message.encode()?;
    let coefficients: [u8; 8] = payload
        .get(KLOBUCHAR_MESSAGE_HEADER.len()..)
        .context("short message 4056")?
        .try_into()?;
    Ok(coefficients.map(|byte| byte as i8))
}

pub fn daily_file_day(url: &str) -> Option<String> {
    let name = url.rsplit('/').next()?;
    let stamp = name
        .split('_')
        .find(|part| part.len() == 11 && part.bytes().all(|b| b.is_ascii_digit()))?;
    chrono::NaiveDate::from_yo_opt(stamp[..4].parse().ok()?, stamp[4..7].parse().ok()?)
        .map(|day| day.to_string())
}

const TRANSMISSION_LEAD_S: f64 = 7200.0;

fn age_limit(number: u16) -> f64 {
    match number {
        1020 => 1800.0,
        1042 => 5400.0,
        1046 => 14400.0,
        _ => 7200.0,
    }
}

pub fn build(
    broadcast: &Broadcast,
    systems: &[System],
    at: Instant,
    ionosphere: Option<&Message>,
) -> Result<(Vec<u8>, Vec<String>, usize)> {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    let mut omitted = 0;
    let now = at.gps() as f64;
    for system in [System::Gps, System::Glonass, System::Bds, System::Galileo] {
        if !systems.contains(&system) {
            continue;
        }
        let mut ages = Vec::new();
        let mut stale = 0;
        for id in 1..=system.slots() as u8 {
            if let Some(nav) = broadcast.nearest(system, id, now).filter(|n| n.healthy()) {
                let age = now - nav.epoch;
                if system == System::Glonass && age.abs() > age_limit(1020) {
                    stale += 1;
                    continue;
                }
                out.extend(rtcm::frame(&from_navigation(nav)?.encode()?)?);
                ages.push(age);
            }
        }
        omitted += stale;
        if stale > 0 {
            notes.push(format!(
                "AGNSS: {stale} GLONASS ephemerides with t_b more than 30 min from the build are omitted"
            ));
            if ages.is_empty() {
                continue;
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
    match ionosphere {
        Some(message) => out.extend(rtcm::frame(&message.encode()?)?),
        None => notes.push("AGNSS: GPS ionosphere coefficients are absent".into()),
    }
    Ok((out, notes, omitted))
}

pub fn validate_fresh(bytes: &[u8], at: Instant, absolute_dates: bool) -> Result<Vec<String>> {
    let now = at.gps() as f64;
    let moscow = at.0 + chrono::Duration::hours(3);
    let mut gps = 0;
    let mut weeks = BTreeMap::<i64, usize>::new();
    let mut days = BTreeMap::<i64, usize>::new();
    let mut transmission_week = 0;
    let mut anchors = 0;
    let mut week_of_early_toe = 0;
    let build_week = (now / 604800.0).floor();
    for payload in rtcm::payloads(bytes)? {
        let m = Message::decode(payload)?;
        let limit = age_limit(m.number);
        match m.number {
            1019 | 1042 | 1046 => {
                let (offset, modulus, lag) = match m.number {
                    1019 => (0.0, 1024.0, 0.0),
                    1046 => (1024.0, 4096.0, 0.0),
                    _ => (1356.0, 8192.0, 14.0),
                };
                let system_now = now - lag;
                let dt = (m.values["toe"] - system_now.rem_euclid(604800.0) + 302400.0)
                    .rem_euclid(604800.0)
                    - 302400.0;
                ensure!(dt.abs() <= limit, "stale AGNSS message {}", m.number);
                let week = ((system_now + dt) / 604800.0).floor() - offset;
                let difference =
                    (m.values["week"] - week + modulus / 2.0).rem_euclid(modulus) - modulus / 2.0;
                let toe = m.values["toe"];
                let early = toe < TRANSMISSION_LEAD_S;
                if m.number == 1046 && !absolute_dates {
                    ensure!(
                        difference == 0.0 || difference == 1.0,
                        "AGNSS Galileo week {difference:+} from its time of week"
                    );
                    if difference != 0.0 {
                        *weeks.entry(difference as i64).or_default() += 1;
                    }
                } else if m.number == 1019 && !absolute_dates && difference == -1.0 && early {
                    transmission_week += 1;
                } else {
                    ensure!(
                        difference == 0.0,
                        "stale AGNSS week in message {}",
                        m.number
                    );
                    let mid_week =
                        (TRANSMISSION_LEAD_S..=604800.0 - TRANSMISSION_LEAD_S).contains(&toe);
                    anchors += usize::from(m.number == 1042 || (m.number == 1019 && mid_week));
                    week_of_early_toe += usize::from(
                        m.number == 1019 && (toe == 0.0 || (early && week > build_week)),
                    );
                }
                gps += usize::from(m.number == 1019);
            }
            1020 => {
                ensure!(m.values["tb"] <= 95.0 * 900.0, "GLONASS t_b beyond slot 95");
                let dt = (m.values["tb"] - f64::from(moscow.num_seconds_from_midnight()) + 43200.0)
                    .rem_euclid(86400.0)
                    - 43200.0;
                ensure!(dt.abs() <= limit, "stale AGNSS message 1020");
                let date = (moscow + chrono::Duration::seconds(dt as i64)).date_naive();
                let base =
                    chrono::NaiveDate::from_ymd_opt(1996 + 4 * (m.values["n4"] as i32 - 1), 1, 1)
                        .context("invalid RTCM GLONASS cycle")?;
                let difference =
                    (base + chrono::Duration::days(m.values["nt"] as i64 - 1) - date).num_days();
                if absolute_dates {
                    ensure!(difference == 0, "stale GLONASS RTCM day");
                } else {
                    ensure!(
                        (-8..=1).contains(&difference),
                        "AGNSS GLONASS day {difference:+} from its time of day"
                    );
                    if difference != 0 {
                        *days.entry(difference).or_default() += 1;
                    }
                }
            }
            _ => {}
        }
    }
    ensure!(gps > 0, "no GPS ephemeris (1019) in AGNSS");
    let mut notes = Vec::new();
    if transmission_week > 0 {
        ensure!(
            anchors > 0,
            "AGNSS: {transmission_week} GPS ephemerides carry the week before their toe, and no 1042 or mid-week 1019 dates the stream"
        );
        ensure!(
            week_of_early_toe == 0,
            "AGNSS: {transmission_week} GPS ephemerides carry the week before their toe, but {week_of_early_toe} carry the week of an early toe"
        );
        notes.push(format!(
            "AGNSS: {transmission_week} GPS ephemerides carry the transmission week"
        ));
    }
    for (difference, count) in weeks {
        notes.push(format!(
            "AGNSS: {count} Galileo ephemerides carry a week {difference:+} from their time of week"
        ));
    }
    for (difference, count) in days {
        notes.push(format!(
            "AGNSS: {count} GLONASS ephemerides carry a day {difference:+} from their time of day"
        ));
    }
    Ok(notes)
}
