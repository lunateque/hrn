//! Confined read-only I/O, credential fences, bounded JSONL reading, and timestamp parsing
//! shared by the Claude Code, Codex CLI, and OMP transcript importers.

use hrn_kernel::{HrnError, Result};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bounded prefix read during session discovery (64 KiB).
pub(crate) const PREFIX_SCAN_BYTES: u64 = 65_536;
/// Maximum JSONL file size accepted for full import (64 MiB).
const MAX_IMPORT_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum single JSONL line length accepted for full import (8 MiB).
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum JSONL lines parsed from a single transcript.
const MAX_IMPORT_LINES: usize = 50_000;
/// Maximum size of auxiliary JSON metadata files (`.claude.json`, `custom-title.json`): 4 MiB.
pub(crate) const MAX_META_JSON_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum character length for imported session titles.
pub(crate) const MAX_IMPORT_TITLE_CHARS: usize = 120;
/// Maximum character length for imported metadata strings (`cwd`, `branch`, external IDs).
pub(crate) const MAX_IMPORT_META_CHARS: usize = 512;

/// Returns `true` when `path` targets a forbidden credential file or disallowed CLI directory.
pub(crate) fn is_forbidden_path(path: &Path) -> bool {
    for comp in path.components() {
        if let Component::Normal(part) = comp
            && let Some(name) = part.to_str()
        {
            let low = name.to_ascii_lowercase();
            if matches!(low.as_str(), ".gemini" | ".omp") {
                return true;
            }
        }
    }
    if let Some(file_name) = path.file_name().and_then(|s| s.to_str()) {
        let low = file_name.to_ascii_lowercase();
        if matches!(
            low.as_str(),
            ".credentials.json"
                | "credentials.json"
                | "auth.json"
                | "token.json"
                | "tokens.json"
                | ".env"
        ) || path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("key") || ext.eq_ignore_ascii_case("pem"))
        {
            return true;
        }
    }
    false
}

/// Returns `true` when `path` targets a forbidden credential, database, or foreign CLI path for OMP import.
pub(crate) fn is_forbidden_omp_path(path: &Path) -> bool {
    for comp in path.components() {
        if let Component::Normal(part) = comp
            && let Some(name) = part.to_str()
        {
            let low = name.to_ascii_lowercase();
            if matches!(
                low.as_str(),
                ".gemini" | ".claude" | ".codex" | "auth-gateway-tokens"
            ) {
                return true;
            }
        }
    }
    if let Some(file_name) = path.file_name().and_then(|s| s.to_str()) {
        let low = file_name.to_ascii_lowercase();
        if matches!(
            low.as_str(),
            ".credentials.json"
                | "credentials.json"
                | "auth.json"
                | "token.json"
                | "tokens.json"
                | ".env"
                | "agent.db"
                | "agent.db-wal"
                | "agent.db-shm"
                | "models.db"
                | "history.db"
                | "transcripts.db"
                | "skill-descriptions.db"
        ) || path.extension().is_some_and(|ext| {
            ext.eq_ignore_ascii_case("key")
                || ext.eq_ignore_ascii_case("pem")
                || ext.eq_ignore_ascii_case("db")
                || ext.eq_ignore_ascii_case("db-wal")
                || ext.eq_ignore_ascii_case("db-shm")
                || ext.eq_ignore_ascii_case("sqlite")
                || ext.eq_ignore_ascii_case("sqlite3")
        }) {
            return true;
        }
    }
    false
}

/// Canonicalize a root source directory while refusing forbidden directories and symlinks.
pub(crate) fn canonicalize_source_root(root: &Path) -> Result<PathBuf> {
    canonicalize_root_with(root, is_forbidden_path)
}

/// Canonicalize an OMP root directory while refusing foreign CLI roots, credential dirs, and symlinks.
pub(crate) fn canonicalize_omp_root(root: &Path) -> Result<PathBuf> {
    canonicalize_root_with(root, is_forbidden_omp_path)
}

fn canonicalize_root_with(root: &Path, is_forbidden: fn(&Path) -> bool) -> Result<PathBuf> {
    if is_forbidden(root) {
        return Err(HrnError::Store(format!(
            "refused forbidden source root `{}`",
            root.display()
        )));
    }
    let meta = fs::symlink_metadata(root)
        .map_err(|e| HrnError::Io(format!("stat `{}`: {e}", root.display())))?;
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(HrnError::Store(format!(
            "refused non-directory or symlink root `{}`",
            root.display()
        )));
    }
    let canon = fs::canonicalize(root)
        .map_err(|e| HrnError::Io(format!("canonicalize `{}`: {e}", root.display())))?;
    if is_forbidden(&canon) {
        return Err(HrnError::Store(format!(
            "refused forbidden source root `{}`",
            root.display()
        )));
    }
    Ok(canon)
}

/// Verify `candidate` is not a symlink, is not a forbidden credential file, and canonicalizes
/// strictly inside `root_canon`.
pub(crate) fn ensure_confined_file(root_canon: &Path, candidate: &Path) -> Result<PathBuf> {
    ensure_confined_file_with(root_canon, candidate, is_forbidden_path)
}

/// Verify an OMP `.jsonl` transcript file is confined inside `root_canon` and not forbidden.
pub(crate) fn ensure_confined_omp_file(root_canon: &Path, candidate: &Path) -> Result<PathBuf> {
    if !candidate
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
    {
        return Err(HrnError::Store(format!(
            "refused non-jsonl OMP transcript `{}`",
            candidate.display()
        )));
    }
    ensure_confined_file_with(root_canon, candidate, is_forbidden_omp_path)
}

fn ensure_confined_file_with(
    root_canon: &Path,
    candidate: &Path,
    is_forbidden: fn(&Path) -> bool,
) -> Result<PathBuf> {
    if is_forbidden(candidate) {
        return Err(HrnError::Store(format!(
            "refused to read forbidden credential path `{}`",
            candidate.display()
        )));
    }
    let meta = fs::symlink_metadata(candidate)
        .map_err(|e| HrnError::Io(format!("stat `{}`: {e}", candidate.display())))?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(HrnError::Store(format!(
            "refused non-regular or symlink source file `{}`",
            candidate.display()
        )));
    }
    let canon = fs::canonicalize(candidate)
        .map_err(|e| HrnError::Io(format!("canonicalize `{}`: {e}", candidate.display())))?;
    if !canon.starts_with(root_canon) || is_forbidden(&canon) {
        return Err(HrnError::Store(format!(
            "refused source path outside root `{}`",
            candidate.display()
        )));
    }
    Ok(canon)
}

/// Verify `candidate` directory is not a symlink and canonicalizes inside `root_canon`.
pub(crate) fn ensure_confined_dir(root_canon: &Path, candidate: &Path) -> Result<PathBuf> {
    ensure_confined_dir_with(root_canon, candidate, is_forbidden_path)
}

/// Verify an OMP subdirectory is not a symlink, not forbidden, and canonicalizes inside `root_canon`.
pub(crate) fn ensure_confined_omp_dir(root_canon: &Path, candidate: &Path) -> Result<PathBuf> {
    ensure_confined_dir_with(root_canon, candidate, is_forbidden_omp_path)
}

fn ensure_confined_dir_with(
    root_canon: &Path,
    candidate: &Path,
    is_forbidden: fn(&Path) -> bool,
) -> Result<PathBuf> {
    if is_forbidden(candidate) {
        return Err(HrnError::Store(format!(
            "refused to inspect forbidden directory `{}`",
            candidate.display()
        )));
    }
    let meta = fs::symlink_metadata(candidate)
        .map_err(|e| HrnError::Io(format!("stat `{}`: {e}", candidate.display())))?;
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(HrnError::Store(format!(
            "refused non-directory or symlink `{}`",
            candidate.display()
        )));
    }
    let canon = fs::canonicalize(candidate)
        .map_err(|e| HrnError::Io(format!("canonicalize `{}`: {e}", candidate.display())))?;
    if !canon.starts_with(root_canon) || is_forbidden(&canon) {
        return Err(HrnError::Store(format!(
            "refused directory outside root `{}`",
            candidate.display()
        )));
    }
    Ok(canon)
}

/// Read at most `max_bytes` from a regular non-symlink file as lossy UTF-8 (strictly read-only).
pub(crate) fn read_bounded_prefix(path: &Path, max_bytes: u64) -> Result<String> {
    read_bounded_prefix_with(path, max_bytes, is_forbidden_path)
}

/// Read at most `max_bytes` from a regular non-symlink OMP transcript file (strictly read-only).
pub(crate) fn read_bounded_prefix_omp(path: &Path, max_bytes: u64) -> Result<String> {
    read_bounded_prefix_with(path, max_bytes, is_forbidden_omp_path)
}

fn read_bounded_prefix_with(
    path: &Path,
    max_bytes: u64,
    is_forbidden: fn(&Path) -> bool,
) -> Result<String> {
    if is_forbidden(path) {
        return Err(HrnError::Store(format!(
            "refused forbidden file `{}`",
            path.display()
        )));
    }
    let meta = fs::symlink_metadata(path)
        .map_err(|e| HrnError::Io(format!("stat `{}`: {e}", path.display())))?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(HrnError::Store(format!(
            "refused symlink or non-file `{}`",
            path.display()
        )));
    }
    let file =
        File::open(path).map_err(|e| HrnError::Io(format!("open `{}`: {e}", path.display())))?;
    let mut buf = Vec::new();
    file.take(max_bytes)
        .read_to_end(&mut buf)
        .map_err(|e| HrnError::Io(format!("read `{}`: {e}", path.display())))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Parse a JSONL file with strict file-size, line-length, and line-count caps.
pub(crate) fn read_jsonl_values(path: &Path) -> Result<Vec<Value>> {
    read_jsonl_values_with(path, is_forbidden_path)
}

/// Parse an OMP JSONL file with strict file-size, line-length, and line-count caps.
pub(crate) fn read_jsonl_values_omp(path: &Path) -> Result<Vec<Value>> {
    read_jsonl_values_with(path, is_forbidden_omp_path)
}

fn read_jsonl_values_with(path: &Path, is_forbidden: fn(&Path) -> bool) -> Result<Vec<Value>> {
    if is_forbidden(path) {
        return Err(HrnError::Store(format!(
            "refused forbidden file `{}`",
            path.display()
        )));
    }
    let meta = fs::symlink_metadata(path)
        .map_err(|e| HrnError::Io(format!("stat `{}`: {e}", path.display())))?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(HrnError::Store(format!(
            "refused symlink or non-file `{}`",
            path.display()
        )));
    }
    if meta.len() > MAX_IMPORT_FILE_BYTES {
        return Err(HrnError::Store(format!(
            "transcript `{}` exceeds {} MiB import limit",
            path.display(),
            MAX_IMPORT_FILE_BYTES / (1024 * 1024)
        )));
    }
    let file =
        File::open(path).map_err(|e| HrnError::Io(format!("open `{}`: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    let mut values = Vec::new();
    let line_cap = (MAX_LINE_BYTES + 1) as u64;
    for _ in 0..MAX_IMPORT_LINES {
        raw.clear();
        let n = (&mut reader)
            .take(line_cap)
            .read_until(b'\n', &mut raw)
            .map_err(|e| HrnError::Io(format!("read `{}`: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        if raw.len() > MAX_LINE_BYTES {
            discard_rest_of_line(&mut reader, path)?;
            continue;
        }
        let line = String::from_utf8_lossy(&raw);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(trimmed)
            && val.is_object()
        {
            values.push(val);
        }
    }
    Ok(values)
}

fn discard_rest_of_line<R: BufRead>(reader: &mut R, path: &Path) -> Result {
    loop {
        let buf = reader
            .fill_buf()
            .map_err(|e| HrnError::Io(format!("read `{}`: {e}", path.display())))?;
        if buf.is_empty() {
            return Ok(());
        }
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            reader.consume(pos + 1);
            return Ok(());
        }
        let len = buf.len();
        reader.consume(len);
    }
}

/// File modification timestamp in Unix milliseconds, falling back to `0`.
pub(crate) fn file_mtime_ms(path: &Path) -> i64 {
    fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Current system time in Unix milliseconds.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

fn consume_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&ch) = chars.peek() {
        if ('\x20'..='\x3F').contains(&ch) {
            chars.next();
        } else {
            break;
        }
    }
    if let Some(&final_ch) = chars.peek()
        && ('\x40'..='\x7E').contains(&final_ch)
    {
        chars.next();
    }
}

fn consume_st_terminated(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, allow_bel: bool) {
    while let Some(ch) = chars.next() {
        if (allow_bel && ch == '\x07') || ch == '\u{009C}' {
            break;
        }
        if ch == '\x1b' {
            if chars.peek() == Some(&'\\') {
                chars.next();
            }
            break;
        }
    }
}

/// Strip ANSI escape sequences (CSI/OSC/DCS/C1), Unicode bidi overrides, and control characters.
pub(crate) fn strip_control_and_escapes(raw: &str, newline_to_space: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    consume_csi(&mut chars);
                }
                Some(']') => {
                    chars.next();
                    consume_st_terminated(&mut chars, true);
                }
                Some('P' | '^' | '_' | 'X') => {
                    chars.next();
                    consume_st_terminated(&mut chars, false);
                }
                Some(next) if ('\x40'..='\x5F').contains(&next) => {
                    chars.next();
                }
                _ => {}
            },
            '\u{009B}' => consume_csi(&mut chars),
            '\u{009D}' => consume_st_terminated(&mut chars, true),
            '\r' | '\n' | '\t' if newline_to_space => out.push(' '),
            ch if ch.is_control() || is_bidi_control(ch) => {}
            ch => out.push(ch),
        }
    }
    out
}

/// Sanitize and clip an imported title/prompt snippet onto a single line within `max_chars`.
pub(crate) fn sanitize_title(raw: &str, max_chars: usize) -> String {
    let cleaned = strip_control_and_escapes(raw, true);
    let single: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = single.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_owned();
    }
    if max_chars <= 3 {
        return trimmed.chars().take(max_chars).collect();
    }
    let keep = max_chars - 3;
    let cut = trimmed
        .char_indices()
        .nth(keep)
        .map_or(trimmed.len(), |(idx, _)| idx);
    format!("{}...", trimmed.get(..cut).unwrap_or(trimmed).trim_end())
}

/// Sanitize imported single-line metadata (`cwd`, `branch`, external IDs) and cap length.
pub(crate) fn sanitize_meta(raw: &str, max_chars: usize) -> String {
    let cleaned = strip_control_and_escapes(raw, false);
    let trimmed = cleaned.trim();
    match trimmed.char_indices().nth(max_chars) {
        Some((cut, _)) => trimmed.get(..cut).unwrap_or(trimmed).trim_end().to_owned(),
        None => trimmed.to_owned(),
    }
}

/// Clip a sanitized title/prompt snippet to `max_chars` on a single line.
pub(crate) fn clip_title(raw: &str, max_chars: usize) -> String {
    sanitize_title(raw, max_chars)
}

/// Parse an integer epoch (ms or seconds) or ISO-8601 / RFC3339 timestamp string into Unix ms.
pub(crate) fn parse_timestamp_ms(val: &Value) -> Option<i64> {
    if let Some(n) = val.as_i64() {
        return Some(if n.abs() < 10_000_000_000 {
            n.saturating_mul(1000)
        } else {
            n
        });
    }
    if let Some(f) = val.as_f64()
        && f.is_finite()
    {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "timestamps fit comfortably in i64"
        )]
        let n = f as i64;
        return Some(if n.abs() < 10_000_000_000 {
            n.saturating_mul(1000)
        } else {
            n
        });
    }
    parse_iso8601_ms(val.as_str()?)
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff][Z|+HH:MM|-HH:MM]` into Unix milliseconds.
pub(crate) fn parse_iso8601_ms(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let (date, rest) = s.split_once('T').or_else(|| s.split_once(' '))?;
    let mut dp = date.split('-');
    let year: i64 = dp.next()?.parse().ok()?;
    let month: u32 = dp.next()?.parse().ok()?;
    let day: u32 = dp.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (time_part, offset_secs) = split_tz_offset(rest)?;
    let (hms, frac_ms) = match time_part.split_once('.') {
        Some((hms, frac)) => (hms, parse_millis_fraction(frac)),
        None => (time_part, 0i64),
    };
    let mut tp = hms.split(':');
    let hour: i64 = tp.next()?.parse().ok()?;
    let min: i64 = tp.next()?.parse().ok()?;
    let sec: i64 = tp.next().unwrap_or("0").parse().ok()?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&min) || !(0..=60).contains(&sec) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days
        .saturating_mul(86_400)
        .saturating_add(hour * 3600 + min * 60 + sec)
        .saturating_sub(offset_secs);
    Some(secs.saturating_mul(1000).saturating_add(frac_ms))
}

fn split_tz_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(time) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return Some((time, 0));
    }
    let Some(pos) = rest.rfind(['+', '-']) else {
        return Some((rest, 0));
    };
    let (time, tz) = rest.split_at(pos);
    let sign = if tz.starts_with('-') { -1i64 } else { 1i64 };
    let digits = tz.get(1..)?;
    let (hh, mm) = match digits.split_once(':') {
        Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
        None if digits.len() == 4 => (
            digits.get(0..2)?.parse::<i64>().ok()?,
            digits.get(2..4)?.parse::<i64>().ok()?,
        ),
        _ => return None,
    };
    Some((time, sign * (hh * 3600 + mm * 60)))
}

fn parse_millis_fraction(frac: &str) -> i64 {
    let digits: String = frac
        .chars()
        .take_while(char::is_ascii_digit)
        .take(3)
        .collect();
    if digits.is_empty() {
        return 0;
    }
    let mut val = digits.parse::<i64>().unwrap_or(0);
    if digits.len() == 1 {
        val *= 100;
    } else if digits.len() == 2 {
        val *= 10;
    }
    val
}

/// Howard Hinnant's civil-to-days algorithm (days since 1970-01-01).
const fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + (day as i64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
