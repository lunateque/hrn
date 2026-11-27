//! Rich `read_file` formats behind one `path` string: archives (`a.zip`, `a.tar.gz`,
//! `a.zip:inner/file`), `SQLite` (`db.sqlite`, `:table`, `?q=SELECT ...`), PDF, DOCX / XLSX /
//! PPTX / EPUB, Jupyter notebooks, and `?q=` jq queries on JSON / JSONL (also inside archives).
//!
//! [`read`] returns `None` for anything that is not one of these, leaving plain-text reading
//! to `fs_read`. Output is text, so the usual line selectors apply to it.

mod archive;
mod exe;
mod jq;
mod json5;
mod notebook;
mod office;
mod params;
mod pdf;
mod pdf_obj;
mod pdf_text;
mod sqlite;
mod tar;
mod write;
mod write_archive;
mod write_sqlite;
mod xml;
mod zip;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod write_tests;

pub(crate) use write::write_to as write_target;

/// Native-executable metadata for a file whose bytes look binary (`None` when the signature does
/// not hold up as a real header).
///
/// # Errors
/// The file cannot be read.
pub(crate) fn exe_meta(path: &Path, size: u64) -> Result<Option<String>> {
    exe::describe(path, size)
}

use crate::fs_read::{paged_lines, render_text};
use crate::paths::{display_rel, resolve};
use crate::selector::{self, Selector};
use archive::{Format, MEMBER_CAP};
use hrn_kernel::{HrnError, Result, ToolOutput};
use params::Params;
use serde_json::Value;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

const BINARY_PROBE_LEN: usize = 8192;
const JSON_EXTENSIONS: &[&str] = &[".jsonl", ".ndjson", ".json"];
const ARCHIVE_LIST_LIMIT: usize = 500;

#[derive(Clone, Copy)]
enum Kind {
    Archive(Format),
    Sqlite,
    Json,
}

/// Read `raw` when it names a rich-format file or a selector into one; `None` otherwise.
pub(crate) fn read(cwd: &Path, raw: &str) -> Result<Option<ToolOutput>> {
    let literal = resolve(cwd, raw);
    if literal.is_file() {
        return whole_file(cwd, &literal, &Selector::default());
    }
    let (base, sel) = selector::split(raw)?;
    let path = resolve(cwd, base);
    if path.is_file() {
        return whole_file(cwd, &path, &sel);
    }
    compound(cwd, base, &sel)
}

/// Apply a `?q=...` JSON query directly to fetched URL text (e.g. `ssh://host/data.json?q=.a`).
pub(crate) fn query_json_text(
    shown: &str,
    text: &str,
    query: &str,
    sel: &Selector,
) -> Result<ToolOutput> {
    let params = Params::parse(query)?;
    let out = json_query(text, &params)?;
    Ok(render_text(shown, &out, sel, None))
}

/// Apply rich-format reading or `?q=...` querying to an already-resolved file path (e.g. from
/// `attachment://`), returning `Ok(None)` when plain file/image reading should handle it.
pub(crate) fn read_resolved_path(
    path: &Path,
    shown: &str,
    query: Option<&str>,
    sel: &Selector,
) -> Result<Option<ToolOutput>> {
    if !path.is_file() {
        return Ok(None);
    }
    if let Some(q) = query {
        let lower = path.to_string_lossy().to_ascii_lowercase();
        let head = first_bytes(path, shown)?;
        if kind_for_name(&lower).is_some_and(|k| matches!(k, Kind::Sqlite))
            && sqlite::has_magic(&head)
        {
            return sqlite_selector(path, shown, &format!("?{q}"), sel).map(Some);
        }
        let bytes = read_capped(path, shown)?;
        let out = query_json_text(shown, &String::from_utf8_lossy(&bytes), q, sel)?;
        return Ok(Some(out));
    }
    whole_file_shown(path, shown, sel)
}

fn io_err(shown: &str, e: &std::io::Error) -> HrnError {
    HrnError::Tool(format!("cannot read `{shown}`: {e}"))
}

fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

fn first_bytes(path: &Path, shown: &str) -> Result<Vec<u8>> {
    let mut head = Vec::with_capacity(512);
    fs::File::open(path)
        .and_then(|f| f.take(512).read_to_end(&mut head))
        .map_err(|e| io_err(shown, &e))?;
    Ok(head)
}

fn read_capped(path: &Path, shown: &str) -> Result<Vec<u8>> {
    let len = fs::metadata(path).map_err(|e| io_err(shown, &e))?.len();
    if len > MEMBER_CAP {
        return Err(HrnError::Tool(format!(
            "`{shown}` is {len} bytes, over the {MEMBER_CAP}-byte read limit"
        )));
    }
    fs::read(path).map_err(|e| io_err(shown, &e))
}

/// A file addressed on its own: documents convert, archives and databases list.
fn whole_file(cwd: &Path, path: &Path, sel: &Selector) -> Result<Option<ToolOutput>> {
    let shown = display_rel(cwd, path);
    whole_file_shown(path, &shown, sel)
}

fn whole_file_shown(path: &Path, shown: &str, sel: &Selector) -> Result<Option<ToolOutput>> {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    let ext = ext_of(&lower);
    if ext == "pdf" || office::handles(&ext) || (ext == "ipynb" && !sel.raw) {
        let bytes = read_capped(path, shown)?;
        let text = document(&lower, &bytes)?.unwrap_or_default();
        return Ok(Some(render_text(shown, &text, sel, None)));
    }
    let Some(kind) = kind_for_name(&lower) else {
        return Ok(None);
    };
    let head = first_bytes(path, shown)?;
    match kind {
        Kind::Archive(format) if archive::matches_magic(format, &head) => {
            let members = archive::members(path, format)?;
            Ok(Some(listing(shown, &members, "", sel)))
        }
        Kind::Sqlite if sqlite::has_magic(&head) => {
            let text = sqlite::read(path, shown, sqlite::Target::List)?;
            Ok(Some(render_text(shown, &text, sel, None)))
        }
        _ => Ok(None),
    }
}

/// Container kind of a file name by its full extension (`.tar.gz` before `.gz`).
fn kind_for_name(lower: &str) -> Option<Kind> {
    archive::EXTENSIONS
        .iter()
        .find(|(ext, _)| lower.ends_with(ext))
        .map(|(_, f)| Kind::Archive(*f))
        .or_else(|| {
            sqlite::EXTENSIONS
                .iter()
                .any(|e| lower.ends_with(e))
                .then_some(Kind::Sqlite)
        })
}

/// Text of a PDF / Office / EPUB / notebook named `lower_name`; `None` for other names.
fn document(lower_name: &str, bytes: &[u8]) -> Result<Option<String>> {
    let ext = ext_of(lower_name);
    Ok(Some(match ext.as_str() {
        "pdf" => pdf::convert(bytes)?,
        "ipynb" => notebook::convert(&String::from_utf8_lossy(bytes))?,
        e if office::handles(e) => office::convert(e, bytes)?,
        _ => return Ok(None),
    }))
}

/// `container.ext` followed by `:inner`, `:table` or `?query`.
fn compound(cwd: &Path, base: &str, sel: &Selector) -> Result<Option<ToolOutput>> {
    let lower = base.to_ascii_lowercase();
    let mut table: Vec<(&str, Kind)> = archive::EXTENSIONS
        .iter()
        .map(|(e, f)| (*e, Kind::Archive(*f)))
        .collect();
    table.extend(sqlite::EXTENSIONS.iter().map(|e| (*e, Kind::Sqlite)));
    table.extend(JSON_EXTENSIONS.iter().map(|e| (*e, Kind::Json)));
    let mut best: Option<(usize, Kind, PathBuf)> = None;
    for (ext, kind) in table {
        for (at, _) in lower.match_indices(ext) {
            let end = at + ext.len();
            let next = lower.as_bytes().get(end).copied();
            let fits = match kind {
                Kind::Json => next == Some(b'?'),
                _ => matches!(next, None | Some(b':' | b'?')),
            };
            if !fits || best.as_ref().is_some_and(|b| b.0 <= end) {
                continue;
            }
            let path = resolve(cwd, base.get(..end).unwrap_or_default());
            if path.is_file() {
                best = Some((end, kind, path));
            }
        }
    }
    let Some((end, kind, path)) = best else {
        return Ok(None);
    };
    let shown = display_rel(cwd, &path);
    let rest = base.get(end..).unwrap_or_default();
    let head = first_bytes(&path, &shown)?;
    match kind {
        Kind::Archive(format) => {
            if !archive::matches_magic(format, &head) {
                return Err(HrnError::Tool(format!("`{shown}` is not a valid archive")));
            }
            archive_inner(&path, format, &shown, rest, sel).map(Some)
        }
        Kind::Sqlite => {
            if !sqlite::has_magic(&head) {
                return Err(HrnError::Tool(format!(
                    "`{shown}` is not a SQLite database"
                )));
            }
            sqlite_selector(&path, &shown, rest, sel).map(Some)
        }
        Kind::Json => {
            let params = Params::parse(rest.get(1..).unwrap_or_default())?;
            let bytes = read_capped(&path, &shown)?;
            let text = json_query(&String::from_utf8_lossy(&bytes), &params)?;
            Ok(Some(render_text(
                &format!("{shown}{rest}"),
                &text,
                sel,
                None,
            )))
        }
    }
}

fn listing(shown: &str, members: &[archive::Member], prefix: &str, sel: &Selector) -> ToolOutput {
    let mut rows = archive::children(members, prefix).unwrap_or_default();
    if rows.is_empty() {
        rows.push("(empty)".into());
    }
    let label = if prefix.is_empty() {
        shown.to_owned()
    } else {
        format!("{shown}:{prefix}")
    };
    paged_lines(&label, &rows, sel, ARCHIVE_LIST_LIMIT)
}

fn archive_inner(
    path: &Path,
    format: Format,
    shown: &str,
    rest: &str,
    sel: &Selector,
) -> Result<ToolOutput> {
    let members = archive::members(path, format)?;
    let Some(inner_full) = rest.strip_prefix(':') else {
        if rest.is_empty() {
            return Ok(listing(shown, &members, "", sel));
        }
        return Err(HrnError::Tool(
            "archive selectors look like `archive.zip:inner/path[?q=...]`".into(),
        ));
    };
    let is_file = |name: &str| members.iter().any(|m| m.name == name && !m.dir);
    // A member whose literal name contains `?` wins over the query reading.
    let whole = archive::normalize(inner_full)?;
    let (inner, query) = match inner_full.split_once('?') {
        Some((i, q)) if !is_file(&whole) => (archive::normalize(i)?, Some(q)),
        _ => (whole, None),
    };
    let label = format!("{shown}:{inner}");
    if !is_file(&inner) {
        return match archive::children(&members, &inner) {
            Some(_) if query.is_none() => Ok(listing(shown, &members, &inner, sel)),
            Some(_) => Err(HrnError::Tool(format!(
                "`{label}` is a directory; JSON queries need a file"
            ))),
            None => Err(HrnError::Tool(format!(
                "no entry `{inner}` in `{shown}`; read `{shown}` to list entries"
            ))),
        };
    }
    let bytes = archive::read_member(path, format, &inner)?;
    let lower = inner.to_ascii_lowercase();
    let text = if let Some(q) = query {
        json_query(&String::from_utf8_lossy(&bytes), &Params::parse(q)?)?
    } else if let Some(doc) = document_unless_raw(&lower, &bytes, sel)? {
        doc
    } else if bytes
        .get(..bytes.len().min(BINARY_PROBE_LEN))
        .is_some_and(|p| p.contains(&0))
    {
        return Ok(ToolOutput::ok(format!(
            "[Cannot read binary archive entry '{inner}' ({} bytes)]",
            bytes.len()
        )));
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let label = query.map_or(label.clone(), |q| format!("{label}?{q}"));
    Ok(render_text(&label, &text, sel, None))
}

/// Convert documents; `.ipynb` stays verbatim under `:raw`.
fn document_unless_raw(lower: &str, bytes: &[u8], sel: &Selector) -> Result<Option<String>> {
    if sel.raw && ext_of(lower) == "ipynb" {
        return Ok(None);
    }
    document(lower, bytes)
}

fn sqlite_selector(path: &Path, shown: &str, rest: &str, sel: &Selector) -> Result<ToolOutput> {
    let label = format!("{shown}{rest}");
    let text = if rest.is_empty() {
        sqlite::read(path, &label, sqlite::Target::List)?
    } else if let Some(query) = rest.strip_prefix('?') {
        let params = Params::parse(query)?;
        if params.q.is_none() {
            return Err(HrnError::Tool(
                "database queries need `?q=SELECT ...`; to read a table use `db:table`".into(),
            ));
        }
        sqlite::read(path, &label, sqlite::Target::Query(params))?
    } else if let Some(table) = rest.strip_prefix(':') {
        match table.split_once('?') {
            Some((name, query)) => {
                let params = Params::parse(query)?;
                sqlite::read(path, &label, sqlite::Target::Rows(name, params))?
            }
            None => match table.split_once(':') {
                Some((name, key)) => sqlite::read(path, &label, sqlite::Target::Row(name, key))?,
                None => sqlite::read(path, &label, sqlite::Target::Table(table))?,
            },
        }
    } else {
        return Err(HrnError::Tool(
            "database selectors: `db`, `db:table`, `db:table?limit=&offset=&order=col:desc`, \
             `db?q=SELECT ...`"
                .into(),
        ));
    };
    Ok(render_text(&label, &text, sel, None))
}

/// Run `params.q` over every JSON value in `text` (a document or JSONL lines).
fn json_query(text: &str, params: &Params) -> Result<String> {
    let program = jq::parse(params.q.as_deref().unwrap_or("."))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut outputs: Vec<Value> = Vec::new();
    for value in serde_json::Deserializer::from_str(text).into_iter::<Value>() {
        let value = value.map_err(|e| HrnError::Tool(format!("JSON parse error: {e}")))?;
        outputs.extend(program.run(value)?);
    }
    let paged = params.limit.is_some() || params.offset > 0;
    if !paged {
        return Ok(render_values(&outputs, params));
    }
    let single_array = matches!(outputs.as_slice(), [Value::Array(_)]);
    let items: Vec<Value> = match outputs.pop() {
        Some(Value::Array(items)) if single_array => items,
        Some(last) => {
            outputs.push(last);
            outputs
        }
        None => Vec::new(),
    };
    let total = items.len();
    let limit = params.limit.unwrap_or(total);
    let page: Vec<Value> = items.into_iter().skip(params.offset).take(limit).collect();
    let end = params.offset + page.len();
    let body = if single_array {
        render_values(&[Value::Array(page)], params)
    } else {
        render_values(&page, params)
    };
    if end < total {
        Ok(format!(
            "{body}\n[Showing items {}-{end} of {total}; continue with `&offset={end}`]",
            params.offset + 1
        ))
    } else {
        Ok(body)
    }
}

fn render_values(values: &[Value], params: &Params) -> String {
    if values.is_empty() {
        return "(no results)".into();
    }
    values
        .iter()
        .map(|v| match v {
            Value::String(s) if params.raw => s.clone(),
            _ if params.compact => v.to_string(),
            _ => serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
