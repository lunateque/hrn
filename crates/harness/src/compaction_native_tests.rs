//! Acceptance tests for Wave 11i `CompactionNative`: `snapcompact` vision gating & image
//! projection, plus async speculative compaction arming, zero-LLM commit on threshold cross, and
//! branch-switch invalidation.

use crate::agents::{AgentRegistry, Spawn};
use crate::compaction::CompactionPolicy;
use crate::compaction::{CompactionConfig, in_speculative_band, speculative_lead_tokens};
use crate::control::SessionControl;
use crate::registry::ToolRegistry;
use crate::runtime::CompactionReason;
use crate::runtime::{AgentRuntime, HarnessConfig, TurnIo};
use crate::testutil::{MemFs, MemStore, VecEvents, block_on};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, ChunkSink, DagNode, DagPayload, EventSink, FinishReason,
    HrnError, ImageAttachment, LlmChunk, LlmProvider, LlmRequest, LlmResponse, ModelInfo, Msg,
    PermissionMode, ProviderCapabilities, Result, ScopedFuture, SessionMeta, SessionStore,
    SnapRenderer, ThinkingLevel,
};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct ManualSpawn(Mutex<Vec<BoxFuture<()>>>);

impl ManualSpawn {
    fn drain_all(&self) -> usize {
        let tasks: Vec<BoxFuture<()>> = std::mem::take(&mut *self.0.lock().expect("lock"));
        let count = tasks.len();
        for task in tasks {
            block_on(task);
        }
        count
    }
}

impl Spawn for ManualSpawn {
    fn spawn(&self, task: BoxFuture<()>) -> Result {
        self.0.lock().expect("lock").push(task);
        Ok(())
    }
}

struct NativeTestLlm {
    responses: Mutex<VecDeque<LlmResponse>>,
    requests: Mutex<Vec<LlmRequest>>,
    vision: bool,
    window: u64,
}

impl NativeTestLlm {
    fn new(responses: Vec<LlmResponse>, vision: bool, window: u64) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
            vision,
            window,
        })
    }
}

impl LlmProvider for NativeTestLlm {
    fn stream_chat<'a>(
        &'a self,
        request: &'a LlmRequest,
        _cancel: &'a CancelFlag,
        sink: &'a dyn ChunkSink,
    ) -> ScopedFuture<'a, Result<LlmResponse>> {
        self.requests.lock().expect("lock").push(request.clone());
        let next = self
            .responses
            .lock()
            .expect("lock")
            .pop_front()
            .ok_or_else(|| HrnError::Provider {
                message: "script exhausted".into(),
                retryable: false,
                retry_after_secs: None,
            });
        Box::pin(async move {
            if let Ok(resp) = &next
                && !resp.content.is_empty()
            {
                sink.on_chunk(LlmChunk::ContentDelta(resp.content.clone()))
                    .await;
            }
            next
        })
    }

    fn models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }

    fn capabilities(&self, _model: Option<&str>) -> ProviderCapabilities {
        ProviderCapabilities {
            tools: true,
            thinking: true,
            prompt_cache: true,
            context_window: self.window,
            max_context_window: None,
        }
    }

    fn supports_vision(&self, _model: Option<&str>) -> bool {
        self.vision
    }
}

struct StubSnapRenderer {
    calls: Mutex<Vec<Vec<String>>>,
}

impl StubSnapRenderer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
        })
    }
}

impl SnapRenderer for StubSnapRenderer {
    fn render_pages(&self, pages: &[String]) -> Result<Vec<ImageAttachment>> {
        self.calls.lock().expect("lock").push(pages.to_vec());
        Ok(pages
            .iter()
            .enumerate()
            .map(|(i, _)| ImageAttachment {
                media_type: "image/png".to_owned(),
                data_base64: format!("iVBORw0KGgoPAGE{i}"),
                label: Some(format!("snapcompact-page-{:02}.png", i + 1)),
                width: Some(800),
                height: Some(600),
            })
            .collect())
    }
}

fn ok_reply(text: &str) -> LlmResponse {
    LlmResponse {
        model_used: "mock".to_owned(),
        fallback_from: None,
        content: text.to_owned(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        usage: None,
        finish_reason: FinishReason::Stop,
    }
}

fn append_msg(store: &MemStore, sid: &str, id: &str, parent: Option<&str>, msg: Msg) {
    store
        .append_node(&DagNode {
            id: id.into(),
            session_id: sid.into(),
            parent_id: parent.map(Into::into),
            created_at_ms: 100,
            payload: DagPayload::Message {
                turn_id: "t0".into(),
                msg,
            },
        })
        .expect("append");
    let mut meta = store.get_session(sid).expect("get").expect("meta");
    meta.head_node_id = Some(id.into());
    store.update_session(&meta).expect("update");
}

fn seed_turns(store: &MemStore, sid: &str, turns: usize, chars_per_msg: usize) {
    let mut prev: Option<String> = None;
    for t in 0..turns {
        let u_id = format!("u{t}");
        let a_id = format!("a{t}");
        append_msg(
            store,
            sid,
            &u_id,
            prev.as_deref(),
            Msg::user(format!("turn {t} {}", "u".repeat(chars_per_msg))),
        );
        append_msg(
            store,
            sid,
            &a_id,
            Some(&u_id),
            Msg::assistant(format!("reply {t} {}", "a".repeat(chars_per_msg))),
        );
        prev = Some(a_id);
    }
}

fn make_io(events: &Arc<VecEvents>) -> TurnIo {
    TurnIo {
        cancel: CancelFlag::new(),
        events: Arc::clone(events) as Arc<dyn EventSink>,
        approval: None,
        ask: None,
    }
}

fn setup_async_fixture(
    title: &str,
    replies: Vec<LlmResponse>,
) -> (
    AgentRuntime,
    Arc<NativeTestLlm>,
    Arc<ManualSpawn>,
    SessionMeta,
) {
    let llm = NativeTestLlm::new(replies, false, 100_000);
    let store = Arc::new(MemStore::default());
    let spawner = Arc::new(ManualSpawn::default());
    let agents = Arc::new(AgentRegistry::new(Arc::clone(&spawner) as _));
    let cfg = CompactionConfig {
        threshold_tokens: 20_000,
        reserve_tokens: Some(4_000),
        keep_recent_tokens: 20_000,
        async_enabled: true,
        method_order: vec!["summary".into()],
        ..CompactionConfig::default()
    };
    let rt = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            compaction: CompactionPolicy::from_config(cfg),
            ..HarnessConfig::default()
        },
    )
    .with_agents(agents);
    let meta = rt
        .create_session(
            title,
            Path::new("/ws"),
            Some("m".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    // 6 turns * 2 msgs * 5_000 chars = 60_000 chars ~= 15_000 tokens -> inside [11_808, 20_000).
    seed_turns(&store, &meta.id, 6, 5_000);
    (rt, llm, spawner, meta)
}

#[test]
fn snapcompact_non_vision_advances() {
    let llm = NativeTestLlm::new(
        vec![ok_reply("Fallback text summary after snapcompact skipped.")],
        false,
        200_000,
    );
    let store = Arc::new(MemStore::default());
    let renderer = StubSnapRenderer::new();
    let cfg = CompactionConfig {
        method_order: vec!["snapcompact".into(), "summary".into()],
        ..CompactionConfig::default()
    };
    let rt = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            compaction: CompactionPolicy::from_config(cfg),
            ..HarnessConfig::default()
        },
    )
    .with_snap_renderer(Arc::clone(&renderer) as _);

    let meta = rt
        .create_session(
            "non-vision",
            Path::new("/ws"),
            Some("text-only".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    seed_turns(&store, &meta.id, 6, 4_000);

    let events = Arc::new(VecEvents::default());
    let sink: Arc<dyn EventSink> = Arc::clone(&events) as Arc<dyn EventSink>;
    let outcome = block_on(rt.compact_session(
        &meta.id,
        CompactionReason::Threshold,
        None,
        &CancelFlag::new(),
        &sink,
    ))
    .expect("walk ok");

    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("snapcompact") && n.contains("vision")),
        "expected snapcompact skip note: {:?}",
        outcome.notes
    );
    let commit = outcome.committed.expect("walk advanced to summary");
    assert_eq!(commit.method, "summary");
    assert!(
        renderer.calls.lock().expect("calls").is_empty(),
        "renderer must not be called for non-vision model"
    );
}

#[test]
fn snapcompact_vision_commits_and_projects_images() {
    let llm = NativeTestLlm::new(vec![ok_reply("seen snapshot archive")], true, 200_000);
    let store = Arc::new(MemStore::default());
    let renderer = StubSnapRenderer::new();
    let cfg = CompactionConfig {
        threshold_tokens: 15_000,
        reserve_tokens: Some(2_000),
        keep_recent_tokens: 4_000,
        async_enabled: false,
        method_order: vec!["snapcompact".into(), "summary".into()],
        ..CompactionConfig::default()
    };
    let rt = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            compaction: CompactionPolicy::from_config(cfg),
            ..HarnessConfig::default()
        },
    )
    .with_snap_renderer(Arc::clone(&renderer) as _);

    let meta = rt
        .create_session(
            "vision-snap",
            Path::new("/ws"),
            Some("claude-sonnet".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    seed_turns(&store, &meta.id, 8, 6_000);

    let events = Arc::new(VecEvents::default());
    let turn = block_on(rt.run_turn(
        &meta.id,
        "continue from snapshot",
        &SessionControl::default(),
        make_io(&events),
    ))
    .expect("turn ok");
    assert_eq!(turn.final_text, "seen snapshot archive");

    let captured = llm.requests.lock().expect("reqs");
    assert_eq!(
        captured.len(),
        1,
        "snapcompact must archive without an extra summarizer LLM call"
    );
    assert!(
        captured[0]
            .messages
            .iter()
            .any(|m| !m.images.is_empty() && m.images[0].media_type == "image/png"),
        "projected summary message must carry rendered PNG ImageAttachments"
    );
    assert!(events.0.lock().expect("events").iter().any(|e| matches!(
        e,
        AgentEvent::Compacted { method, .. } if method == "snapcompact"
    )));
}

#[test]
fn async_armed_commits_on_cross() {
    assert_eq!(speculative_lead_tokens(100_000), 12_500);
    assert_eq!(speculative_lead_tokens(20_000), 8_192);
    assert_eq!(speculative_lead_tokens(400_000), 32_000);
    assert!(in_speculative_band(15_000, 20_000));
    assert!(!in_speculative_band(20_000, 20_000));

    let (rt, llm, spawner, meta) = setup_async_fixture(
        "async-cross",
        vec![
            ok_reply("reply during band"),
            ok_reply("Precomputed speculative summary of turns 0..3."),
            ok_reply("reply after zero-latency commit"),
        ],
    );
    let events = Arc::new(VecEvents::default());
    let r1 = block_on(rt.run_turn(
        &meta.id,
        "keep going in band",
        &SessionControl::default(),
        make_io(&events),
    ))
    .expect("turn 1 ok");
    assert_eq!(r1.final_text, "reply during band");

    assert_eq!(spawner.drain_all(), 1, "one speculative pass must arm");
    assert!(rt.has_armed_speculation(&meta.id));
    assert_eq!(llm.requests.lock().expect("reqs").len(), 2);

    let ev_list = events.0.lock().expect("events").clone();
    assert!(
        ev_list
            .iter()
            .any(|e| matches!(e, AgentEvent::CompactionSpeculating { active: true }))
    );
    assert!(
        ev_list
            .iter()
            .any(|e| matches!(e, AgentEvent::CompactionSpeculating { active: false }))
    );

    let big_prompt = format!("cross threshold {}", "z".repeat(26_000));
    let r2 = block_on(rt.run_turn(
        &meta.id,
        &big_prompt,
        &SessionControl::default(),
        make_io(&events),
    ))
    .expect("turn 2 ok");
    assert_eq!(r2.final_text, "reply after zero-latency commit");

    let reqs = llm.requests.lock().expect("reqs");
    assert_eq!(
        reqs.len(),
        3,
        "crossing threshold must commit armed speculation with zero extra LLM calls"
    );
    assert!(reqs[2].messages.iter().any(|m| {
        m.content
            .contains("Precomputed speculative summary of turns 0..3.")
    }));
    assert!(!rt.has_armed_speculation(&meta.id));
    assert!(events.0.lock().expect("events").iter().any(|e| matches!(
        e,
        AgentEvent::Compacted { method, .. } if method == "summary"
    )));
}

#[test]
fn async_invalidated_by_branch_change() {
    let (rt, llm, spawner, meta) = setup_async_fixture(
        "async-branch",
        vec![
            ok_reply("reply before branch switch"),
            ok_reply("STALE speculative summary from old branch"),
            ok_reply("FRESH synchronous summary after branch switch"),
            ok_reply("reply on switched branch"),
        ],
    );
    let events = Arc::new(VecEvents::default());
    block_on(rt.run_turn(
        &meta.id,
        "enter speculative band",
        &SessionControl::default(),
        make_io(&events),
    ))
    .expect("turn 1 ok");
    assert_eq!(spawner.drain_all(), 1);
    assert!(rt.has_armed_speculation(&meta.id));

    rt.switch_head(&meta.id, "a4").expect("switch head");
    assert!(
        !rt.has_armed_speculation(&meta.id),
        "branch switch must immediately invalidate armed speculation"
    );

    let big_prompt = format!("new branch prompt {}", "q".repeat(40_000));
    let r2 = block_on(rt.run_turn(
        &meta.id,
        &big_prompt,
        &SessionControl::default(),
        make_io(&events),
    ))
    .expect("turn 2 ok");
    assert_eq!(r2.final_text, "reply on switched branch");

    let reqs = llm.requests.lock().expect("reqs");
    assert_eq!(
        reqs.len(),
        4,
        "invalidated speculation must fall through to fresh synchronous compaction"
    );
    assert!(
        !reqs[3]
            .messages
            .iter()
            .any(|m| m.content.contains("STALE speculative summary")),
        "stale summary must never be committed after branch change"
    );
    assert!(reqs[3].messages.iter().any(|m| {
        m.content
            .contains("FRESH synchronous summary after branch switch")
    }));
}
