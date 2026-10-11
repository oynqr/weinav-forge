use crate::{
    cache,
    rinex::{calendar, number},
    seed::decompress,
};
use anyhow::{Context, Result, ensure};
use chrono::{Datelike, Timelike};
use std::collections::BTreeSet;

const MERGED_VERSION: &str = "3.04";
const HEADER_CONTENT_WIDTH: usize = 60;
const FIELD_WIDTH: usize = 19;
const CLOCK_FIELDS: usize = 3;
const ORBIT_FIELDS_PER_LINE: usize = 4;
const RECORD_PREFIX_WIDTH: usize = 23;
const ORBIT_PREFIX_WIDTH: usize = 4;
const HASH_PREFIX: usize = 22;
const KEPLER_RECORD_FIELDS: usize = 31;
const GLONASS_RINEX_304_FIELDS: usize = 15;

pub struct Merged {
    pub bytes: Vec<u8>,
    pub stations: usize,
}

fn kept_fields(system: char) -> Option<usize> {
    match system {
        'G' | 'E' | 'C' | 'J' => Some(KEPLER_RECORD_FIELDS),
        'R' => Some(GLONASS_RINEX_304_FIELDS),
        _ => None,
    }
}

fn orbit_lines(fields: usize) -> usize {
    (fields - CLOCK_FIELDS).div_ceil(ORBIT_FIELDS_PER_LINE)
}

fn rinex_number(value: f64) -> Result<String> {
    let text = format!("{value:.12E}");
    let (mantissa, exponent) = text.split_once('E').context("invalid exponent")?;
    let exponent: i32 = exponent.parse()?;
    ensure!(exponent.abs() < 100, "exponent out of RINEX range");
    let sign = if exponent < 0 { '-' } else { '+' };
    Ok(format!(
        "{:>FIELD_WIDTH$}",
        format!("{mantissa}E{sign}{:02}", exponent.abs())
    ))
}

fn slot(line: &str, start: usize, index: usize) -> Result<f64> {
    let begin = start + index * FIELD_WIDTH;
    let text = line
        .get(begin..(begin + FIELD_WIDTH).min(line.len()))
        .unwrap_or("")
        .trim();
    if text.is_empty() {
        Ok(0.0)
    } else {
        number(text)
    }
}

fn canonical(block: &[&str]) -> Result<Option<(String, String)>> {
    let first = block[0];
    let system = first.chars().next().context("empty record")?;
    let Some(fields) = kept_fields(system) else {
        return Ok(None);
    };
    let lines = orbit_lines(fields);
    ensure!(block.len() > lines, "truncated record");
    let svid: u8 = first.get(1..3).context("short record")?.trim().parse()?;
    ensure!((1..=99).contains(&svid), "invalid satellite");
    let parts: Vec<_> = first
        .get(4..RECORD_PREFIX_WIDTH)
        .context("short epoch")?
        .split_whitespace()
        .collect();
    let date = calendar(&parts)?;
    let epoch = format!(
        "{:04} {:02} {:02} {:02} {:02} {:02}",
        date.year(),
        date.month(),
        date.day(),
        date.hour(),
        date.minute(),
        date.second()
    );
    let key = format!("{system}{svid:02} {epoch}");
    let clock_present = (0..CLOCK_FIELDS).all(|index| {
        let begin = RECORD_PREFIX_WIDTH + index * FIELD_WIDTH;
        first
            .get(begin..(begin + FIELD_WIDTH).min(first.len()))
            .is_some_and(|text| !text.trim().is_empty())
    });
    ensure!(clock_present, "clock field missing");
    let mut text = key.clone();
    for index in 0..CLOCK_FIELDS {
        text.push_str(&rinex_number(slot(first, RECORD_PREFIX_WIDTH, index)?)?);
    }
    let mut remaining = fields - CLOCK_FIELDS;
    for line in &block[1..=lines] {
        ensure!(line.starts_with("    "), "invalid continuation line");
        text.push('\n');
        text.push_str(&" ".repeat(ORBIT_PREFIX_WIDTH));
        for index in 0..remaining.min(ORBIT_FIELDS_PER_LINE) {
            text.push_str(&rinex_number(slot(line, ORBIT_PREFIX_WIDTH, index)?)?);
        }
        remaining = remaining.saturating_sub(ORBIT_FIELDS_PER_LINE);
    }
    Ok(Some((key, text)))
}

fn records(text: &str) -> Option<Vec<Vec<&str>>> {
    let lines: Vec<_> = text.lines().collect();
    let version = number(lines.first()?.get(..9)?).ok()?;
    if !(3.0..4.0).contains(&version) {
        return None;
    }
    let end = lines
        .iter()
        .position(|l| l.get(60..).unwrap_or("").trim() == "END OF HEADER")?;
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in &lines[end + 1..] {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(' ') {
            if let Some(block) = blocks.last_mut() {
                block.push(line);
            }
        } else {
            blocks.push(vec![line]);
        }
    }
    Some(blocks)
}

fn station(name: &str) -> &str {
    name.get(..9).unwrap_or(name)
}

fn header_line(content: &str, label: &str) -> String {
    format!("{content:<HEADER_CONTENT_WIDTH$}{label}")
}

pub fn merge(files: &[(String, Vec<u8>)], minimum: usize) -> Result<Merged> {
    let mut ordered: Vec<_> = files.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(&b.0));
    let mut kept = BTreeSet::new();
    let mut stations = BTreeSet::new();
    let mut components = Vec::new();
    for (name, raw) in ordered {
        let Ok(bytes) = decompress(raw) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if !text.is_ascii() {
            continue;
        }
        let Some(blocks) = records(text) else {
            continue;
        };
        let mut usable = false;
        for block in blocks {
            if let Ok(Some(record)) = canonical(&block) {
                kept.insert(record);
                usable = true;
            }
        }
        if usable {
            stations.insert(station(name).to_owned());
            components.push(format!("{name} {}", &cache::hash(raw)[..HASH_PREFIX]));
        }
    }
    ensure!(
        stations.len() >= minimum,
        "{} station navigation files have usable records; {minimum} are needed",
        stations.len()
    );
    let mut lines = vec![
        header_line(
            &format!(
                "{MERGED_VERSION:>9}{:11}{:<20}{:<20}",
                "", "N: GNSS NAV DATA", "M: MIXED"
            ),
            "RINEX VERSION / TYPE",
        ),
        header_line(
            &format!("{:<20}{:<20}{:<20}", "weinav-forge", "", ""),
            "PGM / RUN BY / DATE",
        ),
    ];
    for component in &components {
        ensure!(
            component.len() <= HEADER_CONTENT_WIDTH,
            "station file name too long"
        );
        lines.push(header_line(component, "COMMENT"));
    }
    lines.push(header_line("", "END OF HEADER"));
    lines.extend(kept.into_iter().map(|(_, text)| text));
    let mut text = lines.join("\n");
    text.push('\n');
    Ok(Merged {
        bytes: text.into_bytes(),
        stations: stations.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{policy::System, rinex::Broadcast};

    fn station_file(version: &str, body: &str) -> Vec<u8> {
        format!(
            "{version:>9}{:51}RINEX VERSION / TYPE\n{:60}END OF HEADER\n{body}",
            "", ""
        )
        .into_bytes()
    }

    fn field(value: f64, exponent: char) -> String {
        format!("{value:19.12E}").replace('E', &exponent.to_string())
    }

    fn record(first: &str, lines: usize, last_line_fields: usize, exponent: char) -> String {
        let mut text = first.to_owned();
        for value in [1e-5, -2e-12, 0.0] {
            text.push_str(&field(value, exponent));
        }
        for line in 0..lines {
            text.push_str("\n    ");
            let count = if line + 1 == lines {
                last_line_fields
            } else {
                ORBIT_FIELDS_PER_LINE
            };
            for index in 0..count {
                text.push_str(&field((line * 4 + index + 1) as f64, exponent));
            }
        }
        text.push('\n');
        text
    }

    #[test]
    fn rinex_numbers_use_two_digit_signed_exponents() -> Result<()> {
        assert_eq!(rinex_number(1.0)?, " 1.000000000000E+00");
        assert_eq!(rinex_number(0.0)?, " 0.000000000000E+00");
        assert_eq!(rinex_number(-1.5e-9)?, "-1.500000000000E-09");
        assert_eq!(rinex_number(2.5e10)?.len(), FIELD_WIDTH);
        assert!(rinex_number(1e120).is_err());
        Ok(())
    }

    #[test]
    fn merge_keeps_one_canonical_copy_of_each_complete_record() -> Result<()> {
        let first_gps = "G01 2026 01 01 00 00 00";
        let mut older = record(first_gps, 7, 2, 'D');
        older.push_str(&record("R05 2026 01 01 00 15 00", 4, 4, 'D'));
        older.push_str(&record("C13 2026 01 01 00 00 00", 0, 0, 'D'));
        let mut newer = record(first_gps, 7, 4, 'E');
        newer.push_str(&record("G02 2026 01 01 00 00 00", 7, 4, 'E'));
        let files = vec![
            (
                "BBBB00XXX_R_20260010000_01H_MN.rnx".to_owned(),
                station_file("3.04", &newer),
            ),
            (
                "AAAA00XXX_R_20260010000_01H_MN.rnx".to_owned(),
                station_file("3.05", &older),
            ),
            (
                "CCCC00XXX_R_20260010000_01H_MN.rnx".to_owned(),
                station_file("4.02", &record("G03 2026 01 01 00 00 00", 7, 4, 'E')),
            ),
        ];
        let merged = merge(&files, 2)?;
        assert_eq!(merged.stations, 2);
        let text = std::str::from_utf8(&merged.bytes)?;
        let starts = |prefix: &str| text.lines().filter(|l| l.starts_with(prefix)).count();
        assert_eq!(starts("G01 "), 2);
        assert_eq!(starts("G02 "), 1);
        assert_eq!(starts("R05 "), 1);
        assert_eq!(starts("C13 "), 0);
        assert_eq!(starts("G03 "), 0);
        assert_eq!(
            text.lines()
                .filter(|l| l.ends_with("COMMENT"))
                .map(|l| &l[..9])
                .collect::<Vec<_>>(),
            ["AAAA00XXX", "BBBB00XXX"]
        );
        let lines: Vec<_> = text.lines().collect();
        let glonass = lines
            .iter()
            .position(|l| l.starts_with("R05 "))
            .context("GLONASS record missing")?;
        assert_eq!(
            lines[glonass + 1..]
                .iter()
                .take_while(|l| l.starts_with("    "))
                .count(),
            orbit_lines(GLONASS_RINEX_304_FIELDS)
        );
        let blank_slots = format!("{}{}", rinex_number(0.0)?, rinex_number(0.0)?);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("    ") && l.ends_with(&blank_slots))
        );
        let mut reversed = files.clone();
        reversed.reverse();
        assert_eq!(merge(&reversed, 2)?.bytes, merged.bytes);
        let mut broadcast = Broadcast::default();
        broadcast.add(&merged.bytes)?;
        for key in [(System::Gps, 1), (System::Gps, 2), (System::Glonass, 5)] {
            assert!(broadcast.records.contains_key(&key), "{key:?}");
        }
        let error = merge(&files, 3)
            .err()
            .context("too few stations accepted")?;
        assert!(
            error
                .to_string()
                .contains("2 station navigation files have usable records; 3 are needed")
        );
        Ok(())
    }
}
