use crate::{
    agnss, extra, orbit,
    policy::System,
    rinex::{Broadcast, Navigation, number},
    seed::decompress,
    time::{GRID_STEP, Instant, WEEK},
};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, f64::consts::PI};

pub type Yuma = BTreeMap<u8, BTreeMap<String, f64>>;

const ALMANAC_AGE_LIMIT_S: i64 = 7 * 86400;
const GALILEO_WEEK_OFFSET: i64 = 1024;
const GALILEO_WNA_VALUES: i64 = 4;
const GALILEO_IOD_VALUES: i64 = 16;
const GALILEO_TOA_STEP_S: i64 = 600;
const GALILEO_TOA_UNITS: i64 = WEEK / GALILEO_TOA_STEP_S;
const GALILEO_SATELLITES: u8 = 36;
const GALILEO_FIRST_SLOT: usize = 0xc60;
const GALILEO_SLOT_BYTES: usize = 22;
const GALILEO_NOMINAL_A: f64 = 29_600_000.0;
const GALILEO_NOMINAL_INCLINATION: f64 = 56.0 / 180.0;
const GALILEO_ANGLE_POWER: i32 = -15;
const GALILEO_HEALTH: &str = "sv_health";
const GALILEO_STATUS_MASK: u32 = 3;
const E1B_STATUS_SHIFT: u32 = 1;
const E5A_STATUS_SHIFT: u32 = 4;
const E5B_STATUS_SHIFT: u32 = 7;
const GALILEO_FIELDS: [(usize, &str, i32, u32, bool); 9] = [
    (2, "ecc", -16, 11, false),
    (4, "deltai", -14, 11, true),
    (6, "omegaDot", -33, 11, true),
    (10, "aSqRoot", -9, 13, true),
    (12, "omega0", GALILEO_ANGLE_POWER, 16, true),
    (14, "w", GALILEO_ANGLE_POWER, 16, true),
    (16, "m0", GALILEO_ANGLE_POWER, 16, true),
    (18, "af0", -19, 16, true),
    (20, "af1", -38, 13, true),
];

pub fn galileo_issue(galileo: &[u8]) -> Result<Instant> {
    let bytes = decompress(galileo)?;
    let xml = roxmltree::Document::parse(std::str::from_utf8(&bytes)?)?;
    let issue = xml
        .descendants()
        .find(|n| n.has_tag_name("issueDate"))
        .and_then(|n| n.text())
        .context("Galileo issue date missing")?;
    Instant::parse(issue)
}

pub fn yuma(input: &[u8]) -> Result<Yuma> {
    let bytes = decompress(input)?;
    let text = std::str::from_utf8(&bytes)?;
    let mut result = BTreeMap::new();
    let mut current = None;
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = number(value)?;
        if name == "ID" {
            ensure!(
                value.fract() == 0.0 && (1.0..=202.0).contains(&value),
                "invalid YUMA satellite"
            );
            let id = value as u8;
            ensure!(
                result.insert(id, BTreeMap::new()).is_none(),
                "duplicate YUMA satellite"
            );
            current = Some(id);
        } else if let Some(id) = current {
            result.get_mut(&id).unwrap().insert(name.into(), value);
        }
    }
    ensure!(!result.is_empty(), "empty YUMA almanac");
    Ok(result)
}

fn put(
    out: &mut [u8],
    offset: usize,
    bytes: usize,
    value: f64,
    bits: u32,
    signed: bool,
) -> Result<()> {
    let raw = value.round_ties_even();
    let low = if signed { -(1_i64 << (bits - 1)) } else { 0 };
    let high = (1_i64 << (bits - u32::from(signed))) - 1;
    ensure!(
        raw.is_finite() && raw >= low as f64 && raw <= high as f64,
        "almanac value out of range at {offset:#x}"
    );
    out[offset..offset + bytes].copy_from_slice(&(raw as i64).to_le_bytes()[..bytes]);
    Ok(())
}

pub fn build(
    gps: &[u8],
    galileo: Option<&[u8]>,
    broadcast: &Broadcast,
    ionosphere: Option<&crate::rtcm::Message>,
    at: Instant,
) -> Result<(Vec<u8>, Vec<String>)> {
    let mut out = vec![0; extra::LENGTH];
    let mut notes = vec!["EXTRA: BeiDou and GLONASS almanacs, UTC corrections and unsourced integrity fields are absent".into(),"EXTRA: Galileo health packing has not been confirmed with nonzero vendor health flags".into()];
    for (offset, tag) in [(0x18, 1_u32), (0x238, 1), (0x340, 3), (0x448, 2)] {
        out[offset..offset + 4].copy_from_slice(&tag.to_le_bytes());
    }
    if let Some(ion) = ionosphere {
        out[16..24].copy_from_slice(&agnss::raw_coefficients(ion)?.map(|raw| raw as u8));
    } else {
        notes.push("EXTRA: GPS ionosphere coefficients are absent".into());
    }
    let channels: Vec<_> = (1..=24)
        .filter_map(|id| broadcast.nearest(System::Glonass, id, at.gps() as f64))
        .collect();
    out[28] = channels.len() as u8;
    for (i, nav) in channels.iter().enumerate() {
        let frequency = nav.get("freq")?;
        ensure!(
            (-7.0..=6.0).contains(&frequency) && frequency.fract() == 0.0,
            "invalid GLONASS frequency"
        );
        out[29 + i * 4..33 + i * 4].copy_from_slice(&[
            nav.svid - 1,
            0xc0,
            1,
            frequency as i8 as u8,
        ]);
    }
    let yuma = yuma(gps)?;
    ensure!(yuma.len() <= 32, "too many GPS almanacs");
    let mut week = None;
    for (i, (&id, values)) in yuma.iter().enumerate() {
        ensure!(id <= 32, "invalid GPS almanac satellite");
        let get = |key: &str| {
            values
                .get(key)
                .copied()
                .with_context(|| format!("YUMA field {key} missing"))
        };
        let current = get("week")? as i64;
        let full = at.gps() / 604800 + (current - at.gps() / 604800 + 512).rem_euclid(1024) - 512;
        ensure!(
            (full * 604800 + get("Time of Applicability(s)")? as i64 - at.gps()).abs()
                <= ALMANAC_AGE_LIMIT_S,
            "GPS almanac is stale"
        );
        ensure!(week.is_none_or(|w| w == full), "mixed GPS almanac weeks");
        week = Some(full);
        let b = 0x554 + i * 32;
        put(&mut out, b, 2, f64::from(id - 1), 16, false)?;
        for (offset, width, key, scale, bits, signed, bias) in [
            (2, 2, "Eccentricity", 2_f64.powi(-21), 16, false, 0.0),
            (4, 2, "Time of Applicability(s)", 4096.0, 8, false, 0.0),
            (
                6,
                2,
                "Orbital Inclination(rad)",
                PI * 2_f64.powi(-19),
                16,
                true,
                0.3 * PI,
            ),
            (
                8,
                2,
                "Rate of Right Ascen(r/s)",
                PI * 2_f64.powi(-38),
                16,
                true,
                0.0,
            ),
            (10, 2, "Health", 1.0, 8, false, 0.0),
            (12, 4, "SQRT(A)  (m 1/2)", 2_f64.powi(-11), 24, false, 0.0),
            (
                16,
                4,
                "Right Ascen at Week(rad)",
                PI * 2_f64.powi(-23),
                24,
                true,
                0.0,
            ),
            (
                20,
                4,
                "Argument of Perigee(rad)",
                PI * 2_f64.powi(-23),
                24,
                true,
                0.0,
            ),
            (24, 4, "Mean Anom(rad)", PI * 2_f64.powi(-23), 24, true, 0.0),
            (28, 2, "Af0(s)", 2_f64.powi(-20), 11, true, 0.0),
            (30, 2, "Af1(s/s)", 2_f64.powi(-38), 11, true, 0.0),
        ] {
            put(
                &mut out,
                b + offset,
                width,
                (get(key)? - bias) / scale,
                bits,
                signed,
            )?;
        }
    }
    out[0x550] = week.context("GPS almanac week missing")? as u8;
    out[0x551] = yuma.len() as u8;
    let gsc = match galileo {
        Some(galileo) => {
            let issue = galileo_issue(galileo)?;
            if (issue.gps() - at.gps()).abs() <= ALMANAC_AGE_LIMIT_S {
                Ok((galileo, issue))
            } else {
                Err("is more than 7 days from the build time")
            }
        }
        None => Err("is not available"),
    };
    match gsc {
        Ok((galileo, issue)) => gsc_galileo(&mut out, &mut notes, galileo, issue)?,
        Err(reason) => {
            notes.push(format!(
                "EXTRA: Galileo almanac derived from broadcast ephemerides; the GSC almanac {reason}"
            ));
            broadcast_galileo(&mut out, &mut notes, broadcast, at)?;
        }
    }
    let (start, end) = validity_window(at)?;
    out[0x1858..0x185c].copy_from_slice(&start.to_le_bytes());
    out[0x185c..0x1860].copy_from_slice(&end.to_le_bytes());
    let leap = at.gps() - (at.0.timestamp() - crate::time::GPS_EPOCH_UNIX);
    out[0x1864..0x1868].copy_from_slice(&(leap as u32).to_le_bytes());
    Ok((out, notes))
}

fn galileo_reference(
    out: &mut [u8],
    reference: Instant,
    toa: i64,
    iod: i64,
    wna: i64,
) -> Result<()> {
    let issue_week = reference.gps() / WEEK - GALILEO_WEEK_OFFSET;
    let half = GALILEO_WNA_VALUES / 2;
    let full_week = issue_week + (wna - issue_week + half).rem_euclid(GALILEO_WNA_VALUES) - half;
    out[0xc59] = full_week as u8;
    let units = toa / GALILEO_TOA_STEP_S;
    ensure!(
        toa % GALILEO_TOA_STEP_S == 0 && (0..GALILEO_TOA_UNITS).contains(&units),
        "invalid Galileo almanac time"
    );
    if units > 255 {
        out[0xc5a] = 1;
        out[0xc5c..0xc5e].copy_from_slice(&(units as u16).to_le_bytes());
    } else {
        out[0xc5e] = units as u8;
    }
    out[0xc5f] = iod as u8;
    Ok(())
}

fn pack_galileo(
    out: &mut [u8],
    b: usize,
    id: f64,
    get: impl Fn(&str) -> Result<f64>,
) -> Result<()> {
    put(out, b, 2, id - 1.0, 16, false)?;
    for (offset, key, power, bits, signed) in GALILEO_FIELDS {
        put(
            out,
            b + offset,
            2,
            get(key)? / 2_f64.powi(power),
            bits,
            signed,
        )?;
    }
    put(out, b + 8, 1, get("statusE5a")?, 2, false)?;
    put(
        out,
        b + 9,
        1,
        get("statusE5b")? + 4.0 * get("statusE1B")?,
        4,
        false,
    )
}

fn gsc_galileo(
    out: &mut [u8],
    notes: &mut Vec<String>,
    galileo: &[u8],
    issue: Instant,
) -> Result<()> {
    let bytes = decompress(galileo)?;
    let xml = roxmltree::Document::parse(std::str::from_utf8(&bytes)?)?;
    let satellites: Vec<_> = xml
        .descendants()
        .filter(|n| n.has_tag_name("svAlmanac"))
        .collect();
    ensure!(
        !satellites.is_empty() && satellites.len() <= 36,
        "invalid Galileo almanac count"
    );
    let get = |node: roxmltree::Node<'_, '_>, name: &str| -> Result<f64> {
        number(
            node.descendants()
                .find(|n| n.has_tag_name(name))
                .and_then(|n| n.text())
                .with_context(|| format!("Galileo almanac {name} missing"))?,
        )
    };
    let mut groups = BTreeMap::<(i64, i64, i64), usize>::new();
    for &sat in &satellites {
        *groups
            .entry((
                get(sat, "t0a")? as i64,
                get(sat, "iod")? as i64,
                get(sat, "wna")? as i64,
            ))
            .or_default() += 1;
    }
    let (&(toa, iod, wna), _) = groups
        .iter()
        .max_by_key(|(_, count)| *count)
        .context("Galileo reference time missing")?;
    galileo_reference(out, issue, toa, iod, wna)?;
    let mut count = 0;
    let mut ids = std::collections::BTreeSet::new();
    for sat in satellites {
        if (
            get(sat, "t0a")? as i64,
            get(sat, "iod")? as i64,
            get(sat, "wna")? as i64,
        ) != (toa, iod, wna)
        {
            notes.push("EXTRA: omitted Galileo almanac with a different reference time".into());
            continue;
        }
        let id = get(sat, "SVID")?;
        ensure!(
            id.fract() == 0.0 && (1.0..=36.0).contains(&id) && ids.insert(id as u8),
            "invalid Galileo satellite"
        );
        pack_galileo(
            out,
            GALILEO_FIRST_SLOT + count * GALILEO_SLOT_BYTES,
            id,
            |key| get(sat, key),
        )?;
        count += 1;
    }
    out[0xc58] = count as u8;
    Ok(())
}

fn semicircles(radians: f64) -> f64 {
    let half_step = 2_f64.powi(GALILEO_ANGLE_POWER - 1);
    (radians / PI + 1.0 + half_step).rem_euclid(2.0) - 1.0 - half_step
}

fn galileo_health(broadcast: &Broadcast, nav: &Navigation) -> Result<u32> {
    let (toe, iod) = (nav.get("toe")?, nav.get("iodnav")?);
    let mut health = nav.get(GALILEO_HEALTH)? as u32;
    for copy in broadcast
        .records
        .get(&(System::Galileo, nav.svid))
        .into_iter()
        .flatten()
        .filter(|copy| copy.values.get("toe") == Some(&toe))
        .filter(|copy| copy.values.get("iodnav") == Some(&iod))
    {
        health |= copy.get(GALILEO_HEALTH)? as u32;
    }
    Ok(health)
}

fn derive_galileo(
    nav: &Navigation,
    health: u32,
    reference: f64,
    toa: f64,
) -> Result<BTreeMap<&'static str, f64>> {
    let dt = reference - nav.toe()?;
    let clock = reference - nav.epoch;
    let sqrt_a = nav.get("sqrt_a")?;
    let motion = orbit::gravity(System::Galileo).sqrt() / sqrt_a.powi(3) + nav.get("delta_n")?;
    let rate = nav.get("omegadot")?;
    let earth = orbit::earth_rate(System::Galileo);
    let node = nav.get("omega0")? + (rate - earth) * dt - earth * nav.get("toe")?;
    let (af1, af2) = (nav.get("af1")?, nav.get("af2")?);
    let status = |shift: u32| f64::from((health >> shift) & GALILEO_STATUS_MASK);
    Ok(BTreeMap::from([
        ("aSqRoot", sqrt_a - GALILEO_NOMINAL_A.sqrt()),
        ("ecc", nav.get("ecc")?),
        (
            "deltai",
            (nav.get("i0")? + nav.get("idot")? * dt) / PI - GALILEO_NOMINAL_INCLINATION,
        ),
        ("omega0", semicircles(node + earth * toa)),
        ("omegaDot", rate / PI),
        ("w", semicircles(nav.get("omega")?)),
        ("m0", semicircles(nav.get("m0")? + motion * dt)),
        ("af0", nav.get("af0")? + af1 * clock + af2 * clock * clock),
        ("af1", af1 + 2.0 * af2 * clock),
        ("statusE1B", status(E1B_STATUS_SHIFT)),
        ("statusE5a", status(E5A_STATUS_SHIFT)),
        ("statusE5b", status(E5B_STATUS_SHIFT)),
    ]))
}

fn broadcast_galileo(
    out: &mut [u8],
    notes: &mut Vec<String>,
    broadcast: &Broadcast,
    at: Instant,
) -> Result<()> {
    let now = at.gps();
    let week_start = now.div_euclid(WEEK) * WEEK;
    let toa = (now - week_start).div_euclid(GALILEO_TOA_STEP_S) * GALILEO_TOA_STEP_S;
    let wna = (now.div_euclid(WEEK) - GALILEO_WEEK_OFFSET).rem_euclid(GALILEO_WNA_VALUES);
    let iod = (toa / GALILEO_TOA_STEP_S).rem_euclid(GALILEO_IOD_VALUES);
    galileo_reference(out, at, toa, iod, wna)?;
    let reference = (week_start + toa) as f64;
    let mut count = 0;
    for id in 1..=GALILEO_SATELLITES {
        let Some(nav) = broadcast.nearest_toe(System::Galileo, id, now as f64) else {
            continue;
        };
        let b = GALILEO_FIRST_SLOT + count * GALILEO_SLOT_BYTES;
        let packed = galileo_health(broadcast, nav)
            .and_then(|health| derive_galileo(nav, health, reference, toa as f64))
            .and_then(|values| {
                pack_galileo(out, b, f64::from(id), |key| {
                    values
                        .get(key)
                        .copied()
                        .with_context(|| format!("Galileo almanac {key} missing"))
                })
            });
        if let Err(error) = packed {
            out[b..b + GALILEO_SLOT_BYTES].fill(0);
            notes.push(format!(
                "EXTRA: omitted Galileo almanac of E{id:02} derived from broadcast ephemerides: {error:#}"
            ));
        } else {
            count += 1;
        }
    }
    out[0xc58] = count as u8;
    if count == 0 {
        notes.push("EXTRA: Galileo almanac is absent".into());
    }
    Ok(())
}

fn validity_window(at: Instant) -> Result<(u32, u32)> {
    let start = u32::try_from(at.gps().div_euclid(GRID_STEP) * GRID_STEP)?;
    Ok((start, start + 270_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_extra_window_brackets_every_shipped_epoch() -> Result<()> {
        for gps in [1_474_436_605, 1_472_313_600, 1_472_320_799] {
            let at = Instant::from_gps(gps)?;
            let (start, end) = validity_window(at)?;
            assert_eq!(i64::from(start), at.grid(System::Glonass)[0] - 3600);
            for system in System::ALL {
                let grid = at.grid(system);
                assert!(i64::from(start) <= grid[0], "{system:?}");
                assert!(
                    grid[grid.len() - 1] + GRID_STEP <= i64::from(end),
                    "{system:?}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn galileo_issue_reads_the_issue_date() -> Result<()> {
        let xml = b"<signalData><header><GAL-header><issueDate>2026-10-06T09:59:59.0Z</issueDate></GAL-header></header></signalData>";
        assert_eq!(
            galileo_issue(xml)?.0,
            Instant::parse("2026-10-06T09:59:59Z")?.0
        );
        assert!(galileo_issue(b"<signalData/>").is_err());
        Ok(())
    }

    const E02: [f64; 28] = [
        7.328297942877e-05,
        2.685851541173e-12,
        0.0,
        47.0,
        35.25,
        3.336567552860e-09,
        1.732023673967,
        1.566484570503e-06,
        3.727787407115e-04,
        7.765367627144e-06,
        5.440617685318e+03,
        561600.0,
        2.793967723846e-08,
        -2.565962105844,
        -1.862645149231e-08,
        9.613966353774e-01,
        176.6875,
        2.527052355817e-01,
        -5.556660028623e-09,
        -3.107272287506e-10,
        258.0,
        2438.0,
        0.0,
        3.12,
        0.0,
        -2.328306436539e-09,
        0.0,
        999900000.0,
    ];
    const E14: [f64; 28] = [
        5.107504315674e-03,
        1.735003252179e-10,
        0.0,
        47.0,
        180.5,
        4.426255799892e-09,
        -2.885366158099,
        6.379559636116e-06,
        1.687930042390e-01,
        1.243315637112e-05,
        5.289395132065e+03,
        561600.0,
        -2.758577466011e-06,
        -1.704488923925,
        -1.467764377594e-06,
        8.510350361338e-01,
        90.96875,
        -3.068524354505,
        -6.510271178926e-09,
        -5.821671067396e-11,
        258.0,
        2438.0,
        0.0,
        -1.0,
        16.0,
        -2.328306436539e-09,
        0.0,
        999900000.0,
    ];
    const TOE: usize = 11;
    const DATA_SOURCES: usize = 20;
    const HEALTH: usize = 24;
    const INAV: f64 = 517.0;
    const EPOCH: &str = "2026 10 03 12 00 00";
    const BUILD: &str = "2026-10-03T12:47:00Z";
    const SECOND_ORDER_MARGIN: f64 = 1.01;

    fn record(svid: u8, epoch: &str, fields: &[f64; 28]) -> String {
        let mut text = format!("E{svid:02} {epoch}");
        for (index, value) in fields.iter().enumerate() {
            if index >= 3 && (index - 3) % 4 == 0 {
                text.push_str("\n    ");
            }
            text.push_str(&format!("{value:19.12E}"));
        }
        text.push('\n');
        text
    }

    fn navigation(records: &[String]) -> Result<Broadcast> {
        let mut broadcast = Broadcast::default();
        broadcast.add(
            format!(
                "{:>9}{:51}RINEX VERSION / TYPE\n{:60}END OF HEADER\n{}",
                "3.05",
                "",
                "",
                records.concat()
            )
            .as_bytes(),
        )?;
        Ok(broadcast)
    }

    fn reference(at: Instant) -> (f64, i64) {
        let toa = at.gps().rem_euclid(WEEK) / GALILEO_TOA_STEP_S * GALILEO_TOA_STEP_S;
        ((at.gps() - at.gps().rem_euclid(WEEK) + toa) as f64, toa)
    }

    #[test]
    fn angles_near_half_a_turn_fit_the_sixteen_bit_range() -> Result<()> {
        let mut out = [0; 2];
        let step = PI * 2_f64.powi(GALILEO_ANGLE_POWER);
        for radians in [
            PI,
            -PI,
            PI - step / 4.0,
            PI - step / 2.0,
            -PI - step / 2.0,
            3.0 * PI - 1e-12,
            0.0,
        ] {
            let value = semicircles(radians);
            assert!((-1.0..1.0).contains(&(value + 2_f64.powi(GALILEO_ANGLE_POWER - 1))));
            put(
                &mut out,
                0,
                2,
                value / 2_f64.powi(GALILEO_ANGLE_POWER),
                16,
                true,
            )?;
            let wrapped = f64::from(i16::from_le_bytes(out)) * step;
            assert!(
                orbit::angle(wrapped - radians).abs() <= step / 2.0,
                "{radians}"
            );
        }
        Ok(())
    }

    #[test]
    fn derived_almanac_follows_the_broadcast_orbit_across_a_week_boundary() -> Result<()> {
        let mut saturday_night = E02;
        saturday_night[TOE] = 604_200.0;
        for (epoch, fields, at) in [
            (EPOCH, E02, BUILD),
            (
                "2026 10 03 23 50 00",
                saturday_night,
                "2026-10-04T00:30:00Z",
            ),
        ] {
            let broadcast = navigation(&[record(2, epoch, &fields)])?;
            let at = Instant::parse(at)?;
            let nav = broadcast
                .nearest_toe(System::Galileo, 2, at.gps() as f64)
                .context("E02 record missing")?;
            let (reference, toa) = reference(at);
            let values = derive_galileo(nav, 0, reference, toa as f64)?;
            let almanac: orbit::Parameters = [
                values["aSqRoot"] + GALILEO_NOMINAL_A.sqrt(),
                values["ecc"],
                (values["deltai"] + GALILEO_NOMINAL_INCLINATION) * PI,
                values["omega0"] * PI,
                values["w"] * PI,
                values["m0"] * PI,
                0.0,
                values["omegaDot"] * PI,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
            ];
            let a = nav.get("sqrt_a")?.powi(2);
            let periodic = nav.get("crc")?.abs()
                + nav.get("crs")?.abs()
                + a * ["cuc", "cus", "cic", "cis"]
                    .iter()
                    .map(|key| nav.get(key).map(f64::abs))
                    .sum::<Result<f64>>()?;
            let drift = a
                * (nav.get("delta_n")?.abs() + nav.get("idot")?.abs())
                * (1.0 + nav.get("ecc")?).powi(3);
            let parameters = nav.parameters()?;
            for elapsed in [0.0, 3.0 * 86400.0] {
                let time = reference + elapsed;
                let truth = orbit::broadcast_position(
                    &parameters,
                    time - nav.toe()?,
                    nav.get("toe")?,
                    System::Galileo,
                    false,
                )?;
                let derived = orbit::broadcast_position(
                    &almanac,
                    time - reference,
                    toa as f64,
                    System::Galileo,
                    false,
                )?;
                let error = truth
                    .iter()
                    .zip(derived)
                    .map(|(x, y)| (x - y).powi(2))
                    .sum::<f64>()
                    .sqrt();
                let bound = SECOND_ORDER_MARGIN * (periodic + drift * elapsed);
                assert!(
                    error <= bound,
                    "{epoch} + {elapsed} s: {error} m > {bound} m"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn derived_almanac_omits_eccentric_orbits_and_merges_both_health_messages() -> Result<()> {
        let mut inav = E02;
        inav[DATA_SOURCES] = INAV;
        inav[HEALTH] = 130.0;
        let broadcast = navigation(&[
            record(2, EPOCH, &E02),
            record(2, EPOCH, &inav),
            record(14, EPOCH, &E14),
        ])?;
        let at = Instant::parse(BUILD)?;
        let mut out = vec![0; extra::LENGTH];
        let mut notes = Vec::new();
        broadcast_galileo(&mut out, &mut notes, &broadcast, at)?;
        assert_eq!(out[0xc58], 1);
        assert_eq!(out[0xc59], 134);
        assert_eq!(out[0xc5a], 1);
        assert_eq!(u16::from_le_bytes([out[0xc5c], out[0xc5d]]), 940);
        assert_eq!(out[0xc5f], 12);
        let first = GALILEO_FIRST_SLOT;
        assert_eq!(out[first..first + 2], [1, 0]);
        assert_eq!(out[first + 8..first + 10], [0, 5]);
        let (reference, toa) = reference(at);
        let nav = broadcast
            .nearest_toe(System::Galileo, 2, at.gps() as f64)
            .context("E02 record missing")?;
        let values = derive_galileo(nav, 0, reference, toa as f64)?;
        for (offset, key, power, _, _) in GALILEO_FIELDS {
            let raw = i16::from_le_bytes([out[first + offset], out[first + offset + 1]]);
            let expected = values[key] / 2_f64.powi(power);
            assert!((f64::from(raw) - expected).abs() <= 0.5, "{key}");
        }
        let second = first + GALILEO_SLOT_BYTES;
        assert!(
            out[second..second + GALILEO_SLOT_BYTES]
                .iter()
                .all(|&b| b == 0)
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].starts_with("EXTRA: omitted Galileo almanac of E14 derived"),
            "{notes:?}"
        );
        Ok(())
    }

    fn yuma() -> Vec<u8> {
        [
            ("ID", "01"),
            ("Health", "000"),
            ("Eccentricity", "0.01"),
            ("Time of Applicability(s)", "589824.0000"),
            ("Orbital Inclination(rad)", "0.96"),
            ("Rate of Right Ascen(r/s)", "-8.0E-09"),
            ("SQRT(A)  (m 1/2)", "5153.6"),
            ("Right Ascen at Week(rad)", "1.0"),
            ("Argument of Perigee(rad)", "0.5"),
            ("Mean Anom(rad)", "0.2"),
            ("Af0(s)", "1.0E-04"),
            ("Af1(s/s)", "0.0"),
            ("week", "390"),
        ]
        .map(|(name, value)| format!("{name}: {value}\n"))
        .concat()
        .into_bytes()
    }

    fn gsc(issue: &str) -> Vec<u8> {
        let almanac = [
            ("aSqRoot", "0.03515625"),
            ("ecc", "0.0003662109375"),
            ("deltai", "-0.00506591796875"),
            ("omega0", "-0.816619873046875"),
            ("omegaDot", "-1.746229827404023817150209e-09"),
            ("w", "0.084564208984375"),
            ("m0", "0.80615234375"),
            ("af0", "7.2479248046875e-05"),
            ("af1", "3.63797880709171295166015625e-12"),
            ("iod", "10"),
            ("t0a", "466800"),
            ("wna", "2"),
            ("statusE5a", "0"),
            ("statusE5b", "0"),
            ("statusE1B", "0"),
        ]
        .map(|(name, value)| format!("<{name}>{value}</{name}>"))
        .concat();
        format!(
            "<signalData><header><GAL-header><issueDate>{issue}</issueDate></GAL-header></header><body><Almanacs><svAlmanac><SVID>05</SVID>{almanac}</svAlmanac></Almanacs></body></signalData>"
        )
        .into_bytes()
    }

    #[test]
    fn a_stale_or_missing_gsc_almanac_falls_back_to_broadcast() -> Result<()> {
        let at = Instant::parse(BUILD)?;
        let broadcast = navigation(&[record(2, EPOCH, &E02)])?;
        let derived = |reason: &str| {
            format!(
                "EXTRA: Galileo almanac derived from broadcast ephemerides; the GSC almanac {reason}"
            )
        };
        let (missing, missing_notes) = build(&yuma(), None, &broadcast, None, at)?;
        assert_eq!(missing[0xc58], 1);
        assert_eq!(missing[GALILEO_FIRST_SLOT], 1);
        assert!(missing_notes.contains(&derived("is not available")));
        let stale_file = gsc("2026-09-26T00:00:00.0Z");
        let (stale, stale_notes) = build(&yuma(), Some(&stale_file), &broadcast, None, at)?;
        assert_eq!(stale, missing);
        assert!(stale_notes.contains(&derived("is more than 7 days from the build time")));
        let fresh_file = gsc("2026-10-02T09:59:59.0Z");
        let (fresh, fresh_notes) = build(&yuma(), Some(&fresh_file), &broadcast, None, at)?;
        assert_eq!(fresh[0xc58], 1);
        assert_eq!(fresh[GALILEO_FIRST_SLOT], 4);
        assert_eq!(fresh[0xc5f], 10);
        assert!(!fresh_notes.iter().any(|note| note.contains("derived")));
        let (empty, empty_notes) = build(&yuma(), None, &Broadcast::default(), None, at)?;
        assert_eq!(empty[0xc58], 0);
        assert!(empty_notes.contains(&"EXTRA: Galileo almanac is absent".to_owned()));
        Ok(())
    }
}
