//! `write` targets that are not whole files: one archive member (`a.zip:inner/file`, creating
//! the archive when it does not exist yet) and one `SQLite` row (`db.sqlite:table` inserts,
//! `db.sqlite:table:key` updates, an empty body deletes). Addresses match what [`super::read`]
//! reads, so `read db.sqlite:users` and `write db.sqlite:users` are the same row set.

use super::{archive, sqlite, write_archive, write_sqlite};
use crate::paths::{display_rel, resolve};
use hrn_kernel::{HrnError, Result, ToolOutput};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
enum Kind {
    Archive(archive::Format),
    Sqlite,
}

fn err(what: impl Into<String>) -> HrnError {
    HrnError::Tool(what.into())
}

/// The container address inside `raw`: the longest known extension followed by `:` or `?`.
fn container(cwd: &Path, base: &str) -> Option<(PathBuf, Kind, String)> {
    let lower = base.to_ascii_lowercase();
    let mut table: Vec<(&str, Kind)> = archive::EXTENSIONS
        .iter()
        .map(|(ext, format)| (*ext, Kind::Archive(*format)))
        .collect();
    table.extend(sqlite::EXTENSIONS.iter().map(|ext| (*ext, Kind::Sqlite)));
    let mut best: Option<(usize, Kind, PathBuf, String)> = None;
    for (ext, kind) in table {
        for (at, _) in lower.match_indices(ext) {
            let end = at + ext.len();
            let next = base.as_bytes().get(end).copied();
            if !matches!(next, Some(b':' | b'?')) || best.as_ref().is_some_and(|b| b.0 >= end) {
                continue;
            }
            let head = base.get(..end).unwrap_or_default();
            let path = resolve(cwd, head);
            if path.exists() || next == Some(b':') {
                best = Some((
                    end,
                    kind,
                    path,
                    base.get(end..).unwrap_or_default().to_owned(),
                ));
            }
        }
    }
    best.map(|(_, kind, path, rest)| (path, kind, rest))
}

/// Why writing the container `raw` itself (not a member/row of it) makes no sense; `None` when
/// `raw` is not a readable container.
fn bare_container(cwd: &Path, raw: &str) -> Result<Option<String>> {
    let path = resolve(cwd, raw);
    if !path.is_file() {
        return Ok(None);
    }
    let shown = display_rel(cwd, &path);
    let lower = shown.to_ascii_lowercase();
    let mut head = [0u8; 16];
    let read = std::io::Read::read(
        &mut std::fs::File::open(&path).map_err(|e| err(format!("cannot open `{shown}`: {e}")))?,
        &mut head,
    )
    .unwrap_or(0);
    let head = head.get(..read).unwrap_or_default();
    if let Some((_, format)) = archive::EXTENSIONS
        .iter()
        .find(|(ext, _)| lower.ends_with(ext))
        && archive::matches_magic(*format, head)
    {
        return Ok(Some(format!(
            "`{shown}` is an archive: address a member (`{shown}:inner/path`); writing the whole              archive as text is not supported"
        )));
    }
    if sqlite::EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) && sqlite::has_magic(head) {
        return Ok(Some(format!(
            "`{shown}` is a database: address a table (`{shown}:table`) or a row              (`{shown}:table:key`)"
        )));
    }
    Ok(None)
}

/// Write `content` at `raw` when it addresses an archive member or a database row; `None` for an
/// ordinary path (including a container that is really a plain text file).
///
/// # Errors
/// The address names a container but no writable part of it, or the write itself fails.
pub(crate) fn write_to(cwd: &Path, raw: &str, content: &str) -> Result<Option<ToolOutput>> {
    if let Some(msg) = bare_container(cwd, raw)? {
        return Err(err(msg));
    }
    if resolve(cwd, raw).is_file() {
        return Ok(None);
    }
    // A write address is taken verbatim: `db:users:1` is row 1, not line 1.
    let Some((path, kind, rest)) = container(cwd, raw) else {
        return Ok(None);
    };
    let shown = display_rel(cwd, &path);
    match kind {
        Kind::Archive(format) => {
            let inner = rest.strip_prefix(':').unwrap_or_default();
            if inner.is_empty() {
                return Err(err(format!(
                    "`{shown}` is an archive: address a member (`{shown}:inner/path`); writing \
                     the whole archive as text is not supported"
                )));
            }
            if rest.contains('?') {
                return Err(err(format!(
                    "`{raw}` is a query, not a member; write `{shown}:inner/path`"
                )));
            }
            write_archive::write_member(cwd, &path, format, inner, content).map(Some)
        }
        Kind::Sqlite => {
            let rest = rest.strip_prefix(':').unwrap_or_default();
            if !path.is_file() {
                return Err(err(format!("SQLite database `{shown}` not found")));
            }
            if rest.contains('?') {
                return Err(err(format!(
                    "`{raw}` is a query, not a row; write `{shown}:table[:key]` with a JSON5 \
                     object"
                )));
            }
            let (table, key) = match rest.split_once(':') {
                Some((table, key)) => (table, Some(key)),
                None if rest.is_empty() => {
                    return Err(err(format!(
                        "`{shown}` is a database: address a table (`{shown}:table`) or a row \
                         (`{shown}:table:key`)"
                    )));
                }
                None => (rest, None),
            };
            write_sqlite::write_row(cwd, &path, table, key, content).map(Some)
        }
    }
}
