//! Unit tests for crash-interrupt diagnosis, pairing repair, fatal exit records, and `--restore`.

use crate::control::SessionControl;
use crate::registry::ToolRegistry;
use crate::runtime::{AgentRuntime, HarnessConfig, TurnIo};
use crate::runtime::{
    INTERRUPTED_ASSISTANT_TEXT, INTERRUPTED_TOOL_RESULT, InterruptDiagnosis, SessionLifecycle,
    diagnose_interrupt, record_fatal_exit, session_lifecycle,
};
use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents, block_on};
use hrn_kernel::{
    AgentStatus, BoxFuture, CancelFlag, DagNode, DagPayload, ExitReason, FinishReason, HrnError,
    LlmResponse, Msg, PermissionMode, Result, Role, SessionStore, ThinkingLevel, Tool,
    ToolApproval, ToolCall, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct CountingTool {
    name: &'static str,
    runs: Arc<AtomicUsize>,
}

impl Tool for CountingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "counting test tool".into(),
            parameters: json!({"type": "object", "properties": {"v": {"type": "string"}}}),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }

    fn execute(&self, _ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let runs = Arc::clone(&self.runs);
        Box::pin(async move {
            runs.fetch_add(1, Ordering::SeqCst);
            let v = args.get("v").and_then(Value::as_str).unwrap_or("");
            Ok(ToolOutput::ok(format!("ran:{v}")))
        })
    }
}

fn scripted_llm(responses: Vec<LlmResponse>) -> Arc<ScriptedLlm> {
    Arc::new(ScriptedLlm {
        responses: Mutex::new(VecDeque::from(
            responses.into_iter().map(Ok).collect::<Vec<_>>(),
        )),
        ..ScriptedLlm::default()
    })
}

fn turn_io(cancel: CancelFlag) -> TurnIo {
    TurnIo {
        cancel,
        events: Arc::new(VecEvents::default()),
        approval: None,
        ask: None,
    }
}

fn append_node(
    store: &MemStore,
    session_id: &str,
    id: &str,
    parent_id: Option<&str>,
    payload: DagPayload,
) {
    store
        .append_node(&DagNode {
            id: id.into(),
            session_id: session_id.into(),
            parent_id: parent_id.map(Into::into),
            created_at_ms: 100,
            payload,
        })
        .expect("append");
    let mut meta = store.get_session(session_id).expect("get").expect("some");
    meta.head_node_id = Some(id.into());
    store.update_session(&meta).expect("update");
}

fn make_crashed_tool_session(
    store: &Arc<MemStore>,
    runtime: &AgentRuntime,
    calls: &[ToolCall],
    answered_first: bool,
) -> String {
    let mut meta = runtime
        .create_session(
            "crash test",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    meta.status = AgentStatus::Working;
    store.update_session(&meta).expect("working");
    let sid = meta.id;
    append_node(
        store,
        &sid,
        "n1",
        None,
        DagPayload::Message {
            turn_id: "t1".into(),
            msg: Msg::user("run tools"),
        },
    );
    append_node(
        store,
        &sid,
        "n2",
        Some("n1"),
        DagPayload::Message {
            turn_id: "t1".into(),
            msg: Msg::assistant_tool_calls(String::new(), None, calls.to_vec()),
        },
    );
    if answered_first && let Some(first) = calls.first() {
        append_node(
            store,
            &sid,
            "n3",
            Some("n2"),
            DagPayload::Message {
                turn_id: "t1".into(),
                msg: Msg::tool_result(first.id.clone(), first.name.clone(), "ok-first"),
            },
        );
    }
    sid
}

#[test]
fn restore_replays_cut_off_tool_batch() {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(CountingTool {
        name: "ping",
        runs: Arc::clone(&runs),
    }));
    let llm = scripted_llm(vec![LlmResponse {
        content: "all done after replay".into(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        usage: None,
        model_used: "mock".into(),
        fallback_from: None,
        finish_reason: FinishReason::Stop,
    }]);
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        tools,
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let sid = make_crashed_tool_session(
        &store,
        &runtime,
        &[ToolCall {
            id: "c1".into(),
            name: "ping".into(),
            arguments: r#"{"v":"hello"}"#.into(),
            signature: None,
        }],
        false,
    );

    let result = block_on(runtime.restore_turn(
        &sid,
        &SessionControl::default(),
        turn_io(CancelFlag::new()),
    ))
    .expect("restore")
    .expect("was interrupted");

    assert_eq!(runs.load(Ordering::SeqCst), 1, "cut-off tool batch re-ran");
    assert_eq!(result.final_text, "all done after replay");
    let reqs = llm.requests.lock().expect("reqs");
    assert_eq!(reqs.len(), 1);
    let sent = &reqs[0].messages;
    assert!(
        sent.iter()
            .any(|m| m.role == Role::Tool && m.content == "ran:hello" && !m.is_error),
        "real replayed tool output reached LLM: {sent:?}"
    );
    drop(reqs);
    let meta = store.get_session(&sid).expect("get").expect("meta");
    assert_eq!(meta.status, AgentStatus::Idle);
    assert!(meta.exit_ms.is_some());
    let branch = store
        .load_branch(&sid, meta.head_node_id.as_deref().expect("head"))
        .expect("branch");
    assert_eq!(
        diagnose_interrupt(&meta, &branch),
        InterruptDiagnosis::Clean
    );
    assert_eq!(
        session_lifecycle(&meta, &branch),
        SessionLifecycle::Complete
    );
}

#[test]
fn resume_repairs_without_request() {
    let llm = scripted_llm(Vec::new());
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let sid = make_crashed_tool_session(
        &store,
        &runtime,
        &[ToolCall {
            id: "c1".into(),
            name: "ping".into(),
            arguments: "{}".into(),
            signature: None,
        }],
        false,
    );

    let repaired = runtime.repair_interrupted(&sid).expect("repair");
    assert!(repaired);
    assert!(
        llm.requests.lock().expect("reqs").is_empty(),
        "normal resume repair must never call the LLM"
    );
    let meta = store.get_session(&sid).expect("get").expect("meta");
    assert_eq!(meta.status, AgentStatus::Idle);
    let branch = store
        .load_branch(&sid, meta.head_node_id.as_deref().expect("head"))
        .expect("branch");
    let messages: Vec<&Msg> = branch
        .iter()
        .filter_map(|n| match &n.payload {
            DagPayload::Message { msg, .. } => Some(msg),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[2].role, Role::Tool);
    assert!(messages[2].is_error);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(messages[2].content, INTERRUPTED_TOOL_RESULT);
    assert_eq!(messages[3].role, Role::Assistant);
    assert_eq!(messages[3].content, INTERRUPTED_ASSISTANT_TEXT);
    assert_eq!(
        diagnose_interrupt(&meta, &branch),
        InterruptDiagnosis::Clean
    );
}

#[test]
fn esc_interrupt_is_final() {
    let llm = scripted_llm(Vec::new());
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let meta = runtime
        .create_session(
            "esc test",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    let cancel = CancelFlag::new();
    cancel.cancel();

    let err = block_on(runtime.run_turn(
        &meta.id,
        "do work",
        &SessionControl::default(),
        turn_io(cancel),
    ))
    .expect_err("cancelled");
    assert!(matches!(err, HrnError::Cancelled));

    let updated = store.get_session(&meta.id).expect("get").expect("meta");
    let branch = store
        .load_branch(&meta.id, updated.head_node_id.as_deref().expect("head"))
        .expect("branch");
    assert_eq!(
        diagnose_interrupt(&updated, &branch),
        InterruptDiagnosis::Clean,
        "Esc interrupt is final and must not be diagnosed as interrupted"
    );
    assert_eq!(
        session_lifecycle(&updated, &branch),
        SessionLifecycle::Aborted
    );
    let restored = block_on(runtime.restore_turn(
        &meta.id,
        &SessionControl::default(),
        turn_io(CancelFlag::new()),
    ))
    .expect("restore");
    assert!(restored.is_none());
    assert!(llm.requests.lock().expect("reqs").is_empty());
}

#[test]
fn partial_tool_batch_repairs_missing_call_and_runs_llm_step() {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(CountingTool {
        name: "ping",
        runs: Arc::clone(&runs),
    }));
    let llm = scripted_llm(vec![LlmResponse {
        content: "continued after partial crash".into(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        usage: None,
        model_used: "mock".into(),
        fallback_from: None,
        finish_reason: FinishReason::Stop,
    }]);
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        tools,
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let sid = make_crashed_tool_session(
        &store,
        &runtime,
        &[
            ToolCall {
                id: "c1".into(),
                name: "ping".into(),
                arguments: r#"{"v":"one"}"#.into(),
                signature: None,
            },
            ToolCall {
                id: "c2".into(),
                name: "ping".into(),
                arguments: r#"{"v":"two"}"#.into(),
                signature: None,
            },
        ],
        true,
    );

    let result = block_on(runtime.restore_turn(
        &sid,
        &SessionControl::default(),
        turn_io(CancelFlag::new()),
    ))
    .expect("restore")
    .expect("was interrupted");

    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "partial batch must repair and let the LLM decide, not blindly re-run"
    );
    assert_eq!(result.final_text, "continued after partial crash");
    let reqs = llm.requests.lock().expect("reqs");
    let sent = &reqs[0].messages;
    assert!(sent.iter().any(|m| {
        m.role == Role::Tool
            && m.tool_call_id.as_deref() == Some("c2")
            && m.is_error
            && m.content == INTERRUPTED_TOOL_RESULT
    }));
}

#[test]
fn fatal_exit_does_not_suppress_restore_and_unanswered_user_continues() {
    let llm = scripted_llm(vec![LlmResponse {
        content: "finished pending user turn".into(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        usage: None,
        model_used: "mock".into(),
        fallback_from: None,
        finish_reason: FinishReason::Stop,
    }]);
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let mut meta = runtime
        .create_session(
            "panic test",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    meta.status = AgentStatus::Working;
    store.update_session(&meta).expect("working");
    append_node(
        &store,
        &meta.id,
        "n1",
        None,
        DagPayload::Message {
            turn_id: "t1".into(),
            msg: Msg::user("please answer"),
        },
    );
    assert!(record_fatal_exit(store.as_ref(), &meta.id, 777).expect("fatal"));

    let updated = store.get_session(&meta.id).expect("get").expect("meta");
    let branch = store
        .load_branch(&meta.id, updated.head_node_id.as_deref().expect("head"))
        .expect("branch");
    assert!(matches!(
        branch.last().map(|n| &n.payload),
        Some(DagPayload::SessionExit {
            reason: ExitReason::Fatal,
            ..
        })
    ));
    assert_eq!(
        session_lifecycle(&updated, &branch),
        SessionLifecycle::Interrupted
    );
    assert!(matches!(
        diagnose_interrupt(&updated, &branch),
        InterruptDiagnosis::UnansweredUser { .. }
    ));

    let done = block_on(runtime.restore_turn(
        &meta.id,
        &SessionControl::default(),
        turn_io(CancelFlag::new()),
    ))
    .expect("restore")
    .expect("restored");
    assert_eq!(done.final_text, "finished pending user turn");
    let reqs = llm.requests.lock().expect("reqs");
    let user_count = reqs[0]
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .count();
    assert_eq!(
        user_count, 1,
        "must not duplicate the existing user message"
    );
}
