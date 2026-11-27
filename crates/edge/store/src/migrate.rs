//! Schema initialization and forward migrations (`PRAGMA user_version`) for [`crate::SqliteStore`].

use crate::store_err;
use hrn_kernel::{HrnError, Result};
use rusqlite::Connection;
use std::time::Duration;

/// Schema version stored in `PRAGMA user_version`; a newer database is refused instead of being
/// silently misread.
pub(crate) const SCHEMA_VERSION: i64 = 4;

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
            [column],
            |row| row.get(0),
        )
        .map_err(|e| store_err(&format!("probe {table}.{column}"), e))?;
    Ok(count > 0)
}

fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result {
    if !has_column(conn, table, column)? {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )
        .map_err(|e| store_err(&format!("add {table}.{column}"), e))?;
    }
    Ok(())
}

pub(crate) fn init(conn: &Connection, require_wal: bool) -> Result {
    // The busy timeout must be armed before anything that can take a lock (journal switch).
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| store_err("set busy_timeout", e))?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|e| store_err("enable WAL", e))?;
    if require_wal && !mode.eq_ignore_ascii_case("wal") {
        return Err(HrnError::Store(format!(
            "SQLite refused WAL journaling (got `{mode}`); move the session database to a local disk"
        )));
    }
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| store_err("read schema version", e))?;
    if version > SCHEMA_VERSION {
        return Err(HrnError::Store(format!(
            "session database schema v{version} is newer than this build (v{SCHEMA_VERSION})"
        )));
    }
    if (1..SCHEMA_VERSION).contains(&version) {
        // v3 added `sessions.goal`; v4 added `sessions.title_source` and `sessions.exit_ms`.
        ensure_column(conn, "sessions", "goal", "TEXT")?;
        ensure_column(conn, "sessions", "title_source", "TEXT")?;
        ensure_column(conn, "sessions", "exit_ms", "INTEGER")?;
    }
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;

         CREATE TABLE IF NOT EXISTS sessions (
             id TEXT PRIMARY KEY,
             title TEXT NOT NULL,
             cwd TEXT NOT NULL,
             branch TEXT,
             head_node_id TEXT,
             model TEXT,
             thinking TEXT NOT NULL DEFAULT 'medium',
             permission_mode TEXT NOT NULL DEFAULT 'default',
             parent_session_id TEXT,
             parent_node_id TEXT,
             status TEXT NOT NULL DEFAULT 'idle',
             created_at_ms INTEGER NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             goal TEXT,
             title_source TEXT,
             exit_ms INTEGER
         );

         CREATE TABLE IF NOT EXISTS dag_nodes (
             id TEXT PRIMARY KEY,
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             parent_id TEXT,
             created_at_ms INTEGER NOT NULL,
             payload_json TEXT NOT NULL
         );

         CREATE INDEX IF NOT EXISTS idx_dag_nodes_session ON dag_nodes(session_id, created_at_ms);
         CREATE TABLE IF NOT EXISTS usage (
             id INTEGER PRIMARY KEY,
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             at_ms INTEGER NOT NULL,
             model TEXT NOT NULL,
             prompt_tokens INTEGER NOT NULL,
             completion_tokens INTEGER NOT NULL,
             total_tokens INTEGER NOT NULL,
             cache_read_tokens INTEGER,
             cache_write_tokens INTEGER,
             reasoning_tokens INTEGER,
             cost_micros INTEGER
         );

         CREATE TABLE IF NOT EXISTS session_pins (
             session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
             pinned_at_ms INTEGER NOT NULL
         );

         CREATE TABLE IF NOT EXISTS btw_topics (
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             topic_id TEXT NOT NULL,
             json TEXT NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             PRIMARY KEY (session_id, topic_id)
         );

         CREATE TABLE IF NOT EXISTS approval_grants (
             key TEXT PRIMARY KEY,
             tier TEXT NOT NULL,
             scope TEXT NOT NULL,
             created_ms INTEGER NOT NULL
         );

         CREATE TABLE IF NOT EXISTS traces (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             node_id TEXT,
             kind TEXT NOT NULL,
             started_ms INTEGER NOT NULL,
             dur_ms INTEGER NOT NULL,
             payload_json TEXT NOT NULL
         );

         CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at_ms DESC);
         CREATE INDEX IF NOT EXISTS idx_sessions_cwd_updated ON sessions(cwd, updated_at_ms DESC);
         CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id);
         CREATE INDEX IF NOT EXISTS idx_usage_session ON usage(session_id);
         CREATE INDEX IF NOT EXISTS idx_usage_day ON usage(at_ms);
         CREATE INDEX IF NOT EXISTS idx_traces_session ON traces(session_id, started_ms);
         PRAGMA user_version = {SCHEMA_VERSION};"
    ))
    .map_err(|e| store_err("init sqlite schema", e))
}
