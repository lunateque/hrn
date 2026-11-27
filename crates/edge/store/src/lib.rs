//! `SQLite` (WAL, bundled `rusqlite`) append-only DAG persistence implementing [`SessionStore`].

mod history;
pub mod import;
mod migrate;
mod usage;
mod v4;

pub use history::{HistoryAgentEntry, HistoryScheme, LiveAgentsFn};
pub use import::{
    ExternalSession, ImportOutcome, ImportSource, default_claude_dir, default_codex_dir,
    default_omp_dir, discover_all, discover_all_with_omp, discover_claude, discover_codex,
    discover_omp, discover_omp_limited, import_claude, import_claude_file, import_codex,
    import_codex_file, import_omp, import_omp_file,
};

use hrn_kernel::{
    AgentStatus, ApprovalGrant, DagNode, DagPayload, HrnError, PermissionMode, Result, SessionMeta,
    SessionStore, ThinkingLevel, TitleSource, TraceRecord, Usage, UsageRow,
};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

/// Thread-safe single-writer `SQLite` DAG session store.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

pub(crate) fn store_err(context: &str, err: impl std::fmt::Display) -> HrnError {
    HrnError::Store(format!("{context}: {err}"))
}

impl SqliteStore {
    /// Open or initialize a `SQLite` store at `path` (creates parent directories). Requires WAL
    /// journaling; a filesystem that cannot provide it is reported rather than degraded.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| HrnError::Io(format!("create `{}`: {e}", parent.display())))?;
        }
        let conn = Connection::open(path).map_err(|e| store_err("open sqlite", e))?;
        migrate::init(&conn, true)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory `SQLite` store for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| store_err("open sqlite", e))?;
        migrate::init(&conn, false)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

const SESSION_COLUMNS: &str = "id, title, cwd, branch, head_node_id, model, thinking, \
     permission_mode, parent_session_id, parent_node_id, status, created_at_ms, updated_at_ms, \
     goal, title_source, exit_ms";

/// [`SESSION_COLUMNS`] plus the derived pin flag: the `session_pins` table stays the truth.
pub(crate) const SESSION_SELECT: &str = "id, title, cwd, branch, head_node_id, model, thinking, \
     permission_mode, parent_session_id, parent_node_id, status, created_at_ms, updated_at_ms, \
     goal, title_source, exit_ms, EXISTS(SELECT 1 FROM session_pins WHERE session_id = sessions.id)";

impl SessionStore for SqliteStore {
    fn create_session(&self, meta: &SessionMeta) -> Result {
        let conn = self.lock();
        conn.execute(
            &format!(
                "INSERT INTO sessions ({SESSION_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
            ),
            params![
                meta.id,
                meta.title,
                meta.cwd,
                meta.branch,
                meta.head_node_id,
                meta.model,
                meta.thinking.as_str(),
                meta.permission_mode.as_str(),
                meta.parent_session_id,
                meta.parent_node_id,
                status_str(meta.status),
                meta.created_at_ms,
                meta.updated_at_ms,
                goal_json(meta.goal.as_ref())?,
                meta.title_source.map(TitleSource::as_str),
                meta.exit_ms,
            ],
        )
        .map_err(|e| store_err("create_session", e))?;
        Ok(())
    }

    fn update_session(&self, meta: &SessionMeta) -> Result {
        let conn = self.lock();
        let changed = conn
            .execute(
                "UPDATE sessions
                 SET title = ?2, cwd = ?3, branch = ?4, head_node_id = ?5, model = ?6,
                     thinking = ?7, permission_mode = ?8, status = ?9, updated_at_ms = ?10,
                     goal = ?11, title_source = ?12, exit_ms = ?13
                 WHERE id = ?1",
                params![
                    meta.id,
                    meta.title,
                    meta.cwd,
                    meta.branch,
                    meta.head_node_id,
                    meta.model,
                    meta.thinking.as_str(),
                    meta.permission_mode.as_str(),
                    status_str(meta.status),
                    meta.updated_at_ms,
                    goal_json(meta.goal.as_ref())?,
                    meta.title_source.map(TitleSource::as_str),
                    meta.exit_ms,
                ],
            )
            .map_err(|e| store_err("update_session", e))?;
        if changed == 0 {
            return Err(HrnError::Store(format!(
                "update_session: session `{}` does not exist",
                meta.id
            )));
        }
        Ok(())
    }

    fn get_session(&self, session_id: &str) -> Result<Option<SessionMeta>> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {SESSION_SELECT} FROM sessions WHERE id = ?1"),
            params![session_id],
            row_to_session,
        )
        .optional()
        .map_err(|e| store_err("get_session", e))
    }

    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMeta>> {
        let conn = self.lock();
        let lim = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {SESSION_SELECT}
                 FROM sessions LEFT JOIN session_pins p ON p.session_id = sessions.id
                 WHERE parent_session_id IS NULL
                 ORDER BY p.session_id IS NULL, updated_at_ms DESC, sessions.rowid DESC
                 LIMIT ?1"
            ))
            .map_err(|e| store_err("list_sessions prepare", e))?;
        let rows = stmt
            .query_map(params![lim], row_to_session)
            .map_err(|e| store_err("list_sessions query", e))?;
        rows.map(|row| row.map_err(|e| store_err("list_sessions row", e)))
            .collect()
    }

    fn delete_session(&self, session_id: &str) -> Result<bool> {
        // Subagent sessions point at their parent by a plain column, so they go with it; nodes,
        // usage and pins follow by their foreign keys.
        let removed = self
            .lock()
            .execute(
                "WITH RECURSIVE doomed(id) AS (
                    SELECT ?1
                    UNION
                    SELECT s.id FROM sessions s JOIN doomed d ON s.parent_session_id = d.id
                 )
                 DELETE FROM sessions WHERE id IN (SELECT id FROM doomed)",
                params![session_id],
            )
            .map_err(|e| store_err("delete_session", e))?;
        Ok(removed > 0)
    }

    fn rename_session(&self, session_id: &str, title: &str, source: TitleSource) -> Result {
        self.rename_with_source(session_id, title, source)
    }

    fn list_recent_for(&self, cwd: &str, limit: usize) -> Result<Vec<SessionMeta>> {
        self.list_recent_for_cwd(cwd, limit)
    }

    fn list_children(&self, parent_session_id: &str) -> Result<Vec<SessionMeta>> {
        self.list_child_sessions(parent_session_id)
    }

    fn put_btw_topic(
        &self,
        session_id: &str,
        topic_id: &str,
        json: &str,
        updated_at_ms: i64,
    ) -> Result {
        self.upsert_btw_topic(session_id, topic_id, json, updated_at_ms)
    }

    fn get_btw_topics(&self, session_id: &str) -> Result<Vec<(String, String, i64)>> {
        self.load_btw_topics(session_id)
    }

    fn put_approval_grant(&self, grant: &ApprovalGrant) -> Result {
        self.upsert_approval_grant(grant)
    }

    fn list_approval_grants(&self) -> Result<Vec<ApprovalGrant>> {
        self.load_approval_grants()
    }

    fn clear_approval_grants(&self) -> Result {
        self.delete_all_approval_grants()
    }

    fn record_trace(&self, trace: &TraceRecord) -> Result {
        self.insert_trace(trace)
    }

    fn list_traces(&self, session_id: &str) -> Result<Vec<TraceRecord>> {
        self.load_traces(session_id)
    }

    fn vacuum_and_checkpoint(&self) -> Result {
        self.run_vacuum_and_checkpoint()
    }

    fn set_pinned(&self, session_id: &str, pinned: bool) -> Result {
        let conn = self.lock();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(|e| store_err("set_pinned", e))?;
        if !exists {
            return Err(HrnError::Store(format!("unknown session `{session_id}`")));
        }
        let outcome = if pinned {
            conn.execute(
                "INSERT OR IGNORE INTO session_pins (session_id, pinned_at_ms)
                 VALUES (?1, CAST(strftime('%s', 'now') AS INTEGER) * 1000)",
                params![session_id],
            )
        } else {
            conn.execute(
                "DELETE FROM session_pins WHERE session_id = ?1",
                params![session_id],
            )
        };
        outcome.map_err(|e| store_err("set_pinned", e))?;
        Ok(())
    }

    fn pinned_sessions(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT session_id FROM session_pins ORDER BY pinned_at_ms, session_id")
            .map_err(|e| store_err("pinned_sessions prepare", e))?;
        let rows = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| store_err("pinned_sessions query", e))?;
        rows.map(|row| row.map_err(|e| store_err("pinned_sessions row", e)))
            .collect()
    }

    fn append_node(&self, node: &DagNode) -> Result {
        let payload_json = serde_json::to_string(&node.payload)?;
        let mut conn = self.lock();
        // Insert + head advance must be one atomic step: a crash in between would leave a node
        // that the session head never points at.
        let tx = conn
            .transaction()
            .map_err(|e| store_err("append_node begin", e))?;
        if let Some(parent) = &node.parent_id {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM dag_nodes WHERE id = ?1 AND session_id = ?2)",
                    params![parent, node.session_id],
                    |row| row.get(0),
                )
                .map_err(|e| store_err("append_node parent check", e))?;
            if !exists {
                return Err(HrnError::Store(format!(
                    "append_node: parent `{parent}` does not exist in session `{}`",
                    node.session_id
                )));
            }
        }
        tx.execute(
            "INSERT INTO dag_nodes (id, session_id, parent_id, created_at_ms, payload_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                node.id,
                node.session_id,
                node.parent_id,
                node.created_at_ms,
                payload_json,
            ],
        )
        .map_err(|e| store_err("append_node", e))?;
        let advanced = tx
            .execute(
                "UPDATE sessions
                 SET head_node_id = ?2, updated_at_ms = max(updated_at_ms, ?3)
                 WHERE id = ?1",
                params![node.session_id, node.id, node.created_at_ms],
            )
            .map_err(|e| store_err("advance session head", e))?;
        if advanced == 0 {
            return Err(HrnError::Store(format!(
                "append_node: session `{}` does not exist",
                node.session_id
            )));
        }
        tx.commit().map_err(|e| store_err("append_node commit", e))
    }

    fn load_branch(&self, session_id: &str, head_node_id: &str) -> Result<Vec<DagNode>> {
        let conn = self.lock();
        // `UNION` (not `UNION ALL`) deduplicates rows, so even a corrupted parent cycle
        // terminates; the chain is then ordered by walking parent links in Rust.
        let mut stmt = conn
            .prepare(
                "WITH RECURSIVE chain(id, session_id, parent_id, created_at_ms, payload_json) AS (
                    SELECT id, session_id, parent_id, created_at_ms, payload_json
                    FROM dag_nodes
                    WHERE id = ?1 AND session_id = ?2
                    UNION
                    SELECT p.id, p.session_id, p.parent_id, p.created_at_ms, p.payload_json
                    FROM dag_nodes p
                    JOIN chain c ON p.id = c.parent_id
                    WHERE p.session_id = ?2
                 )
                 SELECT id, session_id, parent_id, created_at_ms, payload_json FROM chain",
            )
            .map_err(|e| store_err("load_branch prepare", e))?;
        let nodes = collect_nodes(&mut stmt, params![head_node_id, session_id])?;
        order_branch(nodes, head_node_id)
    }

    fn load_tree(&self, session_id: &str) -> Result<Vec<DagNode>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, parent_id, created_at_ms, payload_json
                 FROM dag_nodes
                 WHERE session_id = ?1
                 ORDER BY created_at_ms ASC, rowid ASC",
            )
            .map_err(|e| store_err("load_tree prepare", e))?;
        collect_nodes(&mut stmt, params![session_id])
    }

    fn record_usage(&self, session_id: &str, at_ms: i64, model: &str, usage: &Usage) -> Result {
        self.insert_usage(session_id, at_ms, model, usage)
    }

    fn session_usage(&self, session_id: &str) -> Result<Usage> {
        self.sum_session_usage(session_id)
    }

    fn usage_report(&self) -> Result<Vec<UsageRow>> {
        self.report_usage(None)
    }

    fn session_usage_report(&self, session_id: &str) -> Result<Vec<UsageRow>> {
        self.report_usage(Some(session_id))
    }
}

/// Order the fetched ancestor set root-first by following `parent_id` from `head`.
fn order_branch(nodes: Vec<DagNode>, head: &str) -> Result<Vec<DagNode>> {
    let mut by_id: HashMap<String, DagNode> =
        nodes.into_iter().map(|n| (n.id.clone(), n)).collect();
    let mut chain = Vec::with_capacity(by_id.len());
    let mut seen = HashSet::new();
    let mut cursor = Some(head.to_owned());
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            return Err(HrnError::Store(format!("DAG cycle at node `{id}`")));
        }
        let Some(node) = by_id.remove(&id) else {
            return Err(HrnError::Store(if chain.is_empty() {
                format!("head node `{id}` not found in session")
            } else {
                format!("dangling parent link `{id}` in DAG")
            }));
        };
        let next = node.parent_id.clone();
        chain.push(node);
        cursor = next;
    }
    chain.reverse();
    Ok(chain)
}

fn collect_nodes(
    stmt: &mut rusqlite::Statement<'_>,
    p: impl rusqlite::Params,
) -> Result<Vec<DagNode>> {
    let rows = stmt
        .query_map(p, |row| {
            let id: String = row.get(0)?;
            let session_id: String = row.get(1)?;
            let parent_id: Option<String> = row.get(2)?;
            let created_at_ms: i64 = row.get(3)?;
            let payload_json: String = row.get(4)?;
            Ok((id, session_id, parent_id, created_at_ms, payload_json))
        })
        .map_err(|e| store_err("query nodes", e))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, session_id, parent_id, created_at_ms, payload_json) =
            row.map_err(|e| store_err("read node row", e))?;
        let payload: DagPayload = serde_json::from_str(&payload_json)
            .map_err(|e| store_err(&format!("decode payload of node `{id}`"), e))?;
        out.push(DagNode {
            id,
            session_id,
            parent_id,
            created_at_ms,
            payload,
        });
    }
    Ok(out)
}

fn enum_column<T>(
    row: &rusqlite::Row<'_>,
    idx: usize,
    what: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> rusqlite::Result<T> {
    let raw: String = row.get(idx)?;
    parse(&raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            Type::Text,
            format!("unknown {what} `{raw}`").into(),
        )
    })
}

/// Persist a goal as JSON for the `sessions.goal` column.
fn goal_json(goal: Option<&hrn_kernel::Goal>) -> Result<Option<String>> {
    goal.map(|g| serde_json::to_string(g).map_err(|e| store_err("serialize goal", e)))
        .transpose()
}

pub(crate) fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMeta> {
    let title_source = row
        .get::<_, Option<String>>(14)?
        .map(|raw| {
            TitleSource::parse(&raw).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    14,
                    Type::Text,
                    format!("unknown title source `{raw}`").into(),
                )
            })
        })
        .transpose()?;
    Ok(SessionMeta {
        id: row.get(0)?,
        title: row.get(1)?,
        cwd: row.get(2)?,
        branch: row.get(3)?,
        head_node_id: row.get(4)?,
        model: row.get(5)?,
        thinking: enum_column(row, 6, "thinking level", ThinkingLevel::parse)?,
        goal: row
            .get::<_, Option<String>>(13)?
            .map(|raw| {
                serde_json::from_str(&raw).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(13, Type::Text, e.into())
                })
            })
            .transpose()?,
        permission_mode: enum_column(row, 7, "permission mode", PermissionMode::parse)?,
        parent_session_id: row.get(8)?,
        parent_node_id: row.get(9)?,
        status: enum_column(row, 10, "status", parse_status)?,
        created_at_ms: row.get(11)?,
        updated_at_ms: row.get(12)?,
        title_source,
        exit_ms: row.get(15)?,
        pinned: row.get(16)?,
    })
}

const fn status_str(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Error => "error",
    }
}

fn parse_status(s: &str) -> Option<AgentStatus> {
    match s {
        "idle" => Some(AgentStatus::Idle),
        "working" => Some(AgentStatus::Working),
        "blocked" => Some(AgentStatus::Blocked),
        "error" => Some(AgentStatus::Error),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
