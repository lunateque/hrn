//! Unit and security tests for read-only OMP transcript discovery and DAG import.

use super::*;
use hrn_kernel::{DagPayload, FinishReason, Role, SessionStore, TitleSource};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hrn-omp-import-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn sample_omp_transcript() -> String {
    [
        r#"{"type":"title","v":1,"title":"Inspect build pipeline","source":"auto","updatedAt":"2026-04-01T10:00:05.000Z"}"#,
        r#"{"type":"session","version":3,"id":"omp-sess-alpha-01","timestamp":"2026-04-01T10:00:00.000Z","cwd":"/workspace/demo"}"#,
        r#"{"type":"model_change","id":"mc1","parentId":null,"timestamp":"2026-04-01T10:00:00.500Z","provider":"anthropic","modelId":"claude-sonnet-4-6"}"#,
        r#"{"type":"message","id":"m1","parentId":"mc1","timestamp":"2026-04-01T10:00:01.000Z","message":{"role":"user","content":"Check Cargo.toml"}}"#,
        r#"{"type":"custom","id":"skip1","parentId":"m1","timestamp":"2026-04-01T10:00:01.500Z","customType":"note","data":{}}"#,
        r#"{"type":"message","id":"m2","parentId":"skip1","timestamp":"2026-04-01T10:00:02.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-4-6","stopReason":"toolUse","usage":{"input":100,"output":40,"cacheRead":20,"cacheWrite":10,"totalTokens":170,"cost":{"total":0.0025}},"content":[{"type":"thinking","thinking":"Inspect manifest first","thinkingSignature":"sig-abc"},{"type":"text","text":"Reading Cargo.toml"},{"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"Cargo.toml"}}]}}"#,
        r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-04-01T10:00:03.000Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"read","content":[{"type":"text","text":"[workspace]"}],"isError":false}}"#,
        r#"{"type":"compaction","id":"cmp1","parentId":"m3","timestamp":"2026-04-01T10:00:03.500Z","summary":"Checked Cargo.toml workspace manifest."}"#,
        r#"{"type":"message","id":"m4","parentId":"cmp1","timestamp":"2026-04-01T10:00:04.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-4-6","stopReason":"stop","content":[{"type":"text","text":"Workspace looks healthy."}]}}"#,
        r#"{"type":"title_change","id":"tc1","parentId":"m4","timestamp":"2026-04-01T10:00:05.000Z","title":"User Renamed Pipeline","source":"user"}"#,
    ]
    .join("\n")
}

#[test]
fn discovers_and_imports_omp_v3_dag_read_only() {
    let root = scratch_dir("v3");
    let slug = root.join("sessions").join("--workspace-demo--");
    fs::create_dir_all(&slug).expect("slug dir");
    let file = slug.join("2026-04-01T10-00-00-000Z_omp-sess-alpha-01.jsonl");
    let raw = sample_omp_transcript();
    fs::write(&file, &raw).expect("write transcript");

    let discovered = discover_omp(&root).expect("discover omp");
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].source, ImportSource::Omp);
    assert_eq!(discovered[0].id, "omp-sess-alpha-01");
    assert_eq!(discovered[0].title, "User Renamed Pipeline");
    assert_eq!(discovered[0].cwd, "/workspace/demo");

    let store = SqliteStore::open_in_memory().expect("store");
    let outcome = import_omp(&store, &root, Some("omp-sess")).expect("import by prefix");
    assert_eq!(outcome.source, ImportSource::Omp);
    assert_eq!(outcome.source_id, "omp-sess-alpha-01");
    assert_eq!(outcome.session.title, "User Renamed Pipeline");
    assert_eq!(outcome.session.title_source, Some(TitleSource::User));
    assert_eq!(outcome.session.cwd, "/workspace/demo");
    assert_eq!(
        outcome.session.model.as_deref(),
        Some("anthropic/claude-sonnet-4-6")
    );
    assert_eq!(outcome.last_finish_reason, Some(FinishReason::Stop));
    assert_eq!(outcome.node_count, 5);
    assert_eq!(outcome.usage_records, 1);

    let head = outcome.session.head_node_id.as_deref().expect("head");
    let branch = store
        .load_branch(&outcome.session.id, head)
        .expect("branch");
    assert_eq!(
        branch.len(),
        5,
        "skipped custom record must bridge parentId"
    );
    assert!(matches!(
        &branch[0].payload,
        DagPayload::Message { msg, .. } if msg.role == Role::User && msg.content == "Check Cargo.toml"
    ));
    assert!(matches!(
        &branch[1].payload,
        DagPayload::Message { msg, .. }
            if msg.role == Role::Assistant
            && msg.tool_calls.len() == 1
            && msg.thinking_blocks.len() == 1
            && msg.thinking_blocks[0].signature.as_deref() == Some("sig-abc")
    ));
    assert!(matches!(
        &branch[2].payload,
        DagPayload::Message { msg, .. } if msg.role == Role::Tool && msg.content == "[workspace]"
    ));
    assert!(matches!(
        &branch[3].payload,
        DagPayload::Summary { text, .. } if text == "Checked Cargo.toml workspace manifest."
    ));
    assert!(matches!(
        &branch[4].payload,
        DagPayload::Message { msg, .. } if msg.role == Role::Assistant && msg.content == "Workspace looks healthy."
    ));

    let usage = store.session_usage(&outcome.session.id).expect("usage");
    assert_eq!(usage.prompt_tokens, 130);
    assert_eq!(usage.completion_tokens, 40);
    assert_eq!(usage.cache_read_tokens, Some(20));
    assert_eq!(usage.cache_write_tokens, Some(10));
    assert_eq!(usage.cost_micros, Some(2500));

    // Re-importing produces a fresh hrn session id and leaves source untouched.
    let second = import_omp(&store, &root, None).expect("import latest");
    assert_ne!(second.session.id, outcome.session.id);
    assert_eq!(fs::read_to_string(&file).expect("read back"), raw);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn omp_discovery_honors_limit_and_sanitizes_control_sequences() {
    let root = scratch_dir("limit");
    let slug = root.join("sessions").join("proj");
    fs::create_dir_all(&slug).expect("slug");
    for i in 1..=5 {
        let p = slug.join(format!("2026-04-0{i}T00-00-00Z_id-{i}.jsonl"));
        let line = format!(
            r#"{{"type":"session","version":3,"id":"id-{i}","timestamp":"2026-04-0{i}T00:00:00Z","cwd":"/repo/\u001b[31m{i}"}}
{{"type":"title","v":1,"title":"\u001b]0;spoof\u0007Clean {i}","source":"auto"}}"#
        );
        fs::write(&p, line).expect("write");
    }
    let limited = discover_omp_limited(&root, 2).expect("limited discover");
    assert_eq!(limited.len(), 2);
    for row in &limited {
        assert!(!row.title.contains('\x1b'));
        assert!(!row.title.contains('\x07'));
        assert!(!row.cwd.contains('\x1b'));
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn omp_security_confinement_refuses_credentials_dbs_and_foreign_roots() {
    let root = scratch_dir("sec");
    let store = SqliteStore::open_in_memory().expect("store");
    let env_file = root.join(".env");
    let db_file = root.join("agent.db");
    let gw_dir = root.join("auth-gateway-tokens");
    let gemini = root.join(".gemini");
    fs::create_dir_all(&gw_dir).expect("gw");
    fs::create_dir_all(&gemini).expect("gemini");
    fs::write(&env_file, "SECRET=1").expect("env");
    fs::write(&db_file, "sqlite").expect("db");
    let gw_token = gw_dir.join("foo.jsonl");
    fs::write(&gw_token, "{}").expect("gw token");
    let not_jsonl = root.join("config.yml");
    fs::write(&not_jsonl, "theme: dark").expect("yml");

    assert!(discover_omp(&gemini).is_err());
    assert!(import_omp_file(&store, &root, &env_file).is_err());
    assert!(import_omp_file(&store, &root, &db_file).is_err());
    assert!(import_omp_file(&store, &root, &gw_token).is_err());
    assert!(import_omp_file(&store, &root, &not_jsonl).is_err());

    // Claude and Codex importers must STILL reject .omp roots.
    let dot_omp = root.join(".omp");
    fs::create_dir_all(&dot_omp).expect("dot omp");
    assert!(discover_claude(&dot_omp).is_err());
    assert!(discover_codex(&dot_omp).is_err());

    let _ = fs::remove_dir_all(&root);
}
