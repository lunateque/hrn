//! Read-only Codex CLI session discovery: `state_*.sqlite` (`threads` table opened strictly
//! read-only + immutable) plus bounded JSONL fallback across `sessions/**` and `archived_sessions/**`.

use super::codex::{extract_codex_text, str_field};
use super::io::{
    MAX_IMPORT_META_CHARS, MAX_IMPORT_TITLE_CHARS, PREFIX_SCAN_BYTES, canonicalize_source_root,
    clip_title, ensure_confined_dir, ensure_confined_file, file_mtime_ms, parse_iso8601_ms,
    parse_timestamp_ms, read_bounded_prefix, sanitize_meta, sanitize_title,
};
use super::{ExternalSession, ImportSource};
use hrn_kernel::{HrnError, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Maximum directory recursion depth when scanning `<codex_dir>/sessions/**`.
const MAX_SCAN_DEPTH: usize = 6;

/// Discover Codex sessions from strictly read-only `state_*.sqlite` `threads` (when present)
/// with automatic fallback to scanning `sessions/**` and `archived_sessions/**`.
pub(crate) fn discover(codex_dir: &Path) -> Result<Vec<ExternalSession>> {
    if !codex_dir.exists() {
        return Ok(Vec::new());
    }
    let root_canon = canonicalize_source_root(codex_dir)?;
    let mut out = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();

    if let Some(db_path) = newest_state_sqlite(&root_canon)
        && let Ok(rows) = discover_from_sqlite(&root_canon, &db_path)
    {
        for row in rows {
            seen_paths.insert(row.path.clone());
            out.push(row);
        }
    }

    for (subdir, archived) in [("sessions", false), ("archived_sessions", true)] {
        let dir = root_canon.join(subdir);
        if dir.exists()
            && let Ok(dir_canon) = ensure_confined_dir(&root_canon, &dir)
        {
            scan_rollouts(
                &root_canon,
                &dir_canon,
                archived,
                0,
                &mut seen_paths,
                &mut out,
            );
        }
    }

    out.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(out)
}

/// Find the newest `state_*.sqlite` file confined inside `root_canon`.
fn newest_state_sqlite(root_canon: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(root_canon).ok()?;
    let mut best: Option<(u64, String, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(middle) = name
            .strip_prefix("state_")
            .and_then(|s| s.strip_suffix(".sqlite"))
        else {
            continue;
        };
        let Ok(canon) = ensure_confined_file(root_canon, &path) else {
            continue;
        };
        let version = middle.parse::<u64>().unwrap_or(0);
        let key = (version, name.to_owned(), canon);
        if best
            .as_ref()
            .is_none_or(|cur| (key.0, &key.1) > (cur.0, &cur.1))
        {
            best = Some(key);
        }
    }
    best.map(|(_, _, p)| p)
}

/// Build a strictly read-only + immutable `SQLite` URI so opening Codex's DB NEVER writes `-wal`
/// or `-shm` files and NEVER modifies source timestamps.
pub(crate) fn readonly_immutable_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let slashes = stripped.replace('\\', "/");
    let mut encoded = String::with_capacity(slashes.len() + 32);
    if slashes.starts_with('/') {
        encoded.push_str("file:");
    } else {
        encoded.push_str("file:///");
    }
    for byte in slashes.bytes() {
        match byte {
            b'%' | b'?' | b'#' | b' ' | 0x00..=0x1F | 0x7F..=0xFF => {
                let _ = write!(encoded, "%{byte:02X}");
            }
            _ => encoded.push(char::from(byte)),
        }
    }
    encoded.push_str("?mode=ro&immutable=1");
    encoded
}

fn open_state_readonly(path: &Path) -> Result<Connection> {
    let uri = readonly_immutable_uri(path);
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(&uri, flags)
        .map_err(|e| HrnError::Store(format!("open codex sqlite `{}`: {e}", path.display())))?;
    conn.execute_batch("PRAGMA query_only = ON;")
        .map_err(|e| HrnError::Store(format!("set query_only on codex sqlite: {e}")))?;
    Ok(conn)
}

fn build_threads_query(cols: &HashSet<String>) -> Option<String> {
    if !cols.contains("id") || !cols.contains("rollout_path") {
        return None;
    }
    let title_expr = if cols.contains("title") {
        "COALESCE(title, '')"
    } else {
        "''"
    };
    let cwd_expr = if cols.contains("cwd") {
        "COALESCE(cwd, '')"
    } else {
        "''"
    };
    let updated_expr = if cols.contains("updated_at") {
        "updated_at"
    } else if cols.contains("created_at") {
        "created_at"
    } else {
        "NULL"
    };
    let archived_expr = if cols.contains("archived") {
        "COALESCE(archived, 0)"
    } else {
        "0"
    };
    Some(format!(
        "SELECT id, rollout_path, {title_expr}, {cwd_expr}, {updated_expr}, {archived_expr}
         FROM threads
         WHERE rollout_path IS NOT NULL AND rollout_path != ''"
    ))
}

fn discover_from_sqlite(root_canon: &Path, db_path: &Path) -> Result<Vec<ExternalSession>> {
    let conn = open_state_readonly(db_path)?;
    let mut col_stmt = conn
        .prepare("PRAGMA table_info(threads)")
        .map_err(|e| HrnError::Store(format!("inspect codex threads: {e}")))?;
    let cols: HashSet<String> = col_stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| HrnError::Store(format!("read codex threads columns: {e}")))?
        .filter_map(std::result::Result::ok)
        .collect();
    let Some(sql) = build_threads_query(&cols) else {
        return Ok(Vec::new());
    };
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| HrnError::Store(format!("query codex threads: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let rollout: String = row.get(1)?;
            let title: String = row.get(2)?;
            let cwd: String = row.get(3)?;
            let updated_raw: rusqlite::types::Value = row.get(4)?;
            let archived: i64 = row.get(5)?;
            Ok((id, rollout, title, cwd, updated_raw, archived != 0))
        })
        .map_err(|e| HrnError::Store(format!("iterate codex threads: {e}")))?;

    let mut out = Vec::new();
    for (id, rollout, title, cwd, updated_raw, archived) in rows.flatten() {
        let candidate = resolve_rollout_path(root_canon, &rollout);
        if !candidate
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        {
            continue;
        }
        let Ok(canon) = ensure_confined_file(root_canon, &candidate) else {
            continue;
        };
        let prefix = scan_prefix(&canon);
        let clean_id = sanitize_meta(&id, MAX_IMPORT_META_CHARS);
        let resolved_id = if clean_id.is_empty() {
            prefix.id.unwrap_or_else(|| "codex-session".to_owned())
        } else {
            clean_id
        };
        let clean_cwd = sanitize_meta(&cwd, MAX_IMPORT_META_CHARS);
        let resolved_cwd = if clean_cwd.is_empty() {
            prefix.cwd.unwrap_or_else(|| ".".to_owned())
        } else {
            clean_cwd
        };
        let clean_title = sanitize_title(&title, MAX_IMPORT_TITLE_CHARS);
        let resolved_title = if clean_title.is_empty() {
            prefix.title.unwrap_or_default()
        } else {
            clean_title
        };
        let updated_at_ms = sqlite_timestamp_ms(&updated_raw)
            .or(prefix.updated_at_ms)
            .unwrap_or_else(|| file_mtime_ms(&canon));
        out.push(ExternalSession {
            source: ImportSource::Codex,
            id: resolved_id,
            title: resolved_title,
            cwd: resolved_cwd,
            updated_at_ms,
            path: canon,
            archived,
        });
    }
    Ok(out)
}

fn resolve_rollout_path(root_canon: &Path, raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        root_canon.join(p)
    }
}

fn sqlite_timestamp_ms(val: &rusqlite::types::Value) -> Option<i64> {
    match val {
        rusqlite::types::Value::Integer(n) => Some(if n.abs() < 10_000_000_000 {
            n.saturating_mul(1000)
        } else {
            *n
        }),
        rusqlite::types::Value::Real(f) if f.is_finite() => {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "timestamps fit comfortably in i64"
            )]
            let n = *f as i64;
            Some(if n.abs() < 10_000_000_000 {
                n.saturating_mul(1000)
            } else {
                n
            })
        }
        rusqlite::types::Value::Text(s) => parse_iso8601_ms(s),
        _ => None,
    }
}

fn scan_rollouts(
    root_canon: &Path,
    dir: &Path,
    archived: bool,
    depth: usize,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<ExternalSession>,
) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.file_type().is_dir() {
            if let Ok(sub_canon) = ensure_confined_dir(root_canon, &path) {
                scan_rollouts(root_canon, &sub_canon, archived, depth + 1, seen, out);
            }
            continue;
        }
        if !meta.file_type().is_file()
            || !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        {
            continue;
        }
        let Ok(file_canon) = ensure_confined_file(root_canon, &path) else {
            continue;
        };
        if !seen.insert(file_canon.clone()) {
            continue;
        }
        let prefix = scan_prefix(&file_canon);
        let id = prefix.id.unwrap_or_else(|| {
            sanitize_meta(
                file_canon
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("codex-session"),
                MAX_IMPORT_META_CHARS,
            )
        });
        out.push(ExternalSession {
            source: ImportSource::Codex,
            id,
            title: prefix
                .title
                .map(|t| sanitize_title(&t, MAX_IMPORT_TITLE_CHARS))
                .unwrap_or_default(),
            cwd: prefix
                .cwd
                .map(|c| sanitize_meta(&c, MAX_IMPORT_META_CHARS))
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| ".".to_owned()),
            updated_at_ms: prefix
                .updated_at_ms
                .unwrap_or_else(|| file_mtime_ms(&file_canon)),
            path: file_canon,
            archived,
        });
    }
}

#[derive(Default)]
struct PrefixInfo {
    id: Option<String>,
    cwd: Option<String>,
    title: Option<String>,
    updated_at_ms: Option<i64>,
}

fn scan_prefix(path: &Path) -> PrefixInfo {
    let mut info = PrefixInfo::default();
    let Ok(prefix) = read_bounded_prefix(path, PREFIX_SCAN_BYTES) else {
        return info;
    };
    for line in prefix.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(ts) = val.get("timestamp").and_then(parse_timestamp_ms) {
            info.updated_at_ms = Some(info.updated_at_ms.map_or(ts, |prev| prev.max(ts)));
        }
        let kind = val.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = val.get("payload").unwrap_or(&val);
        match kind {
            "session_meta" => {
                if info.id.is_none() {
                    info.id = str_field(payload, "id")
                        .map(|s| sanitize_meta(&s, MAX_IMPORT_META_CHARS))
                        .filter(|s| !s.is_empty());
                }
                if info.cwd.is_none() {
                    info.cwd = str_field(payload, "cwd")
                        .map(|s| sanitize_meta(&s, MAX_IMPORT_META_CHARS))
                        .filter(|s| !s.is_empty());
                }
            }
            "turn_context" => {
                if info.cwd.is_none() {
                    info.cwd = str_field(payload, "cwd")
                        .map(|s| sanitize_meta(&s, MAX_IMPORT_META_CHARS))
                        .filter(|s| !s.is_empty());
                }
            }
            "response_item" => {
                if info.title.is_none()
                    && payload.get("type").and_then(Value::as_str) == Some("message")
                    && payload.get("role").and_then(Value::as_str) == Some("user")
                {
                    let text = extract_codex_text(payload.get("content"));
                    if !text.is_empty() {
                        info.title = Some(clip_title(&text, 80));
                    }
                }
            }
            "event_msg" => {
                if info.title.is_none()
                    && payload.get("type").and_then(Value::as_str) == Some("user_message")
                    && let Some(msg) = str_field(payload, "message")
                {
                    info.title = Some(clip_title(&msg, 80));
                }
            }
            _ => {}
        }
    }
    info
}
