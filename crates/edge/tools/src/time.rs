//! Small wall-clock helpers shared by the executable reader and the archive writers: unix
//! "now", ISO formatting of a `time_t` and the packed DOS date/time, plus the days-from-civil
//! conversion they are built on.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the unix epoch (`0` when the clock is before it).
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Days since 1970-01-01 to `(year, month, day)` (the days-from-civil algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `time_t` as an ISO-8601 UTC timestamp.
pub(crate) fn unix_utc(secs: u32) -> String {
    let days = i64::from(secs / 86_400);
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Today's `(time, date)` in the packed DOS form zip headers use (UTC, clamped to 1980).
pub(crate) fn dos_stamp() -> (u16, u16) {
    let secs = unix_now();
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
    let rem = (secs % 86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    let year = u16::try_from(y.max(1980) - 1980).unwrap_or(0);
    (
        (((rem / 3600) as u16) << 11) | (((rem % 3600 / 60) as u16) << 5) | ((rem % 60 / 2) as u16),
        (year << 9) | (u16::try_from(m).unwrap_or(1) << 5) | u16::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_universal_time() {
        assert_eq!(unix_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_utc(1_735_689_600), "2025-01-01T00:00:00Z");
        let (_, date) = dos_stamp();
        let day = u32::from(date & 0x1f);
        let month = u32::from((date >> 5) & 0x0f);
        let year = 1980 + i32::from(date >> 9);
        let today = unix_utc(u32::try_from(unix_now()).unwrap_or(0));
        assert_eq!(
            today[..10],
            format!("{year:04}-{month:02}-{day:02}"),
            "{today}"
        );
    }
}
