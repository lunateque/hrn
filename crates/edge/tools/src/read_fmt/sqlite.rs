//! `SQLite` databases: table list, schema + sample rows, paged table rows and read-only queries.

use super::params::Params;
use hrn_kernel::{HrnError, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, params};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

/// Database file extensions, checked together with the `SQLite format 3` header.
pub(super) const EXTENSIONS: &[&str] = &[".sqlite3", ".sqlite", ".db3", ".db"];
const SAMPLE_ROWS: usize = 5;
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 500;
const MAX_TABLES: usize = 500;
const MAX_COLUMN_WIDTH: usize = 40;

/// What to read from the database.
pub(super) enum Target<'a> {
    /// All tables with row counts.
    List,
    /// `:table` — schema and the first rows.
    Table(&'a str),
    /// `:table?limit=&offset=&order=`.
    Rows(&'a str, Params),
    /// `:table:key` — one row by its primary key (or `rowid`).
    Row(&'a str, &'a str),
    /// `?q=SELECT ...`.
    Query(Params),
}

/// `(column, declared type, primary-key position)` of a table.
pub(super) fn columns(conn: &Connection, table: &str) -> Result<Vec<(String, String, usize)>> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({})", quote(table)))
        .map_err(|e| db(&e))?;
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            usize::try_from(r.get::<_, i64>(5)?).unwrap_or(0),
        ))
    })
    .map_err(|e| db(&e))?
    .collect::<std::result::Result<_, _>>()
    .map_err(|e| db(&e))
}

/// The column a row key addresses: a single-column primary key, else `rowid`.
pub(super) fn key_column(cols: &[(String, String, usize)]) -> Result<(String, String)> {
    match cols
        .iter()
        .filter(|c| c.2 > 0)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => Ok(("rowid".to_owned(), "INTEGER".to_owned())),
        [one] => Ok((one.0.clone(), one.1.clone())),
        [..] => Err(HrnError::Tool(
            "the table has a composite primary key; `:table:key` needs a single-column key \
             (query the row with `?q=SELECT ...` instead)"
                .to_owned(),
        )),
    }
}

/// Bind `key` with the key column's affinity: an integer when the column is INTEGER-ish.
pub(super) fn key_value(key: &str, declared: &str) -> rusqlite::types::Value {
    if declared.to_ascii_uppercase().contains("INT")
        && let Ok(n) = key.trim().parse::<i64>()
    {
        return rusqlite::types::Value::Integer(n);
    }
    rusqlite::types::Value::Text(key.to_owned())
}

pub(super) fn db(e: &rusqlite::Error) -> HrnError {
    HrnError::Tool(format!("sqlite: {e}"))
}

/// Whether `head` is a `SQLite` file header.
pub(super) fn has_magic(head: &[u8]) -> bool {
    head.starts_with(b"SQLite format 3\0")
}

fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| db(&e))?;
    conn.busy_timeout(Duration::from_secs(3))
        .map_err(|e| db(&e))?;
    conn.pragma_update(None, "query_only", "ON")
        .map_err(|e| db(&e))?;
    Ok(conn)
}

/// Open an existing database read-write (never creating one) for `write db:table[:key]`.
pub(super) fn write_open(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| db(&e))?;
    conn.busy_timeout(Duration::from_secs(3))
        .map_err(|e| db(&e))?;
    Ok(conn)
}

pub(super) fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Read `target` from the database at `path`; `shown` names it in continuation hints.
pub(super) fn read(path: &Path, shown: &str, target: Target<'_>) -> Result<String> {
    let conn = open(path)?;
    match target {
        Target::List => list(&conn),
        Target::Table(name) => schema(&conn, name),
        Target::Rows(name, params) => table_rows(&conn, shown, name, &params),
        Target::Row(name, key) => row_of(&conn, shown, name, key),
        Target::Query(params) => {
            let sql = params
                .q
                .as_deref()
                .filter(|q| !q.trim().is_empty())
                .ok_or_else(|| HrnError::Tool("`?q=` needs a SQL statement".into()))?;
            query(&conn, shown, sql, params.limit, params.offset)
        }
    }
}

/// One row by key, as `column = value` lines; `shown` names the database in the miss message.
pub(super) fn row_of(conn: &Connection, shown: &str, table: &str, key: &str) -> Result<String> {
    table_sql(conn, table)?;
    let (key_col, key_type) = key_column(&columns(conn, table)?)?;
    one_row(
        conn,
        shown,
        table,
        &key_col,
        key,
        &key_value(key, &key_type),
    )
}

/// The row with `rowid`, as `column = value` lines.
pub(super) fn row_of_rowid(
    conn: &Connection,
    shown: &str,
    table: &str,
    rowid: i64,
) -> Result<String> {
    table_sql(conn, table)?;
    one_row(
        conn,
        shown,
        table,
        "rowid",
        &rowid.to_string(),
        &rusqlite::types::Value::Integer(rowid),
    )
}

/// One row where `column` equals `value`, rendered as `column = value` lines.
fn one_row(
    conn: &Connection,
    shown: &str,
    table: &str,
    column: &str,
    key: &str,
    value: &rusqlite::types::Value,
) -> Result<String> {
    let sql = format!(
        "SELECT * FROM {} WHERE {} = ?1",
        quote(table),
        quote(column)
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| db(&e))?;
    let names: Vec<String> = stmt.column_names().map(String::from).collect();
    let mut rows = stmt
        .query_map(params![value], |r| {
            (0..names.len())
                .map(|i| Ok(cell(r.get_ref(i)?)))
                .collect::<rusqlite::Result<Vec<String>>>()
        })
        .map_err(|e| db(&e))?;
    let first = match rows.next() {
        Some(values) => values.map_err(|e| db(&e))?,
        None => {
            return Ok(format!(
                "No row with {column} = '{key}' in `{table}` of `{shown}`."
            ));
        }
    };
    Ok(names
        .iter()
        .zip(first)
        .map(|(n, v)| format!("{n} = {v}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn list(conn: &Connection) -> Result<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name, type FROM sqlite_master WHERE type IN ('table','view') \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .map_err(|e| db(&e))?;
    let tables: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| db(&e))?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| db(&e))?;
    if tables.is_empty() {
        return Ok("(no tables)".into());
    }
    let mut out = String::new();
    for (name, kind) in tables.iter().take(MAX_TABLES) {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {}", quote(name)), [], |r| {
                r.get(0)
            })
            .map_err(|e| db(&e))?;
        let _ = writeln!(
            out,
            "{name}{} ({count} rows)",
            if kind == "view" { " [view]" } else { "" }
        );
    }
    if tables.len() > MAX_TABLES {
        let _ = writeln!(out, "… {} more tables", tables.len() - MAX_TABLES);
    }
    Ok(out)
}

pub(super) fn table_sql(conn: &Connection, name: &str) -> Result<String> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE name = ?1 AND type IN ('table','view')",
        [name],
        |r| r.get::<_, Option<String>>(0),
    )
    .map(Option::unwrap_or_default)
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => HrnError::Tool(format!(
            "no table `{name}`; read the database path alone to list tables"
        )),
        other => db(&other),
    })
}

fn schema(conn: &Connection, name: &str) -> Result<String> {
    let mut out = table_sql(conn, name)?;
    out.push_str("\n\n");
    let sql = format!("SELECT * FROM {} LIMIT {SAMPLE_ROWS}", quote(name));
    let (cols, rows, _) = collect(conn, &sql, SAMPLE_ROWS, 0)?;
    let _ = writeln!(out, "First {} rows:", rows.len());
    out.push_str(&render(&cols, &rows));
    Ok(out)
}

fn table_rows(conn: &Connection, shown: &str, name: &str, p: &Params) -> Result<String> {
    table_sql(conn, name)?;
    let mut sql = format!("SELECT * FROM {}", quote(name));
    if let Some(order) = &p.order {
        let (col, dir) = order.split_once(':').unwrap_or((order, "asc"));
        if !["asc", "desc"].contains(&dir.to_ascii_lowercase().as_str()) {
            return Err(HrnError::Tool(format!(
                "`order` direction must be asc or desc, not `{dir}`"
            )));
        }
        let known = conn
            .prepare(&format!("SELECT * FROM {} LIMIT 0", quote(name)))
            .map_err(|e| db(&e))?
            .column_names()
            .into_iter()
            .any(|c| c == col);
        if !known {
            return Err(HrnError::Tool(format!(
                "table `{name}` has no column `{col}`"
            )));
        }
        let _ = write!(sql, " ORDER BY {} {}", quote(col), dir.to_ascii_uppercase());
    }
    query(conn, shown, &sql, p.limit, p.offset)
}

/// Run one read-only statement and render a page of its rows.
fn query(
    conn: &Connection,
    shown: &str,
    sql: &str,
    limit: Option<usize>,
    offset: usize,
) -> Result<String> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let (cols, rows, more) = collect(conn, sql, limit, offset)?;
    let mut out = render(&cols, &rows);
    let _ = write!(out, "\n{} row(s), offset {offset}", rows.len());
    if more {
        let _ = write!(
            out,
            "; more rows exist. Next page: `{shown}` with `&limit={limit}&offset={}`",
            offset + rows.len()
        );
    }
    out.push('\n');
    Ok(out)
}

type Page = (Vec<String>, Vec<Vec<String>>, bool);

/// Column names, up to `limit` rows after skipping `offset`, and whether more rows follow.
fn collect(conn: &Connection, sql: &str, limit: usize, offset: usize) -> Result<Page> {
    let mut stmt = conn.prepare(sql).map_err(|e| db(&e))?;
    if !stmt.readonly() {
        return Err(HrnError::Tool(
            "only read-only statements can run through `read_file`".into(),
        ));
    }
    let cols: Vec<String> = stmt.column_names().map(String::from).collect();
    let mut cursor = stmt.query([]).map_err(|e| db(&e))?;
    let mut rows = Vec::new();
    let mut seen = 0usize;
    while let Some(row) = cursor.next().map_err(|e| db(&e))? {
        seen += 1;
        if seen <= offset {
            continue;
        }
        if rows.len() == limit {
            return Ok((cols, rows, true));
        }
        let mut cells = Vec::with_capacity(cols.len());
        for i in 0..cols.len() {
            cells.push(cell(row.get_ref(i).map_err(|e| db(&e))?));
        }
        rows.push(cells);
    }
    Ok((cols, rows, false))
}

fn cell(v: ValueRef<'_>) -> String {
    match v {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => format!("<blob {} bytes>", b.len()),
    }
}

fn clip(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    match flat.char_indices().nth(MAX_COLUMN_WIDTH) {
        Some((cut, _)) => format!("{}…", flat.get(..cut).unwrap_or_default()),
        None => flat,
    }
}

/// Pipe-separated table with a header rule.
fn render(cols: &[String], rows: &[Vec<String>]) -> String {
    let clipped: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| clip(c)).collect())
        .collect();
    let widths: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            clipped
                .iter()
                .filter_map(|r| r.get(i))
                .map(|c| c.chars().count())
                .chain([c.chars().count()])
                .max()
                .unwrap_or(1)
        })
        .collect();
    let line = |cells: &mut dyn Iterator<Item = &str>| {
        let parts: Vec<String> = cells
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        format!("| {} |", parts.join(" | "))
    };
    let mut out = line(&mut cols.iter().map(String::as_str));
    out.push('\n');
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    let _ = writeln!(out, "|-{}-|", rule.join("-|-"));
    for r in &clipped {
        out.push_str(&line(&mut r.iter().map(String::as_str)));
        out.push('\n');
    }
    out
}
