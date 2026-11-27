//! Writing `SQLite` rows through `write`: `db.sqlite:table` inserts a row, `db.sqlite:table:key`
//! updates one, and `db.sqlite:table:key` with an empty body deletes it. Row bodies are JSON5
//! objects of scalars, and the receipt echoes the row in the shape `read db:table:key` shows.

use super::{json5, sqlite};
use crate::paths::display_rel;
use hrn_kernel::{HrnError, Result, ToolOutput};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params};
use serde_json::{Map, Value};
use std::path::Path;

fn err(what: impl Into<String>) -> HrnError {
    HrnError::Tool(what.into())
}

fn body_of(content: &str) -> Result<Map<String, Value>> {
    match json5::parse(content)? {
        Value::Object(map) => Ok(map),
        other => Err(err(format!(
            "a row body is a JSON5 object of scalars; got `{}`",
            other.to_string().chars().take(60).collect::<String>()
        ))),
    }
}

fn bind(value: &Value) -> Result<SqlValue> {
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                SqlValue::Real(f)
            } else {
                SqlValue::Text(n.to_string())
            }
        }
        Value::String(s) => SqlValue::Text(s.clone()),
        other => {
            let text = other.to_string();
            return Err(err(format!(
                "`{}` is not a scalar; a row body is a JSON5 object of numbers, strings, \
                 booleans and null",
                text.chars().take(60).collect::<String>()
            )));
        }
    })
}

fn check_columns(
    table: &str,
    map: &Map<String, Value>,
    cols: &[(String, String, usize)],
) -> Result {
    let known: Vec<&str> = cols.iter().map(|c| c.0.as_str()).collect();
    let missing: Vec<String> = map
        .keys()
        .filter(|k| !known.contains(&k.as_str()))
        .map(|k| format!("`{k}`"))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(err(format!(
        "table `{table}` has no column(s) {}; the columns are {}",
        missing.join(", "),
        known.join(", ")
    )))
}

fn echo(conn: &Connection, shown: &str, table: &str, key: &str) -> String {
    sqlite::row_of(conn, shown, table, key).map_or_else(
        |e| format!("(the row cannot be shown: {e})"),
        |row| format!("\n{row}"),
    )
}

/// The inserted row, addressed by the rowid `SQLite` returned.
fn echo_by_rowid(conn: &Connection, shown: &str, table: &str, rowid: i64) -> String {
    sqlite::row_of_rowid(conn, shown, table, rowid).map_or_else(
        |e| format!("(the row cannot be shown: {e})"),
        |row| format!("\n{row}"),
    )
}

fn miss(verb: &str, shown: &str, table: &str, key_col: &str, key: &str) -> ToolOutput {
    let mut out = ToolOutput::ok(format!(
        "No row with {key_col} = '{key}' in `{table}` of `{shown}`; nothing {verb}."
    ));
    out.is_error = true;
    out
}

/// Write the row of `table` addressed by `key` (`None` inserts; an empty body deletes).
///
/// # Errors
/// The database or table is missing, or the body is not an object of scalars. A key matching no
/// row comes back as a failed result rather than an error.
pub(super) fn write_row(
    cwd: &Path,
    path: &Path,
    table: &str,
    key: Option<&str>,
    content: &str,
) -> Result<ToolOutput> {
    let shown = display_rel(cwd, path);
    if !path.is_file() {
        return Err(err(format!("SQLite database `{shown}` not found")));
    }
    let conn = sqlite::write_open(path)?;
    sqlite::table_sql(&conn, table)?;
    let cols = sqlite::columns(&conn, table)?;
    match key {
        Some(key) if content.trim().is_empty() => delete(&conn, &shown, table, key, &cols),
        Some(key) => {
            let map = body_of(content)?;
            check_columns(table, &map, &cols)?;
            if map.is_empty() {
                return Err(err(
                    "an update body needs at least one column; an empty body deletes the row",
                ));
            }
            let (key_col, key_type) = sqlite::key_column(&cols)?;
            let sets: Vec<String> = map
                .keys()
                .enumerate()
                .map(|(i, k)| format!("{} = ?{}", sqlite::quote(k), i + 1))
                .collect();
            let mut values: Vec<SqlValue> = map.values().map(bind).collect::<Result<_>>()?;
            values.push(sqlite::key_value(key, &key_type));
            let refs: Vec<&dyn rusqlite::ToSql> =
                values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            let changed = conn
                .execute(
                    &format!(
                        "UPDATE {} SET {} WHERE {} = ?{}",
                        sqlite::quote(table),
                        sets.join(", "),
                        sqlite::quote(&key_col),
                        values.len()
                    ),
                    refs.as_slice(),
                )
                .map_err(|e| sqlite::db(&e))?;
            if changed == 0 {
                return Ok(miss("updated", &shown, table, &key_col, key));
            }
            Ok(ToolOutput::ok(format!(
                "Updated row '{key}' ({key_col}) in `{table}` of `{shown}` ({} column(s) \
                 set).{}",
                map.len(),
                echo(&conn, &shown, table, key)
            )))
        }
        None => {
            let map = if content.trim().is_empty() {
                Map::new()
            } else {
                body_of(content)?
            };
            check_columns(table, &map, &cols)?;
            let rowid = if map.is_empty() {
                conn.execute(
                    &format!("INSERT INTO {} DEFAULT VALUES", sqlite::quote(table)),
                    [],
                )
                .map_err(|e| sqlite::db(&e))?;
                conn.last_insert_rowid()
            } else {
                let names: Vec<String> = map.keys().map(|k| sqlite::quote(k)).collect();
                let marks = (1..=map.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let values: Vec<SqlValue> = map.values().map(bind).collect::<Result<_>>()?;
                let refs: Vec<&dyn rusqlite::ToSql> =
                    values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
                conn.execute(
                    &format!(
                        "INSERT INTO {} ({}) VALUES ({marks})",
                        sqlite::quote(table),
                        names.join(", ")
                    ),
                    refs.as_slice(),
                )
                .map_err(|e| sqlite::db(&e))?;
                conn.last_insert_rowid()
            };
            Ok(ToolOutput::ok(format!(
                "Inserted row into `{table}` of `{shown}` (rowid {rowid}, {} column(s) \
                 given).{}",
                map.len(),
                echo_by_rowid(&conn, &shown, table, rowid)
            )))
        }
    }
}

fn delete(
    conn: &Connection,
    shown: &str,
    table: &str,
    key: &str,
    cols: &[(String, String, usize)],
) -> Result<ToolOutput> {
    let (key_col, key_type) = sqlite::key_column(cols)?;
    let changed = conn
        .execute(
            &format!(
                "DELETE FROM {} WHERE {} = ?1",
                sqlite::quote(table),
                sqlite::quote(&key_col)
            ),
            params![sqlite::key_value(key, &key_type)],
        )
        .map_err(|e| sqlite::db(&e))?;
    if changed == 0 {
        return Ok(miss("deleted", shown, table, &key_col, key));
    }
    Ok(ToolOutput::ok(format!(
        "Deleted row '{key}' ({key_col}) from `{table}` of `{shown}`."
    )))
}
