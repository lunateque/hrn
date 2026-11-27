//! Schema v4 operations: recency by cwd, children, `/btw` topics, approval grants, traces, and vacuum.

use crate::{SESSION_SELECT, SqliteStore, row_to_session, store_err};
use hrn_kernel::{
    ApprovalGrant, HrnError, Result, SessionMeta, TitleSource, ToolTier, TraceRecord,
};
use rusqlite::types::Type;
use rusqlite::{OptionalExtension, params};

impl SqliteStore {
    pub(crate) fn rename_with_source(
        &self,
        session_id: &str,
        title: &str,
        source: TitleSource,
    ) -> Result {
        let conn = self.lock();
        let current: Option<Option<String>> = conn
            .query_row(
                "SELECT title_source FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| store_err("rename_session lookup", e))?;
        let Some(existing_source) = current else {
            return Err(HrnError::Store(format!("unknown session `{session_id}`")));
        };
        if source == TitleSource::Auto
            && existing_source.as_deref().and_then(TitleSource::parse) == Some(TitleSource::User)
        {
            return Ok(());
        }
        conn.execute(
            "UPDATE sessions SET title = ?2, title_source = ?3 WHERE id = ?1",
            params![session_id, title, source.as_str()],
        )
        .map_err(|e| store_err("rename_session", e))?;
        Ok(())
    }

    pub(crate) fn list_recent_for_cwd(&self, cwd: &str, limit: usize) -> Result<Vec<SessionMeta>> {
        let conn = self.lock();
        let lim = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {SESSION_SELECT}
                 FROM sessions
                 WHERE parent_session_id IS NULL AND cwd = ?1
                 ORDER BY updated_at_ms DESC, rowid DESC
                 LIMIT ?2"
            ))
            .map_err(|e| store_err("list_recent_for prepare", e))?;
        let rows = stmt
            .query_map(params![cwd, lim], row_to_session)
            .map_err(|e| store_err("list_recent_for query", e))?;
        rows.map(|row| row.map_err(|e| store_err("list_recent_for row", e)))
            .collect()
    }

    pub(crate) fn list_child_sessions(&self, parent_session_id: &str) -> Result<Vec<SessionMeta>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {SESSION_SELECT}
                 FROM sessions
                 WHERE parent_session_id = ?1
                 ORDER BY created_at_ms ASC, rowid ASC"
            ))
            .map_err(|e| store_err("list_children prepare", e))?;
        let rows = stmt
            .query_map(params![parent_session_id], row_to_session)
            .map_err(|e| store_err("list_children query", e))?;
        rows.map(|row| row.map_err(|e| store_err("list_children row", e)))
            .collect()
    }

    pub(crate) fn upsert_btw_topic(
        &self,
        session_id: &str,
        topic_id: &str,
        json: &str,
        updated_at_ms: i64,
    ) -> Result {
        self.lock()
            .execute(
                "INSERT INTO btw_topics (session_id, topic_id, json, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id, topic_id) DO UPDATE SET
                     json = excluded.json,
                     updated_at_ms = excluded.updated_at_ms",
                params![session_id, topic_id, json, updated_at_ms],
            )
            .map_err(|e| store_err("put_btw_topic", e))?;
        Ok(())
    }

    pub(crate) fn load_btw_topics(&self, session_id: &str) -> Result<Vec<(String, String, i64)>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT topic_id, json, updated_at_ms
                 FROM btw_topics
                 WHERE session_id = ?1
                 ORDER BY updated_at_ms DESC, topic_id ASC",
            )
            .map_err(|e| store_err("get_btw_topics prepare", e))?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|e| store_err("get_btw_topics query", e))?;
        rows.map(|row| row.map_err(|e| store_err("get_btw_topics row", e)))
            .collect()
    }

    pub(crate) fn upsert_approval_grant(&self, grant: &ApprovalGrant) -> Result {
        // Encode scope into the primary key so distinct scopes (`file`, `directory`, `tool`)
        // on the same path or name never overwrite one another.
        let storage_key = format!("{}:{}", grant.scope, grant.key);
        self.lock()
            .execute(
                "INSERT INTO approval_grants (key, tier, scope, created_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key) DO UPDATE SET
                     tier = excluded.tier,
                     scope = excluded.scope,
                     created_ms = excluded.created_ms",
                params![
                    storage_key,
                    grant.tier.as_str(),
                    grant.scope,
                    grant.created_ms
                ],
            )
            .map_err(|e| store_err("put_approval_grant", e))?;
        Ok(())
    }

    pub(crate) fn load_approval_grants(&self) -> Result<Vec<ApprovalGrant>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT key, tier, scope, created_ms
                 FROM approval_grants
                 ORDER BY created_ms ASC, key ASC",
            )
            .map_err(|e| store_err("list_approval_grants prepare", e))?;
        let rows = stmt
            .query_map([], |row| {
                let raw_key: String = row.get(0)?;
                let raw_tier: String = row.get(1)?;
                let scope: String = row.get(2)?;
                let tier = ToolTier::parse(&raw_tier).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        Type::Text,
                        format!("unknown tool tier `{raw_tier}`").into(),
                    )
                })?;
                let prefix = format!("{scope}:");
                let key = raw_key.strip_prefix(&prefix).unwrap_or(&raw_key).to_owned();
                Ok(ApprovalGrant {
                    key,
                    tier,
                    scope,
                    created_ms: row.get(3)?,
                })
            })
            .map_err(|e| store_err("list_approval_grants query", e))?;
        rows.map(|row| row.map_err(|e| store_err("list_approval_grants row", e)))
            .collect()
    }

    pub(crate) fn delete_all_approval_grants(&self) -> Result {
        self.lock()
            .execute("DELETE FROM approval_grants", [])
            .map_err(|e| store_err("clear_approval_grants", e))?;
        Ok(())
    }

    pub(crate) fn insert_trace(&self, trace: &TraceRecord) -> Result {
        let payload_json = serde_json::to_string(&trace.payload)?;
        self.lock()
            .execute(
                "INSERT INTO traces (session_id, node_id, kind, started_ms, dur_ms, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    trace.session_id,
                    trace.node_id,
                    trace.kind,
                    trace.started_ms,
                    trace.dur_ms,
                    payload_json,
                ],
            )
            .map_err(|e| store_err("record_trace", e))?;
        Ok(())
    }

    pub(crate) fn load_traces(&self, session_id: &str) -> Result<Vec<TraceRecord>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT session_id, node_id, kind, started_ms, dur_ms, payload_json
                 FROM traces
                 WHERE session_id = ?1
                 ORDER BY started_ms ASC, id ASC",
            )
            .map_err(|e| store_err("list_traces prepare", e))?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                let raw: String = row.get(5)?;
                let payload = serde_json::from_str(&raw).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(5, Type::Text, e.into())
                })?;
                Ok(TraceRecord {
                    session_id: row.get(0)?,
                    node_id: row.get(1)?,
                    kind: row.get(2)?,
                    started_ms: row.get(3)?,
                    dur_ms: row.get(4)?,
                    payload,
                })
            })
            .map_err(|e| store_err("list_traces query", e))?;
        rows.map(|row| row.map_err(|e| store_err("list_traces row", e)))
            .collect()
    }

    pub(crate) fn run_vacuum_and_checkpoint(&self) -> Result {
        self.lock()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")
            .map_err(|e| store_err("vacuum_and_checkpoint", e))
    }

    /// Open an existing `SQLite` store strictly read-only and immutable without running migrations,
    /// creating or mutating `-shm`/`-wal` sidecars, or touching disk timestamps (used by `hrn gc`
    /// dry-run).
    pub fn open_read_only(path: &std::path::Path) -> Result<Self> {
        let uri = crate::import::codex::readonly_immutable_uri(path);
        let conn = rusqlite::Connection::open_with_flags(
            &uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| store_err("open sqlite read-only", e))?;
        conn.execute_batch("PRAGMA query_only = ON;")
            .map_err(|e| store_err("set query_only", e))?;
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
        })
    }
}
