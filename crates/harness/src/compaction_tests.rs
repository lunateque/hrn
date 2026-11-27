//! Acceptance tests for Wave 10d: `[compaction]` settings, reserve floor, model thresholds,
//! method walk (`shake` -> `summary`), no-progress guard, and custom `/compact [instructions]`.
//! Overflow, promotion, `Length` stop, `mid_turn_enabled`, and idle tests live in
//! `compaction_recovery_tests.rs`.

use crate::compaction::CompactionConfig;
use crate::compaction::CompactionPolicy;
use crate::control::SessionControl;
use crate::registry::ToolRegistry;
use crate::runtime::CompactionReason;
use crate::runtime::{AgentRuntime, HarnessConfig, TurnIo};
use crate::testutil::{MemFs, MemStore, VecEvents, block_on};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, ChunkSink, DagNode, DagPayload, EventSink, FinishReason,
    HrnError, LlmChunk, LlmProvider, LlmRequest, LlmResponse, ModelInfo, Msg, PermissionMode,
    ProviderCapabilities, Result, Role, ScopedFuture, SessionStore, ThinkingLevel, Tool,
    ToolApproval, ToolCall, ToolContext, ToolEffect, ToolOutput, ToolSpec, UrlBody, UrlRouter,
    UrlScheme,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[path = "compaction_recovery_tests.rs"]
mod recovery;

pub(super) struct CompactionTestLlm {
    pub(super) responses: Mutex<VecDeque<Result<LlmResponse>>>,
    pub(super) requests: Mutex<Vec<LlmRequest>>,
    pub(super) models: Vec<ModelInfo>,
    pub(super) windows: BTreeMap<String, u64>,
    pub(super) default_window: u64,
    pub(super) promotion_targets: BTreeMap<String, String>,
}

impl CompactionTestLlm {
    pub(super) fn new(responses: Vec<Result<LlmResponse>>, default_window: u64) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
            models: Vec::new(),
            windows: BTreeMap::new(),
            default_window,
            promotion_targets: BTreeMap::new(),
        })
    }
}

impl LlmProvider for CompactionTestLlm {
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
            .unwrap_or_else(|| {
                Err(HrnError::Provider {
                    message: "script exhausted".into(),
                    retryable: false,
                    retry_after_secs: None,
                })
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
        self.models.clone()
    }

    fn capabilities(&self, model: Option<&str>) -> ProviderCapabilities {
        let context_window = model
            .and_then(|m| self.windows.get(m).copied())
            .unwrap_or(self.default_window);
        ProviderCapabilities {
            tools: true,
            thinking: true,
            prompt_cache: true,
            context_window,
            max_context_window: None,
        }
    }

    fn is_context_overflow(&self, err: &HrnError) -> bool {
        let low = err.to_string().to_ascii_lowercase();
        low.contains("context_length_exceeded") || low.contains("prompt is too long")
    }

    fn context_promotion_target(&self, model: Option<&str>) -> Option<String> {
        model.and_then(|m| self.promotion_targets.get(m).cloned())
    }
}

struct MemoryLocalScheme {
    seq: AtomicUsize,
    spilled: Mutex<BTreeMap<String, String>>,
}

impl Default for MemoryLocalScheme {
    fn default() -> Self {
        Self {
            seq: AtomicUsize::new(1),
            spilled: Mutex::new(BTreeMap::new()),
        }
    }
}

impl UrlScheme for MemoryLocalScheme {
    fn scheme(&self) -> &'static str {
        "local"
    }

    fn about(&self) -> &'static str {
        "in-memory spill store"
    }

    fn read(&self, _ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        self.spilled
            .lock()
            .expect("lock")
            .get(rest)
            .cloned()
            .map(UrlBody::Text)
            .ok_or_else(|| HrnError::Tool(format!("missing local://{rest}")))
    }

    fn spill(&self, _ctx: &ToolContext, content: &str) -> Result<String> {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let key = format!("{n}.log");
        self.spilled
            .lock()
            .expect("lock")
            .insert(key.clone(), content.to_owned());
        Ok(format!("local://{key}"))
    }
}

pub(super) struct BigOutputTool {
    pub(super) payload: String,
}

impl Tool for BigOutputTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "big_tool".to_owned(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }

    fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
        let body = self.payload.clone();
        Box::pin(async move { Ok(ToolOutput::ok(body)) })
    }
}

pub(super) fn ok_resp(content: &str, finish: FinishReason) -> LlmResponse {
    LlmResponse {
        content: content.to_owned(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        usage: None,
        model_used: "mock".to_owned(),
        fallback_from: None,
        finish_reason: finish,
    }
}

pub(super) fn tool_call_resp(id: &str, name: &str) -> LlmResponse {
    LlmResponse {
        content: String::new(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: "{}".to_owned(),
            signature: None,
        }],
        usage: None,
        model_used: "mock".to_owned(),
        fallback_from: None,
        finish_reason: FinishReason::ToolCalls,
    }
}

pub(super) fn append_msg(store: &MemStore, sid: &str, id: &str, parent: Option<&str>, msg: Msg) {
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

pub(super) fn seed_turns(store: &MemStore, sid: &str, turns: usize, chars_per_msg: usize) {
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

pub(super) fn make_io(events: &Arc<VecEvents>) -> TurnIo {
    TurnIo {
        cancel: CancelFlag::new(),
        events: Arc::clone(events) as Arc<dyn EventSink>,
        approval: None,
        ask: None,
    }
}

fn tc(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
        signature: None,
    }
}

fn seed_shake_branch(store: &MemStore, sid: &str) -> (String, String, String) {
    let big = "log_line\n".repeat(13_500);
    let recent = "recent_line\n".repeat(6_500);
    let skill_body = "skill instructions ".repeat(120);
    let plan_body = "plan step details ".repeat(120);

    let mut skill_msg = Msg::tool_result("c_skill", "read", &skill_body);
    skill_msg.read_key = Some("skill://rust/SKILL.md".into());
    let mut plan_msg = Msg::tool_result("c_plan", "read", &plan_body);
    plan_msg.read_key = Some("/ws/.harness/plans/plan.md".into());
    let tiny_msg = Msg::tool_result("c_tiny", "bash", "ok (12 bytes)");

    let nodes = vec![
        ("n1", Msg::user("inspect build")),
        (
            "n2",
            Msg::assistant_tool_calls(
                "",
                None,
                vec![
                    tc("c_old1", "bash", r#"{"command":"cargo build"}"#),
                    tc("c_skill", "read", r#"{"path":"skill://rust/SKILL.md"}"#),
                    tc("c_plan", "read", r#"{"path":".harness/plans/plan.md"}"#),
                    tc("c_tiny", "bash", r#"{"command":"true"}"#),
                ],
            ),
        ),
        ("n3", Msg::tool_result("c_old1", "bash", &big)),
        ("n4", skill_msg),
        ("n5", plan_msg),
        ("n6", tiny_msg),
        ("n7", Msg::assistant("checked first batch")),
        ("n8", Msg::user("run tests")),
        (
            "n9",
            Msg::assistant_tool_calls(
                "",
                None,
                vec![tc("c_old2", "bash", r#"{"command":"test"}"#)],
            ),
        ),
        ("n10", Msg::tool_result("c_old2", "bash", &big)),
        ("n11", Msg::assistant("checked tests")),
        ("n12", Msg::user("check latest log")),
        (
            "n13",
            Msg::assistant_tool_calls(
                "",
                None,
                vec![tc("c_recent", "bash", r#"{"command":"tail"}"#)],
            ),
        ),
        ("n14", Msg::tool_result("c_recent", "bash", &recent)),
        ("n15", Msg::assistant("latest log ready")),
    ];
    let mut prev: Option<&str> = None;
    for (id, msg) in nodes {
        append_msg(store, sid, id, prev, msg);
        prev = Some(id);
    }
    (skill_body, plan_body, recent)
}

#[test]
fn percent_threshold_triggers_summary() {
    let llm = CompactionTestLlm::new(
        vec![
            Ok(ok_resp(
                "Concise summary of turns 0 and 1.",
                FinishReason::Stop,
            )),
            Ok(ok_resp("continued cleanly", FinishReason::Stop)),
        ],
        20_000,
    );
    let store = Arc::new(MemStore::default());
    let cfg = CompactionConfig {
        threshold_percent: 50,
        reserve_tokens: Some(2_000),
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
    );
    let meta = rt
        .create_session(
            "pct",
            Path::new("/ws"),
            Some("m".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    seed_turns(&store, &meta.id, 4, 6_000);

    let events = Arc::new(VecEvents::default());
    let result = block_on(rt.run_turn(
        &meta.id,
        "keep going",
        &SessionControl::default(),
        make_io(&events),
    ))
    .expect("turn ok");
    assert_eq!(result.final_text, "continued cleanly");
    let reqs = llm.requests.lock().expect("reqs");
    assert_eq!(reqs.len(), 2);
    assert!(reqs[1].messages.iter().any(|m| {
        m.content
            .contains("[Conversation summary from earlier turns]")
    }));
    assert!(events.0.lock().expect("events").iter().any(|e| matches!(
        e,
        AgentEvent::Compacted { method, tokens_before, tokens_after, .. }
            if method == "summary" && tokens_after < tokens_before
    )));
}

#[test]
fn reserve_floor_rule() {
    let default_cfg = CompactionConfig::default();
    assert_eq!(default_cfg.reserve_floor(64_000), 16_384);
    assert_eq!(default_cfg.trigger_tokens(64_000, None), 64_000 - 16_384);
    assert_eq!(default_cfg.reserve_floor(100_000), 16_384);
    assert_eq!(default_cfg.trigger_tokens(100_000, None), 100_000 - 16_384);
    assert_eq!(default_cfg.reserve_floor(200_000), 30_000);
    assert_eq!(default_cfg.trigger_tokens(200_000, None), 170_000);
    assert_eq!(default_cfg.reserve_floor(1_000_000), 150_000);
    assert_eq!(default_cfg.trigger_tokens(1_000_000, None), 850_000);
    assert_eq!(
        default_cfg.effective_keep_recent_tokens(40_000, None),
        2_508
    );

    let explicit = CompactionConfig {
        reserve_tokens: Some(10_000),
        threshold_percent: 50,
        threshold_tokens: 195_000,
        ..CompactionConfig::default()
    };
    assert_eq!(explicit.reserve_floor(200_000), 10_000);
    assert_eq!(explicit.trigger_tokens(200_000, None), 190_000);

    let tok_wins = CompactionConfig {
        threshold_percent: 50,
        threshold_tokens: 80_000,
        ..CompactionConfig::default()
    };
    assert_eq!(tok_wins.trigger_tokens(200_000, None), 80_000);
}

#[test]
fn method_order_shake_then_summary_counts_calls() {
    let llm = CompactionTestLlm::new(
        vec![Ok(ok_resp("answered after shake", FinishReason::Stop))],
        200_000,
    );
    let store = Arc::new(MemStore::default());
    let local_scheme = Arc::new(MemoryLocalScheme::default());
    let urls = Arc::new(UrlRouter::new().with_builtin(Arc::clone(&local_scheme) as _));
    let cfg = CompactionConfig {
        threshold_tokens: 60_000,
        reserve_tokens: Some(10_000),
        method_order: vec![
            "remote".into(),
            "snapcompact".into(),
            "shake".into(),
            "summary".into(),
        ],
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
    .with_urls(urls);
    let meta = rt
        .create_session(
            "shake",
            Path::new("/ws"),
            Some("m".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    let (skill_body, plan_body, recent) = seed_shake_branch(&store, &meta.id);

    let events = Arc::new(VecEvents::default());
    let res = block_on(rt.run_turn(
        &meta.id,
        "summarize findings",
        &SessionControl::default(),
        make_io(&events),
    ))
    .expect("turn ok");
    assert_eq!(res.final_text, "answered after shake");

    let captured = llm.requests.lock().expect("reqs");
    assert_eq!(
        captured.len(),
        1,
        "shake must avoid calling the summarizer LLM"
    );
    let sent = &captured[0].messages;
    assert!(sent.iter().any(|m| {
        m.role == Role::Tool
            && m.content.contains("[Output truncated - ")
            && m.content.contains("local://")
    }));
    assert!(sent.iter().any(|m| m.content == skill_body));
    assert!(sent.iter().any(|m| m.content == plan_body));
    assert!(sent.iter().any(|m| m.content == "ok (12 bytes)"));
    assert!(sent.iter().any(|m| m.content == recent));
    assert!(sent.iter().any(|m| m.content == "inspect build"));
    assert_eq!(local_scheme.spilled.lock().expect("spilled").len(), 2);
    assert!(events.0.lock().expect("events").iter().any(|e| matches!(
        e,
        AgentEvent::Compacted { method, tokens_before, tokens_after, .. }
            if method == "shake" && *tokens_after < 60_000 && tokens_after < tokens_before
    )));
}

#[test]
fn no_progress_guard_blocks_autocontinue() {
    let bloated = "very verbose bloated summary ".repeat(400);
    let llm = CompactionTestLlm::new(vec![Ok(ok_resp(&bloated, FinishReason::Stop))], 200_000);
    let store = Arc::new(MemStore::default());
    let cfg = CompactionConfig {
        auto_continue: true,
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
    );
    let meta = rt
        .create_session(
            "noprog",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    seed_turns(&store, &meta.id, 3, 40);

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

    assert!(outcome.no_progress);
    assert!(outcome.committed.is_none());
    assert!(!outcome.allow_auto_continue);
    assert!(events.0.lock().expect("events").iter().any(|e| matches!(
        e,
        AgentEvent::StatusChanged { message: Some(m), .. } if m.contains("no progress")
    )));
    assert!(
        !store
            .load_tree(&meta.id)
            .expect("tree")
            .iter()
            .any(|n| matches!(n.payload, DagPayload::Summary { .. }))
    );
}
