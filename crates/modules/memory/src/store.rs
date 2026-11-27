//! The memory database: one `SQLite` file with an FTS5 index, opened on first use.

use crate::fmt::now_ms;
use crate::project::GLOBAL;
use hrn_kernel::{HrnError, Result};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const SCHEMA_VERSION: i64 = 1;
const COLUMNS: &str = "m.id, m.scope, m.content, m.context, m.source, m.importance, m.status, \
                       m.replacement_id, m.session_id, m.cwd, m.created_ms, m.updated_ms";

/// One stored memory.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub(crate) id: String,
    /// [`GLOBAL`] or a project key.
    pub(crate) scope: String,
    pub(crate) content: String,
    pub(crate) context: String,
    /// `retain` or `learn`.
    pub(crate) source: String,
    pub(crate) importance: f64,
    /// `active` or `invalidated`.
    pub(crate) status: String,
    pub(crate) replacement_id: Option<String>,
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    pub(crate) created_ms: i64,
    pub(crate) updated_ms: i64,
}

impl Entry {
    pub(crate) fn is_global(&self) -> bool {
        self.scope == GLOBAL
    }

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            scope: row.get(1)?,
            content: row.get(2)?,
            context: row.get(3)?,
            source: row.get(4)?,
            importance: row.get(5)?,
            status: row.get(6)?,
            replacement_id: row.get(7)?,
            session_id: row.get(8)?,
            cwd: row.get(9)?,
            created_ms: row.get(10)?,
            updated_ms: row.get(11)?,
        })
    }
}

/// A memory to store (already redacted).
pub(crate) struct NewMemory<'a> {
    pub(crate) scope: &'a str,
    pub(crate) content: &'a str,
    pub(crate) context: &'a str,
    pub(crate) source: &'static str,
    pub(crate) importance: f64,
    pub(crate) session_id: &'a str,
    pub(crate) cwd: &'a str,
}

/// What `memory_edit` does to one memory.
pub(crate) enum Edit<'a> {
    /// Replace the text and/or importance.
    Update {
        content: Option<&'a str>,
        importance: Option<f64>,
    },
    /// Delete the row.
    Forget,
    /// Keep the row but stop recalling it, optionally naming its replacement.
    Invalidate { replacement_id: Option<&'a str> },
}

fn db_err(context: &str, err: impl std::fmt::Display) -> HrnError {
    HrnError::Store(format!("memory database: {context}: {err}"))
}

/// Aggregate counts and storage size for `/memory stats`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MemoryStats {
    pub(crate) exists: bool,
    pub(crate) db_bytes: u64,
    pub(crate) total_active: usize,
    pub(crate) project_active: usize,
    pub(crate) global_active: usize,
    pub(crate) invalidated: usize,
    pub(crate) latest_updated_ms: Option<i64>,
}

/// The memory database path plus its lazily opened connection. Nothing touches the disk until
/// a tool, the `memory://` scheme or the prompt injector actually needs a memory.
pub(crate) struct Memory {
    path: PathBuf,
    conn: Mutex<Option<Connection>>,
}

impl Memory {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            conn: Mutex::new(None),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Run `f` on the open database. With `create == false` a database that does not exist yet
    /// is not created and `f` does not run (`Ok(None)`): nothing was ever retained.
    fn with<R>(
        &self,
        create: bool,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<R>,
    ) -> Result<Option<R>> {
        let mut slot = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            if !create && !self.path.exists() {
                return Ok(None);
            }
            *slot = Some(self.open()?);
        }
        match slot.as_mut() {
            Some(conn) => f(conn).map(Some).map_err(|e| db_err("query", e)),
            None => Ok(None),
        }
    }

    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| HrnError::Io(format!("create `{}`: {e}", parent.display())))?;
        }
        let conn = Connection::open(&self.path).map_err(|e| db_err("open", e))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| db_err("set busy_timeout", e))?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| db_err("read schema version", e))?;
        if version > SCHEMA_VERSION {
            return Err(HrnError::Store(format!(
                "memory database schema v{version} is newer than this build (v{SCHEMA_VERSION})"
            )));
        }
        if version < SCHEMA_VERSION {
            conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))
                .map_err(|e| db_err("enable WAL", e))?;
            conn.execute_batch(&format!(
                "{SCHEMA}\nPRAGMA user_version = {SCHEMA_VERSION};"
            ))
            .map_err(|e| db_err("create schema", e))?;
        }
        Ok(conn)
    }

    /// Store `items`; an identical active memory in the same scope is refreshed instead of
    /// duplicated. Returns the ids in order.
    pub(crate) fn retain(&self, items: &[NewMemory<'_>]) -> Result<Vec<String>> {
        let now = now_ms();
        let ids = self.with(true, |conn| {
            let tx = conn.transaction()?;
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT id FROM memory WHERE scope = ?1 AND content = ?2 AND status = 'active'",
                        params![item.scope, item.content],
                        |r| r.get(0),
                    )
                    .optional()?;
                let id = match existing {
                    Some(id) => {
                        tx.execute(
                            "UPDATE memory SET importance = max(importance, ?2), \
                             context = CASE WHEN ?3 = '' THEN context ELSE ?3 END, updated_ms = ?4 \
                             WHERE id = ?1",
                            params![id, item.importance, item.context, now],
                        )?;
                        id
                    }
                    None => tx.query_row(
                        "INSERT INTO memory (scope, content, context, source, importance, \
                         session_id, cwd, created_ms, updated_ms) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8) RETURNING id",
                        params![
                            item.scope,
                            item.content,
                            item.context,
                            item.source,
                            item.importance,
                            item.session_id,
                            item.cwd,
                            now
                        ],
                        |r| r.get(0),
                    )?,
                };
                ids.push(id);
            }
            tx.commit()?;
            Ok(ids)
        })?;
        Ok(ids.unwrap_or_default())
    }

    /// Active memories of `scopes` matching the FTS expression `expr`, best first. Importance
    /// scales BM25, so a decisive lesson outranks an offhand note with the same words.
    pub(crate) fn search(&self, expr: &str, scopes: [&str; 2], limit: usize) -> Result<Vec<Entry>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let sql = format!(
            "SELECT {COLUMNS} FROM memory_fts JOIN memory m ON m.n = memory_fts.rowid \
             WHERE memory_fts MATCH ?1 AND m.status = 'active' AND m.scope IN (?2, ?3) \
             ORDER BY bm25(memory_fts, 1.0, 0.5) * (0.5 + m.importance) LIMIT ?4"
        );
        let rows = self.with(false, |conn| {
            let mut stmt = conn.prepare_cached(&sql)?;
            stmt.query_map(params![expr, scopes[0], scopes[1], limit], Entry::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.unwrap_or_default())
    }

    /// One memory of `scopes` by id, whatever its status.
    pub(crate) fn get(&self, id: &str, scopes: [&str; 2]) -> Result<Option<Entry>> {
        let sql = format!("SELECT {COLUMNS} FROM memory m WHERE m.id = ?1 AND m.scope IN (?2, ?3)");
        let row = self.with(false, |conn| {
            conn.query_row(&sql, params![id, scopes[0], scopes[1]], Entry::from_row)
                .optional()
        })?;
        Ok(row.flatten())
    }

    /// Active lessons (`learn`) of `scopes`, newest first.
    pub(crate) fn lessons(&self, scopes: [&str; 2], limit: usize) -> Result<Vec<Entry>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let sql = format!(
            "SELECT {COLUMNS} FROM memory m WHERE m.source = 'learn' AND m.status = 'active' \
             AND m.scope IN (?1, ?2) ORDER BY m.updated_ms DESC LIMIT ?3"
        );
        let rows = self.with(false, |conn| {
            let mut stmt = conn.prepare_cached(&sql)?;
            stmt.query_map(params![scopes[0], scopes[1], limit], Entry::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.unwrap_or_default())
    }

    /// Most recently updated active memories across `scopes`, newest first.
    pub(crate) fn list_recent(&self, scopes: [&str; 2], limit: usize) -> Result<Vec<Entry>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let sql = format!(
            "SELECT {COLUMNS} FROM memory m WHERE m.status = 'active' \
             AND m.scope IN (?1, ?2) ORDER BY m.updated_ms DESC, m.n DESC LIMIT ?3"
        );
        let rows = self.with(false, |conn| {
            let mut stmt = conn.prepare_cached(&sql)?;
            stmt.query_map(params![scopes[0], scopes[1], limit], Entry::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.unwrap_or_default())
    }

    /// Storage counts and timestamps without creating a missing database.
    pub(crate) fn stats(&self, project_scope: &str) -> Result<MemoryStats> {
        let meta = std::fs::metadata(&self.path).ok();
        let db_bytes = meta.as_ref().map_or(0, std::fs::Metadata::len);
        let exists = meta.is_some();
        let counts = self.with(false, |conn| {
            let total_active: i64 = conn.query_row(
                "SELECT COUNT(*) FROM memory WHERE status = 'active'",
                [],
                |r| r.get(0),
            )?;
            let project_active: i64 = conn.query_row(
                "SELECT COUNT(*) FROM memory WHERE status = 'active' AND scope = ?1",
                params![project_scope],
                |r| r.get(0),
            )?;
            let global_active: i64 = conn.query_row(
                "SELECT COUNT(*) FROM memory WHERE status = 'active' AND scope = ?1",
                params![GLOBAL],
                |r| r.get(0),
            )?;
            let invalidated: i64 = conn.query_row(
                "SELECT COUNT(*) FROM memory WHERE status != 'active'",
                [],
                |r| r.get(0),
            )?;
            let latest_updated_ms: Option<i64> =
                conn.query_row("SELECT MAX(updated_ms) FROM memory", [], |r| r.get(0))?;
            Ok((
                total_active,
                project_active,
                global_active,
                invalidated,
                latest_updated_ms,
            ))
        })?;
        let Some((total_active, project_active, global_active, invalidated, latest_updated_ms)) =
            counts
        else {
            return Ok(MemoryStats {
                exists,
                db_bytes,
                ..MemoryStats::default()
            });
        };
        Ok(MemoryStats {
            exists,
            db_bytes,
            total_active: usize::try_from(total_active).unwrap_or(0),
            project_active: usize::try_from(project_active).unwrap_or(0),
            global_active: usize::try_from(global_active).unwrap_or(0),
            invalidated: usize::try_from(invalidated).unwrap_or(0),
            latest_updated_ms,
        })
    }

    /// Delete every memory belonging to one of `scopes`; returns deleted row count.
    pub(crate) fn clear_scopes(&self, scopes: &[&str]) -> Result<usize> {
        if scopes.is_empty() {
            return Ok(0);
        }
        let removed = self.with(false, |conn| {
            let tx = conn.transaction()?;
            let mut total = 0usize;
            for scope in scopes {
                total += tx.execute("DELETE FROM memory WHERE scope = ?1", params![scope])?;
            }
            tx.commit()?;
            Ok(total)
        })?;
        Ok(removed.unwrap_or(0))
    }

    /// Apply `edit` to the memory `id` of `scopes`; `false` when there is no such memory.
    pub(crate) fn edit(&self, id: &str, scopes: [&str; 2], edit: &Edit<'_>) -> Result<bool> {
        let now = now_ms();
        let changed = self.with(false, |conn| match edit {
            Edit::Update {
                content,
                importance,
            } => conn.execute(
                "UPDATE memory SET content = coalesce(?4, content), \
                 importance = coalesce(?5, importance), updated_ms = ?6 \
                 WHERE id = ?1 AND scope IN (?2, ?3)",
                params![id, scopes[0], scopes[1], content, importance, now],
            ),
            Edit::Forget => conn.execute(
                "DELETE FROM memory WHERE id = ?1 AND scope IN (?2, ?3)",
                params![id, scopes[0], scopes[1]],
            ),
            Edit::Invalidate { replacement_id } => conn.execute(
                "UPDATE memory SET status = 'invalidated', replacement_id = ?4, updated_ms = ?5 \
                 WHERE id = ?1 AND scope IN (?2, ?3)",
                params![id, scopes[0], scopes[1], replacement_id, now],
            ),
        })?;
        Ok(changed.unwrap_or(0) > 0)
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memory (
    n INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE DEFAULT (lower(hex(randomblob(6)))),
    scope TEXT NOT NULL,
    content TEXT NOT NULL,
    context TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL,
    importance REAL NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    replacement_id TEXT,
    session_id TEXT NOT NULL,
    cwd TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS memory_scope ON memory (scope, status);
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    content, context, content = 'memory', content_rowid = 'n', tokenize = 'porter unicode61'
);
CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory BEGIN
    INSERT INTO memory_fts (rowid, content, context) VALUES (new.n, new.content, new.context);
END;
CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory BEGIN
    INSERT INTO memory_fts (memory_fts, rowid, content, context)
    VALUES ('delete', old.n, old.content, old.context);
END;
CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE OF content, context ON memory BEGIN
    INSERT INTO memory_fts (memory_fts, rowid, content, context)
    VALUES ('delete', old.n, old.content, old.context);
    INSERT INTO memory_fts (rowid, content, context) VALUES (new.n, new.content, new.context);
END;
";
