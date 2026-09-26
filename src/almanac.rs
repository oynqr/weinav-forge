use crate::{
    agnss, extra,
    policy::System,
    rinex::{Broadcast, number},
    seed::decompress,
    time::{GRID_STEP, Instant},
};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, f64::consts::PI};

pub type Yuma = BTreeMap<u8, BTreeMap<String, f64>>;

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
    galileo: &[u8],
    broadcast: &Broadcast,
    at: Instant,
) -> Result<(Vec<u8>, Vec<String>)> {
    let mut out = vec![0; extra::LENGTH];
    let mut notes = vec!["EXTRA: BeiDou and GLONASS almanacs, UTC corrections and unsourced integrity fields are absent".into(),"EXTRA: Galileo health packing has not been confirmed with nonzero vendor health flags".into()];
    for (offset, tag) in [(0x18, 1_u32), (0x238, 1), (0x340, 3), (0x448, 2)] {
        out[offset..offset + 4].copy_from_slice(&tag.to_le_bytes());
    }
    if let Some(ion) = agnss::ionosphere(broadcast) {
        out[16..24].copy_from_slice(&ion.encode()?[3..11]);
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
            (full * 604800 + get("Time of Applicability(s)")? as i64 - at.gps()).abs() <= 7 * 86400,
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
    let bytes = decompress(galileo)?;
    let xml = roxmltree::Document::parse(std::str::from_utf8(&bytes)?)?;
    let issue = xml
        .descendants()
        .find(|n| n.has_tag_name("issueDate"))
        .and_then(|n| n.text())
        .context("Galileo issue date missing")?;
    let issue = Instant::parse(issue)?;
    ensure!(
        (issue.gps() - at.gps()).abs() <= 7 * 86400,
        "Galileo almanac is stale"
    );
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
    let issue_week = issue.gps() / 604800 - 1024;
    let full_week = issue_week + (wna - issue_week + 2).rem_euclid(4) - 2;
    out[0xc59] = full_week as u8;
    let units = toa / 600;
    ensure!(
        toa % 600 == 0 && (0..1008).contains(&units),
        "invalid Galileo almanac time"
    );
    if units > 255 {
        out[0xc5a] = 1;
        out[0xc5c..0xc5e].copy_from_slice(&(units as u16).to_le_bytes());
    } else {
        out[0xc5e] = units as u8;
    }
    out[0xc5f] = iod as u8;
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
        let b = 0xc60 + count * 22;
        put(&mut out, b, 2, id - 1.0, 16, false)?;
        for (offset, key, power, bits, signed) in [
            (2, "ecc", -16, 11, false),
            (4, "deltai", -14, 11, true),
            (6, "omegaDot", -33, 11, true),
            (10, "aSqRoot", -9, 13, true),
            (12, "omega0", -15, 16, true),
            (14, "w", -15, 16, true),
            (16, "m0", -15, 16, true),
            (18, "af0", -19, 16, true),
            (20, "af1", -38, 13, true),
        ] {
            put(
                &mut out,
                b + offset,
                2,
                get(sat, key)? / 2_f64.powi(power),
                bits,
                signed,
            )?;
        }
        put(&mut out, b + 8, 1, get(sat, "statusE5a")?, 2, false)?;
        put(
            &mut out,
            b + 9,
            1,
            get(sat, "statusE5b")? + 4.0 * get(sat, "statusE1B")?,
            4,
            false,
        )?;
        count += 1;
    }
    out[0xc58] = count as u8;
    let (start, end) = validity_window(at)?;
    out[0x1858..0x185c].copy_from_slice(&start.to_le_bytes());
    out[0x185c..0x1860].copy_from_slice(&end.to_le_bytes());
    let leap = at.gps() - (at.0.timestamp() - crate::time::GPS_EPOCH_UNIX);
    out[0x1864..0x1868].copy_from_slice(&(leap as u32).to_le_bytes());
    Ok((out, notes))
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
}
