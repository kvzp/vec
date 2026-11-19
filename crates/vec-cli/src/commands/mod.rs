//! The subcommands of `vec`, grouped by area, and their shared helpers.

pub mod index;
pub mod search;
pub mod services;
pub mod setup;
pub mod status;

use std::path::Path;

use anyhow::{Context, Result};
use vec_core::config::Config;
use vec_store::{Store, StoreOptions};

/// Load and validate the configuration (`--config` applied last).
pub fn load_config(cfg_path: Option<&Path>) -> Result<Config> {
    Config::load(cfg_path).context("loading config")
}

/// Open the index read-write (indexing and maintenance commands).
pub fn open_store_rw(cfg: &Config) -> Result<Store> {
    Store::open(
        &cfg.database.db_path,
        &StoreOptions::from_config(&cfg.database, false),
    )
    .context("opening the index")
}

/// A byte count as a human-readable size.
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A Unix timestamp as `YYYY-MM-DD HH:MM UTC` (proleptic Gregorian calendar,
/// without a date library).
pub fn fmt_unix_ts(secs: u64) -> String {
    let days = secs / 86_400;
    let time_of_day = secs % 86_400;
    let (hh, mm) = (time_of_day / 3600, (time_of_day % 3600) / 60);

    // Days since the epoch to a civil date (public-domain "days_to_ymd").
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);

    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(1023), "1023 B");
        assert_eq!(fmt_size(1536), "1.5 KiB");
        assert_eq!(fmt_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn timestamps_are_utc_dates() {
        assert_eq!(fmt_unix_ts(0), "1970-01-01 00:00 UTC");
        assert_eq!(fmt_unix_ts(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(fmt_unix_ts(1_700_000_000), "2023-11-14 22:13 UTC");
    }
}
