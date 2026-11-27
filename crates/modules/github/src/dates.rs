//! `since` / `until` bounds for GitHub search qualifiers: relative durations (`3d`, `12h`,
//! `2w`, `2mo`, `1y`), `YYYY-MM-DD`, or an ISO datetime. Calendar math is hand-rolled.

use hrn_kernel::{HrnError, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// The `(year, month, day)` of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `YYYY-MM-DD` validated, or `None`.
fn parse_date(text: &str) -> Option<(i64, i64, i64)> {
    let mut parts = text.splitn(3, '-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    ((1..=12).contains(&m) && (1..=days_in_month(y, m)).contains(&d)).then_some((y, m, d))
}

fn date_string((y, m, d): (i64, i64, i64)) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

fn invalid(raw: &str) -> HrnError {
    HrnError::Tool(format!(
        "invalid date bound `{raw}`: use 3d, 12h, 2w, 2mo, 1y, YYYY-MM-DD or an ISO datetime"
    ))
}

/// Normalise a `since` / `until` value into a search-qualifier date, relative to `now`.
pub(crate) fn bound(raw: &str, now: i64) -> Result<String> {
    let raw = raw.trim();
    if let Some((date, time)) = raw.split_once(['T', 't']) {
        let clock_ok = !time.is_empty()
            && time
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, ':' | '.' | '+' | '-' | 'Z' | 'z'));
        return if parse_date(date).is_some() && clock_ok {
            Ok(raw.to_owned())
        } else {
            Err(invalid(raw))
        };
    }
    if let Some(date) = parse_date(raw) {
        return Ok(date_string(date));
    }
    let (digits, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let n: i64 = digits.parse().map_err(|_| invalid(raw))?;
    match unit {
        "h" => {
            let t = now - n * 3600;
            let (date, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
            Ok(format!(
                "{}T{:02}:{:02}:{:02}Z",
                date_string(civil_from_days(date)),
                secs / 3600,
                secs % 3600 / 60,
                secs % 60
            ))
        }
        "d" | "w" => {
            let days = if unit == "w" { n * 7 } else { n };
            Ok(date_string(civil_from_days(now.div_euclid(86_400) - days)))
        }
        "mo" | "y" => {
            let months = if unit == "y" { n * 12 } else { n };
            let (y, m, d) = civil_from_days(now.div_euclid(86_400));
            let total = y * 12 + (m - 1) - months;
            let (y, m) = (total.div_euclid(12), total.rem_euclid(12) + 1);
            Ok(date_string((y, m, d.min(days_in_month(y, m)))))
        }
        _ => Err(invalid(raw)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Days since 1970-01-01 of a proleptic Gregorian date.
    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// 2024-03-31T13:05:09Z
    const NOW: i64 = 1_711_890_309;

    #[test]
    fn civil_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(civil_from_days(NOW / 86_400), (2024, 3, 31));
    }

    #[test]
    fn relative_bounds() {
        assert_eq!(bound("3d", NOW).expect("d"), "2024-03-28");
        assert_eq!(bound("2w", NOW).expect("w"), "2024-03-17");
        assert_eq!(bound("12h", NOW).expect("h"), "2024-03-31T01:05:09Z");
        assert_eq!(bound("1mo", NOW).expect("mo"), "2024-02-29");
        assert_eq!(bound("1y", NOW).expect("y"), "2023-03-31");
    }

    #[test]
    fn absolute_bounds_and_rejects() {
        assert_eq!(bound("2024-01-05", NOW).expect("date"), "2024-01-05");
        assert_eq!(
            bound("2024-01-05T10:00:00Z", NOW).expect("datetime"),
            "2024-01-05T10:00:00Z"
        );
        for bad in ["2024-13-01", "2023-02-29", "yesterday", "5x", "d3", ""] {
            assert!(bound(bad, NOW).is_err(), "{bad}");
        }
    }
}
