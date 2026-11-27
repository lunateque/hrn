//! Synthetic fixture tests for read-only Claude Code and Codex CLI transcript import.
//! NEVER reads the developer's real `~/.claude` or `~/.codex`.

use super::*;
use crate::SqliteStore;
use hrn_kernel::{DagPayload, FinishReason, Role, SessionStore, TitleSource};
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hrn-import-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_jsonl(path: &Path, lines: &[serde_json::Value]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    let body = lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{body}\n")).expect("write jsonl");
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, (u64, SystemTime)> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<PathBuf, (u64, SystemTime)>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("entry");
            let p = entry.path();
            let meta = fs::symlink_metadata(&p).expect("stat");
            if meta.is_dir() {
                walk(&p, base, out);
            } else {
                let rel = p.strip_prefix(base).expect("strip").to_path_buf();
                out.insert(rel, (meta.len(), meta.modified().expect("mtime")));
            }
        }
    }
    let mut map = BTreeMap::new();
    walk(root, root, &mut map);
    map
}

fn claude_tool_pair_records() -> Vec<serde_json::Value> {
    vec![
        json!({
            "type": "user",
            "uuid": "u0",
            "parentUuid": null,
            "isMeta": true,
            "cwd": "/work/demo",
            "timestamp": "2026-04-01T10:00:00Z",
            "message": {"role": "user", "content": "meta noise"}
        }),
        json!({
            "type": "user",
            "uuid": "u1",
            "parentUuid": "u0",
            "cwd": "/work/demo",
            "gitBranch": "main",
            "timestamp": "2026-04-01T10:00:01Z",
            "message": {"role": "user", "content": "Inspect Cargo.toml"}
        }),
        json!({
            "type": "assistant",
            "uuid": "a1",
            "parentUuid": "u1",
            "timestamp": "2026-04-01T10:00:02Z",
            "message": {
                "id": "msg_1",
                "role": "assistant",
                "model": "claude-sonnet-4-20250514",
                "stop_reason": "tool_use",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 40,
                    "cache_read_input_tokens": 500,
                    "cache_creation_input_tokens": 50
                },
                "content": [
                    {"type": "thinking", "thinking": "Check manifest first.", "signature": "sig_abc"},
                    {"type": "text", "text": "Reading Cargo.toml."},
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "Cargo.toml"}}
                ]
            }
        }),
        json!({
            "type": "assistant",
            "uuid": "side_1",
            "parentUuid": "a1",
            "isSidechain": true,
            "timestamp": "2026-04-01T10:00:03Z",
            "message": {"role": "assistant", "content": "sidechain ignored"}
        }),
        json!({
            "type": "user",
            "uuid": "u2",
            "parentUuid": "a1",
            "timestamp": "2026-04-01T10:00:04Z",
            "message": {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "[package]\nname = \"demo\"", "is_error": false}
                ]
            }
        }),
        json!({
            "type": "assistant",
            "uuid": "a2",
            "parentUuid": "u2",
            "timestamp": "2026-04-01T10:00:05Z",
            "message": {
                "id": "msg_2",
                "role": "assistant",
                "model": "claude-sonnet-4-20250514",
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 120, "output_tokens": 30},
                "content": [{"type": "text", "text": "Package is named demo."}]
            }
        }),
    ]
}

#[test]
fn import_claude_reconstructs_tool_pairs() {
    let tmp = TempDir::new("claude-tools");
    let claude_dir = tmp.path().join("claude");
    let session_file = claude_dir
        .join("projects")
        .join("-work-demo")
        .join("sess-uuid-1.jsonl");
    write_jsonl(&session_file, &claude_tool_pair_records());

    let store = SqliteStore::open_in_memory().expect("store");
    let outcome = import_claude(&store, &claude_dir, Some("sess-uuid-1")).expect("import");
    assert_eq!(outcome.source, ImportSource::Claude);
    assert_eq!(outcome.session.cwd, "/work/demo");
    assert_eq!(outcome.session.branch.as_deref(), Some("main"));
    assert_eq!(outcome.last_finish_reason, Some(FinishReason::Stop));
    assert_eq!(outcome.usage_records, 2);

    let head = outcome.session.head_node_id.as_deref().expect("head");
    let branch = store
        .load_branch(&outcome.session.id, head)
        .expect("branch");
    assert_eq!(
        branch.len(),
        4,
        "meta and sidechain entries must be skipped"
    );

    let DagPayload::Message { msg: m1, .. } = &branch[1].payload else {
        panic!("expected assistant message");
    };
    assert_eq!(m1.role, Role::Assistant);
    assert_eq!(m1.tool_calls.len(), 1);
    assert_eq!(m1.tool_calls[0].id, "toolu_1");
    assert_eq!(m1.tool_calls[0].name, "Read");
    assert_eq!(m1.thinking_blocks.len(), 1);
    assert_eq!(m1.thinking_blocks[0].signature.as_deref(), Some("sig_abc"));
    assert_eq!(m1.reasoning.as_deref(), Some("Check manifest first."));

    let DagPayload::Message { msg: m2, .. } = &branch[2].payload else {
        panic!("expected tool result");
    };
    assert_eq!(m2.role, Role::Tool);
    assert_eq!(m2.tool_call_id.as_deref(), Some("toolu_1"));
    assert_eq!(m2.tool_name.as_deref(), Some("Read"));
    assert!(m2.content.contains("name = \"demo\""));

    let total_usage = store.session_usage(&outcome.session.id).expect("usage");
    assert_eq!(total_usage.prompt_tokens, 650 + 120);
    assert_eq!(total_usage.completion_tokens, 70);
    assert_eq!(total_usage.cache_read_tokens, Some(500));
    assert_eq!(total_usage.cache_write_tokens, Some(50));
}

#[test]
fn import_claude_title_is_user() {
    let tmp = TempDir::new("claude-title");
    let claude_dir = tmp.path().join(".claude");
    let slug_dir = claude_dir.join("projects").join("-repo-app");
    fs::create_dir_all(&slug_dir).expect("mkdir");
    // `.claude.json` projects mapping resolves cwd without needing `cwd` inside JSONL.
    fs::write(
        tmp.path().join(".claude.json"),
        json!({"projects": {"/repo/app": {"allowedTools": []}}}).to_string(),
    )
    .expect("write .claude.json");
    fs::write(
        slug_dir.join("custom-title.json"),
        json!({"sess-title-1": "Custom Auth Refactor"}).to_string(),
    )
    .expect("write custom-title.json");

    write_jsonl(
        &slug_dir.join("sess-title-1.jsonl"),
        &[
            json!({
                "type": "ai-title",
                "aiTitle": "Auto Generated Title"
            }),
            json!({
                "type": "user",
                "uuid": "u1",
                "parentUuid": null,
                "timestamp": "2026-04-02T12:00:00Z",
                "message": {"role": "user", "content": "Refactor auth middleware"}
            }),
            json!({
                "type": "assistant",
                "uuid": "a1",
                "parentUuid": "u1",
                "timestamp": "2026-04-02T12:00:02Z",
                "message": {"role": "assistant", "content": "Done."}
            }),
        ],
    );

    let store = SqliteStore::open_in_memory().expect("store");
    let outcome = import_claude(&store, &claude_dir, Some("sess-title-1")).expect("import");
    assert_eq!(outcome.session.title, "Custom Auth Refactor");
    assert_eq!(outcome.session.title_source, Some(TitleSource::User));
    assert_eq!(outcome.session.cwd, "/repo/app");

    // Also verify `ai-title` alone yields `TitleSource::Auto`.
    write_jsonl(
        &slug_dir.join("sess-ai-1.jsonl"),
        &[
            json!({"type": "ai-title", "aiTitle": "Fix Cache Invalidation"}),
            json!({
                "type": "user",
                "uuid": "u1",
                "parentUuid": null,
                "timestamp": "2026-04-02T13:00:00Z",
                "message": {"role": "user", "content": "Fix cache"}
            }),
        ],
    );
    let ai_out = import_claude(&store, &claude_dir, Some("sess-ai-1")).expect("import ai");
    assert_eq!(ai_out.session.title, "Fix Cache Invalidation");
    assert_eq!(ai_out.session.title_source, Some(TitleSource::Auto));
}

#[test]
fn import_claude_fork_tree() {
    let tmp = TempDir::new("claude-fork");
    let claude_dir = tmp.path().join("claude");
    let session_file = claude_dir
        .join("projects")
        .join("-repo-fork")
        .join("sess-fork.jsonl");
    write_jsonl(
        &session_file,
        &[
            json!({
                "type": "user",
                "uuid": "root",
                "parentUuid": null,
                "cwd": "/repo/fork",
                "timestamp": "2026-04-03T10:00:00Z",
                "message": {"role": "user", "content": "Start"}
            }),
            json!({
                "type": "assistant",
                "uuid": "branch_a",
                "parentUuid": "root",
                "timestamp": "2026-04-03T10:00:01Z",
                "message": {"role": "assistant", "content": "Approach A"}
            }),
            json!({
                "type": "assistant",
                "uuid": "branch_b",
                "parentUuid": "root",
                "timestamp": "2026-04-03T10:00:05Z",
                "message": {"role": "assistant", "content": "Approach B"}
            }),
            json!({
                "type": "user",
                "uuid": "branch_b_child",
                "parentUuid": "branch_b",
                "timestamp": "2026-04-03T10:00:06Z",
                "message": {"role": "user", "content": "Continue B"}
            }),
        ],
    );

    let store = SqliteStore::open_in_memory().expect("store");
    let outcome = import_claude(&store, &claude_dir, None).expect("import");
    let tree = store.load_tree(&outcome.session.id).expect("tree");
    assert_eq!(
        tree.len(),
        4,
        "all forked branches must be preserved in DAG"
    );

    let root = tree.iter().find(|n| n.parent_id.is_none()).expect("root");
    let children_of_root: Vec<_> = tree
        .iter()
        .filter(|n| n.parent_id.as_deref() == Some(root.id.as_str()))
        .collect();
    assert_eq!(
        children_of_root.len(),
        2,
        "root must have two forked children"
    );

    let head = outcome.session.head_node_id.as_deref().expect("head");
    let active = store
        .load_branch(&outcome.session.id, head)
        .expect("active");
    assert_eq!(active.len(), 3, "active branch follows latest leaf");
}

#[test]
fn import_codex_rollback_and_compaction() {
    let tmp = TempDir::new("codex-rollback");
    let codex_dir = tmp.path().join("codex");
    let rollout = codex_dir
        .join("sessions")
        .join("2026")
        .join("04")
        .join("04")
        .join("rollout-2026-04-04-t1.jsonl");
    write_jsonl(
        &rollout,
        &[
            json!({
                "timestamp": "2026-04-04T09:00:00Z",
                "type": "session_meta",
                "payload": {"id": "codex-thread-1", "cwd": "/work/codex", "model": "gpt-5-codex"}
            }),
            json!({
                "timestamp": "2026-04-04T09:00:01Z",
                "type": "response_item",
                "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Turn one kept"}]}
            }),
            json!({
                "timestamp": "2026-04-04T09:00:02Z",
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reply one"}]}
            }),
            json!({
                "timestamp": "2026-04-04T09:00:03Z",
                "type": "turn_context",
                "payload": {"cwd": "/work/codex", "model": "gpt-5.4"}
            }),
            json!({
                "timestamp": "2026-04-04T09:00:04Z",
                "type": "compacted",
                "payload": {"message": "Earlier exploration folded"}
            }),
            json!({
                "timestamp": "2026-04-04T09:00:05Z",
                "type": "response_item",
                "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Turn two rolled back"}]}
            }),
            json!({
                "timestamp": "2026-04-04T09:00:06Z",
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reply two rolled back"}]}
            }),
            json!({
                "timestamp": "2026-04-04T09:00:07Z",
                "type": "event_msg",
                "payload": {"type": "thread_rolled_back", "num_turns": 1}
            }),
            json!({
                "timestamp": "2026-04-04T09:00:08Z",
                "type": "response_item",
                "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Turn three final"}]}
            }),
            json!({
                "timestamp": "2026-04-04T09:00:09Z",
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reply three final"}]}
            }),
        ],
    );

    let store = SqliteStore::open_in_memory().expect("store");
    let outcome = import_codex(&store, &codex_dir, Some("codex-thread-1")).expect("import");
    let head = outcome.session.head_node_id.as_deref().expect("head");
    let branch = store
        .load_branch(&outcome.session.id, head)
        .expect("branch");

    assert!(branch.iter().any(|n| matches!(
        &n.payload,
        DagPayload::ModelChange { selector, .. } if selector == "gpt-5.4"
    )));
    assert!(branch.iter().any(|n| matches!(
        &n.payload,
        DagPayload::Summary { text, .. } if text.starts_with("Imported Codex compaction")
    )));
    assert!(
        !branch.iter().any(|n| matches!(
            &n.payload,
            DagPayload::Message { msg, .. } if msg.content.contains("rolled back")
        )),
        "rolled-back turn must be dropped"
    );
    assert!(branch.iter().any(|n| matches!(
        &n.payload,
        DagPayload::Message { msg, .. } if msg.content == "Turn three final"
    )));
}

#[test]
fn import_twice_two_ids_sources_untouched() {
    let tmp = TempDir::new("codex-twice");
    let codex_dir = tmp.path().join("codex");
    let rollout = codex_dir
        .join("archived_sessions")
        .join("rollout-arch-1.jsonl");
    write_jsonl(
        &rollout,
        &[
            json!({
                "timestamp": "2026-04-05T10:00:00Z",
                "type": "session_meta",
                "payload": {"id": "arch-1", "cwd": "/work/arch"}
            }),
            json!({
                "timestamp": "2026-04-05T10:00:01Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Audit security fence"}
            }),
            json!({
                "timestamp": "2026-04-05T10:00:02Z",
                "type": "event_msg",
                "payload": {"type": "agent_message", "message": "Fence verified."}
            }),
        ],
    );
    // Create a synthetic `state_5.sqlite` threads table and close cleanly.
    let state_db = codex_dir.join("state_5.sqlite");
    {
        let conn = Connection::open(&state_db).expect("create state db");
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                title TEXT,
                cwd TEXT,
                updated_at INTEGER,
                archived INTEGER DEFAULT 0
            );",
        )
        .expect("schema");
        conn.execute(
            "INSERT INTO threads (id, rollout_path, title, cwd, updated_at, archived)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            rusqlite::params![
                "arch-1",
                "archived_sessions/rollout-arch-1.jsonl",
                "Archived Audit",
                "/work/arch",
                1_775_383_202_i64,
            ],
        )
        .expect("insert thread");
    }

    let before = snapshot_tree(&codex_dir);
    let store = SqliteStore::open_in_memory().expect("store");

    let first = import_codex(&store, &codex_dir, Some("arch-1")).expect("first import");
    let second = import_codex(&store, &codex_dir, Some("arch-1")).expect("second import");
    assert_ne!(
        first.session.id, second.session.id,
        "each import must allocate a fresh hrn session ID"
    );
    assert_eq!(first.session.title, "Archived Audit");
    assert_eq!(second.session.title, "Archived Audit");

    let after = snapshot_tree(&codex_dir);
    assert_eq!(
        before, after,
        "source files and state_*.sqlite must remain completely untouched (no WAL/SHM or mtime change)"
    );
}

#[test]
fn forbidden_credential_files_are_refused() {
    let tmp = TempDir::new("fence");
    let claude_dir = tmp.path().join("claude");
    fs::create_dir_all(&claude_dir).expect("mkdir");
    let cred = claude_dir.join(".credentials.json");
    fs::write(&cred, r#"{"access_token":"secret"}"#).expect("write");
    let store = SqliteStore::open_in_memory().expect("store");
    assert!(import_claude_file(&store, &claude_dir, &cred).is_err());

    let codex_dir = tmp.path().join("codex");
    fs::create_dir_all(&codex_dir).expect("mkdir");
    let auth = codex_dir.join("auth.json");
    fs::write(&auth, r#"{"token":"secret"}"#).expect("write");
    assert!(import_codex_file(&store, &codex_dir, &auth).is_err());

    let gemini_dir = tmp.path().join(".gemini");
    fs::create_dir_all(&gemini_dir).expect("mkdir");
    assert!(discover_claude(&gemini_dir).is_err());
    let omp_dir = tmp.path().join(".omp");
    fs::create_dir_all(&omp_dir).expect("mkdir");
    assert!(discover_codex(&omp_dir).is_err());
}
