//! Single-transaction `SQLite` commit for imported transcripts with fresh `hrn` session/node IDs.

use super::io::{
    MAX_IMPORT_META_CHARS, MAX_IMPORT_TITLE_CHARS, now_ms, sanitize_meta, sanitize_title,
};
use super::{ImportOutcome, ImportedNode, ImportedTranscript, ImportedUsage};
use crate::{SESSION_COLUMNS, SqliteStore, store_err};
use hrn_kernel::{
    AgentStatus, DagNode, DagPayload, PermissionMode, Result, SessionMeta, ThinkingLevel,
    TitleSource,
};
use rusqlite::{Transaction, params};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

static IMPORT_SEQ: AtomicU64 = AtomicU64::new(1);

fn fresh_id(prefix: &str) -> String {
    let seq = IMPORT_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{:x}_{:x}_{seq}", now_ms(), std::process::id())
}

fn stored_u64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Commit a parsed transcript into `store` with fresh IDs inside one atomic `SQLite` transaction.
pub(crate) fn commit_import(
    store: &SqliteStore,
    transcript: ImportedTranscript,
) -> Result<ImportOutcome> {
    let session_id = fresh_id("sess_imp");
    let (meta, dag_nodes) = remap_transcript(&session_id, &transcript);
    let usage = transcript.usage;

    let mut conn = store.lock();
    let tx = conn
        .transaction()
        .map_err(|e| store_err("import begin", e))?;
    insert_session_row(&tx, &meta)?;
    insert_dag_nodes(&tx, &dag_nodes)?;
    insert_usage_rows(&tx, &session_id, &usage)?;
    tx.commit().map_err(|e| store_err("import commit", e))?;

    Ok(ImportOutcome {
        session: meta,
        source: transcript.source,
        source_id: sanitize_meta(&transcript.source_id, MAX_IMPORT_META_CHARS),
        node_count: dag_nodes.len(),
        usage_records: usage.len(),
        last_finish_reason: transcript.last_finish_reason,
    })
}

fn remap_transcript(
    session_id: &str,
    transcript: &ImportedTranscript,
) -> (SessionMeta, Vec<DagNode>) {
    let mut remap: HashMap<&str, String> = HashMap::with_capacity(transcript.nodes.len());
    for node in &transcript.nodes {
        remap.insert(node.local_id.as_str(), fresh_id("node_imp"));
    }
    let head_node_id = transcript
        .head_local_id
        .as_deref()
        .and_then(|id| remap.get(id).cloned())
        .or_else(|| {
            transcript
                .nodes
                .last()
                .and_then(|n| remap.get(n.local_id.as_str()).cloned())
        });
    let clean_cwd = sanitize_meta(&transcript.cwd, MAX_IMPORT_META_CHARS);
    let clean_branch = transcript
        .branch
        .as_deref()
        .map(|b| sanitize_meta(b, MAX_IMPORT_META_CHARS))
        .filter(|b| !b.is_empty());
    let meta = SessionMeta {
        id: session_id.to_owned(),
        title: sanitize_title(&transcript.title, MAX_IMPORT_TITLE_CHARS),
        cwd: if clean_cwd.is_empty() {
            ".".to_owned()
        } else {
            clean_cwd
        },
        branch: clean_branch,
        head_node_id,
        model: transcript.model.clone(),
        thinking: ThinkingLevel::default(),
        goal: None,
        permission_mode: PermissionMode::Default,
        parent_session_id: None,
        parent_node_id: None,
        status: transcript.status,
        pinned: false,
        title_source: transcript.title_source,
        exit_ms: None,
        created_at_ms: transcript.created_at_ms,
        updated_at_ms: transcript.updated_at_ms,
    };
    let dag_nodes = build_dag_nodes(session_id, &transcript.nodes, &remap);
    (meta, dag_nodes)
}

fn build_dag_nodes(
    session_id: &str,
    nodes: &[ImportedNode],
    remap: &HashMap<&str, String>,
) -> Vec<DagNode> {
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        let Some(new_id) = remap.get(node.local_id.as_str()).cloned() else {
            continue;
        };
        let parent_id = node
            .parent_local_id
            .as_deref()
            .and_then(|p| remap.get(p).cloned());
        let payload = match &node.payload {
            DagPayload::Summary {
                covers_node_id,
                text,
                method,
                preserve,
            } => DagPayload::Summary {
                covers_node_id: remap
                    .get(covers_node_id.as_str())
                    .cloned()
                    .unwrap_or_else(|| new_id.clone()),
                text: text.clone(),
                method: method.clone(),
                preserve: preserve.clone(),
            },
            other => other.clone(),
        };
        out.push(DagNode {
            id: new_id,
            session_id: session_id.to_owned(),
            parent_id,
            created_at_ms: node.created_at_ms,
            payload,
        });
    }
    out
}

fn insert_session_row(tx: &Transaction<'_>, meta: &SessionMeta) -> Result {
    let status = match meta.status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Error => "error",
    };
    tx.execute(
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
            status,
            meta.created_at_ms,
            meta.updated_at_ms,
            Option::<String>::None,
            meta.title_source.map(TitleSource::as_str),
            meta.exit_ms,
        ],
    )
    .map_err(|e| store_err("import insert session", e))?;
    Ok(())
}

fn insert_dag_nodes(tx: &Transaction<'_>, dag_nodes: &[DagNode]) -> Result {
    for node in dag_nodes {
        let payload_json = serde_json::to_string(&node.payload)?;
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
        .map_err(|e| store_err("import insert node", e))?;
    }
    Ok(())
}

fn insert_usage_rows(tx: &Transaction<'_>, session_id: &str, usage: &[ImportedUsage]) -> Result {
    for row in usage {
        tx.execute(
            "INSERT INTO usage (session_id, at_ms, model, prompt_tokens, completion_tokens,
                 total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                 cost_micros)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id,
                row.at_ms,
                row.model,
                stored_u64(row.usage.prompt_tokens),
                stored_u64(row.usage.completion_tokens),
                stored_u64(row.usage.total_tokens),
                row.usage.cache_read_tokens.map(stored_u64),
                row.usage.cache_write_tokens.map(stored_u64),
                row.usage.reasoning_tokens.map(stored_u64),
                row.usage.cost_micros.map(stored_u64),
            ],
        )
        .map_err(|e| store_err("import insert usage", e))?;
    }
    Ok(())
}
