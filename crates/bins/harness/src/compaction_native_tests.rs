//! End-to-end Wave 11i `CompactionNative` acceptance tests wired with `LlmRouter`, `SqliteStore`,
//! `PngSnapRenderer`, and loopback HTTP stubs.

use hrn_edge_http::HttpClient;
use hrn_edge_llm::{LlmConfig, LlmRouter, ModelPreset, ProviderEntry, ProviderKind};
use hrn_edge_oauth::OAuthManager;
use hrn_edge_store::SqliteStore;
use hrn_edge_tools::{PngSnapRenderer, StdWorkspaceFs};
use hrn_harness::{
    AgentRuntime, CompactionConfig, CompactionPolicy, HarnessConfig, SessionControl, ToolRegistry,
    TurnIo,
};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, DagNode, DagPayload, EventSink, Msg, PermissionMode,
    SessionMeta, SessionStore, ThinkingLevel,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Default)]
struct CaptureEvents(Mutex<Vec<AgentEvent>>);

impl EventSink for CaptureEvents {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        self.0.lock().expect("lock").push(event);
        Box::pin(async { true })
    }
}

struct CapturedHttp {
    head: String,
    body: String,
}

fn header_val<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then_some(v.trim())
    })
}

async fn serve_requests(
    replies: Vec<(&'static str, &'static str, String)>,
) -> (String, Arc<Mutex<Vec<CapturedHttp>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        for (status, content_type, reply_body) in replies {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head, body) = loop {
                let n = sock.read(&mut chunk).await.expect("read");
                assert!(n > 0, "connection closed before full request");
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Some((h, b)) = text.split_once("\r\n\r\n") {
                    let expected = header_val(h, "content-length")
                        .map_or(0, |v| v.parse::<usize>().expect("content-length"));
                    if b.len() >= expected {
                        break (h.to_owned(), b[..expected].to_owned());
                    }
                }
            };
            log.lock().expect("log").push(CapturedHttp { head, body });
            let raw = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{reply_body}",
                reply_body.len()
            );
            sock.write_all(raw.as_bytes()).await.expect("write");
            sock.shutdown().await.ok();
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn build_codex_runtime(
    base: String,
    token_path: &Path,
) -> (AgentRuntime, Arc<SqliteStore>, SessionMeta) {
    let mut llm_cfg = LlmConfig::default();
    llm_cfg.providers.insert(
        "openai".into(),
        ProviderEntry {
            kind: ProviderKind::OpenaiResponses,
            base_url: Some(base),
            token_file: Some(token_path.to_string_lossy().into_owned()),
            ..ProviderEntry::default()
        },
    );
    llm_cfg.presets.insert(
        "codex".into(),
        ModelPreset {
            provider: "openai".into(),
            id: "gpt-5.3-codex".into(),
            context_window: Some(128_000),
            ..ModelPreset::default()
        },
    );
    llm_cfg.roles.insert("default".into(), "codex".into());

    let router = Arc::new(
        LlmRouter::try_new(
            llm_cfg,
            HttpClient::new().expect("http"),
            OAuthManager::default(),
        )
        .expect("router"),
    );
    let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
    let comp_cfg = CompactionConfig {
        threshold_tokens: 2_000,
        reserve_tokens: Some(400),
        keep_recent_tokens: 800,
        async_enabled: false,
        method_order: vec!["remote".into(), "summary".into()],
        ..CompactionConfig::default()
    };
    let rt = AgentRuntime::new(
        router,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(StdWorkspaceFs),
        HarnessConfig {
            compaction: CompactionPolicy::from_config(comp_cfg),
            ..HarnessConfig::default()
        },
    )
    .with_snap_renderer(Arc::new(PngSnapRenderer));
    let mut meta = rt
        .create_session(
            "remote-compact",
            Path::new("."),
            Some("codex".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    seed_turns(&store, &mut meta, 6, 1_200);
    (rt, store, meta)
}

fn seed_turns(store: &SqliteStore, meta: &mut SessionMeta, turns: i64, chars: usize) {
    let mut parent: Option<String> = None;
    for t in 0..turns {
        let u_id = format!("u{t}");
        let a_id = format!("a{t}");
        store
            .append_node(&DagNode {
                id: u_id.clone(),
                session_id: meta.id.clone(),
                parent_id: parent.clone(),
                created_at_ms: 100 + t,
                payload: DagPayload::Message {
                    turn_id: "seed".into(),
                    msg: Msg::user(format!("user turn {t} {}", "u".repeat(chars))),
                },
            })
            .expect("append user");
        store
            .append_node(&DagNode {
                id: a_id.clone(),
                session_id: meta.id.clone(),
                parent_id: Some(u_id),
                created_at_ms: 101 + t,
                payload: DagPayload::Message {
                    turn_id: "seed".into(),
                    msg: Msg::assistant(format!("assistant turn {t} {}", "a".repeat(chars))),
                },
            })
            .expect("append assistant");
        parent = Some(a_id);
    }
    meta.head_node_id = parent;
    store.update_session(meta).expect("update head");
}

fn temp_token_file() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "hrn-11i-remote-stub-tok-{}.txt",
        std::process::id()
    ));
    std::fs::write(&path, "stub-bearer-token").expect("write token");
    path
}

#[tokio::test]
async fn remote_compaction_codex_stub() {
    let compact_json = serde_json::json!({
        "id": "cmp_resp_42",
        "object": "response.compaction",
        "output": [{
            "type": "compaction",
            "id": "cmp_item_42",
            "encrypted_content": "enc_opaque_codex_state_999"
        }],
        "summary": "Compact summary of earlier turns."
    })
    .to_string();
    let sse_reply = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"replayed ok\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":50,\"output_tokens\":4}}}\n\n",
    )
    .to_owned();

    let (base, seen) = serve_requests(vec![
        ("200 OK", "application/json", compact_json),
        ("200 OK", "text/event-stream", sse_reply),
    ])
    .await;
    let token_path = temp_token_file();
    let (rt, store, meta) = build_codex_runtime(base, &token_path);

    let events = Arc::new(CaptureEvents::default());
    let io = TurnIo {
        cancel: CancelFlag::new(),
        events: Arc::clone(&events) as Arc<dyn EventSink>,
        approval: None,
        ask: None,
    };
    let turn = rt
        .run_turn(&meta.id, "continue work", &SessionControl::default(), io)
        .await
        .expect("turn ok");
    assert_eq!(turn.final_text, "replayed ok");

    // 1. Verify DagPayload::Summary persisted with method = "remote" and opaque preserve state.
    let tree = store.load_tree(&meta.id).expect("tree");
    let (summary_text, preserve) = tree
        .iter()
        .find_map(|n| match &n.payload {
            DagPayload::Summary {
                method,
                preserve,
                text,
                ..
            } if method.as_deref() == Some("remote") => Some((text.as_str(), preserve.as_ref())),
            _ => None,
        })
        .expect("remote summary node persisted");
    assert_eq!(summary_text, "Compact summary of earlier turns.");
    assert_eq!(
        preserve.expect("preserve")["output"][0]["encrypted_content"],
        "enc_opaque_codex_state_999"
    );
    assert!(events.0.lock().expect("events").iter().any(|e| matches!(
        e,
        AgentEvent::Compacted { method, .. } if method == "remote"
    )));

    // 2. Verify HTTP traffic: POST /responses/compact followed by replay in POST /responses.
    let captured = seen.lock().expect("seen");
    assert_eq!(captured.len(), 2);
    assert!(
        captured[0]
            .head
            .starts_with("POST /responses/compact HTTP/1.1"),
        "{}",
        captured[0].head
    );
    let compact_req: Value = serde_json::from_str(&captured[0].body).expect("compact req");
    assert_eq!(compact_req["model"], "gpt-5.3-codex");
    assert!(
        compact_req["input"]
            .as_array()
            .is_some_and(|arr| !arr.is_empty())
    );

    assert!(
        captured[1].head.starts_with("POST /responses HTTP/1.1"),
        "{}",
        captured[1].head
    );
    let replay_req: Value = serde_json::from_str(&captured[1].body).expect("replay req");
    let replayed_input = replay_req["input"].as_array().expect("input array");
    assert!(
        replayed_input.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("compaction")
                && item.get("encrypted_content").and_then(Value::as_str)
                    == Some("enc_opaque_codex_state_999")
        }),
        "next /responses request must replay opaque compaction item: {replay_req}"
    );
    let _ = std::fs::remove_file(&token_path);
}
