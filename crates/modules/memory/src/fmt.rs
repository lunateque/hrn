//! Dates, previews and the shared text layout of recalled memories.

use crate::store::Entry;
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

/// Longest content shown for one memory in a listing; the full row is `read memory://<id>`.
pub(crate) const PREVIEW_CHARS: usize = 500;

/// Unix time in milliseconds.
pub(crate) fn now_ms() -> i64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(dur.as_millis()).unwrap_or(i64::MAX)
}

/// `YYYY-MM-DD HH:MM` (UTC) of a Unix millisecond timestamp.
pub(crate) fn utc_minute(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let in_day = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        in_day / 3600,
        in_day % 3600 / 60
    )
}

/// `YYYY-MM-DD` (UTC).
pub(crate) fn utc_day(ms: i64) -> String {
    utc_minute(ms).chars().take(10).collect()
}

/// Proleptic Gregorian date of a day count since 1970-01-01 (Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `text` cut to `max` characters, ending in `…` when it was cut.
pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// One bullet of a recall listing: `- <content> (id: <id>) [<source>/<scope>] (<date>)`.
pub(crate) fn bullet(entry: &Entry, max_chars: usize) -> String {
    let scope = if entry.is_global() {
        "global"
    } else {
        "project"
    };
    let mut line = format!(
        "- {} (id: {}) [{}/{scope}] ({})",
        clip(&entry.content, max_chars).replace('\n', " "),
        entry.id,
        entry.source,
        utc_day(entry.updated_ms)
    );
    if !entry.context.is_empty() {
        let _ = write!(line, " — {}", clip(&entry.context, 120).replace('\n', " "));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::{clip, utc_minute};

    #[test]
    fn formats_utc_and_clips_by_characters() {
        assert_eq!(utc_minute(0), "1970-01-01 00:00");
        assert_eq!(
            utc_minute(1_709_164_800_000 + 3_660_000),
            "2024-02-29 01:01"
        );
        assert_eq!(clip("héllo wörld", 5), "héllo…");
        assert_eq!(clip("short", 5), "short");
    }
}
