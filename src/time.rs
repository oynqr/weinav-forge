use anyhow::{Context, Result, ensure};
use chrono::{DateTime, TimeZone, Utc};

use crate::policy::System;

pub const GPS_EPOCH_UNIX: i64 = 315_964_800;
pub const WEEK: i64 = 604_800;
pub const GRID_STEP: i64 = 7200;
pub const GRID_EPOCHS: usize = 36;
pub const ZIP_FRESHNESS_SECONDS: i64 = 1800;

const LEAP_EFFECTIVE_UNIX: [i64; 18] = [
    362_793_600,
    394_329_600,
    425_865_600,
    489_024_000,
    567_993_600,
    631_152_000,
    662_688_000,
    709_948_800,
    741_484_800,
    773_020_800,
    820_454_400,
    867_715_200,
    915_148_800,
    1_136_073_600,
    1_230_768_000,
    1_341_100_800,
    1_435_708_800,
    1_483_228_800,
];

#[derive(Clone, Copy, Debug)]
pub struct Instant(pub DateTime<Utc>);

impl Instant {
    pub fn parse(text: &str) -> Result<Self> {
        let utc = DateTime::parse_from_rfc3339(text)
            .context("use an RFC 3339 date with a time zone")?
            .with_timezone(&Utc);
        ensure!(
            utc.timestamp() >= GPS_EPOCH_UNIX,
            "time precedes the GPS epoch"
        );
        ensure!(
            utc.timestamp_subsec_nanos() < 1_000_000_000,
            "leap-second instants are not supported"
        );
        Ok(Self(utc))
    }

    pub fn now() -> Self {
        Self(Utc::now())
    }

    pub fn gps(self) -> i64 {
        let unix = self.0.timestamp();
        unix - GPS_EPOCH_UNIX + LEAP_EFFECTIVE_UNIX.iter().filter(|&&t| unix >= t).count() as i64
    }

    pub fn from_gps(gps: i64) -> Result<Self> {
        ensure!(gps >= 0, "negative GPS time");
        let mut unix = gps
            .checked_add(GPS_EPOCH_UNIX)
            .context("GPS time overflow")?;
        for _ in 0..4 {
            let leap = LEAP_EFFECTIVE_UNIX.iter().filter(|&&t| unix >= t).count() as i64;
            unix = gps + GPS_EPOCH_UNIX - leap;
        }
        let instant = Self(
            Utc.timestamp_opt(unix, 0)
                .single()
                .context("time out of range")?,
        );
        ensure!(
            instant.gps() == gps,
            "GPS time falls inside a UTC leap second"
        );
        Ok(instant)
    }

    pub fn grid(self, system: System) -> [i64; GRID_EPOCHS] {
        let base = self.gps().div_euclid(GRID_STEP) * GRID_STEP + GRID_STEP;
        let offset = match system {
            System::Bds => 14,
            System::Glonass => -3600,
            _ => 0,
        };
        std::array::from_fn(|i| base + offset + i as i64 * GRID_STEP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_grid_and_bucket_edges() -> Result<()> {
        let now = Instant::from_gps(1_472_311_489)?;
        assert_eq!(now.grid(System::Gps)[0], 1_472_313_600);
        assert_eq!(now.grid(System::Glonass)[0], 1_472_310_000);
        assert_eq!(now.grid(System::Bds)[0], 1_472_313_614);
        for gps in [1_472_306_400, 1_472_306_401, 1_472_313_599] {
            assert_eq!(
                Instant::from_gps(gps)?.grid(System::Gps),
                now.grid(System::Gps)
            );
        }
        assert_eq!(
            Instant::from_gps(1_472_313_600)?.grid(System::Gps)[0],
            1_472_320_800
        );
        Ok(())
    }

    #[test]
    fn leap_offsets_are_date_dependent() -> Result<()> {
        assert_eq!(Instant::parse("1980-01-06T00:00:00Z")?.gps(), 0);
        let before = Instant::parse("2016-12-31T23:59:59Z")?;
        let after = Instant::parse("2017-01-01T00:00:00Z")?;
        assert_eq!(after.gps() - before.gps(), 2);
        assert!(Instant::from_gps(before.gps() + 1).is_err());
        assert_eq!(
            Instant::from_gps(1_472_353_200)?.0.to_rfc3339(),
            "2026-09-02T02:59:42+00:00"
        );
        Ok(())
    }
}
