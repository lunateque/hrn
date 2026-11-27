//! End-to-end turn-loop tests against a scripted LLM and in-memory store.

use crate::control::SessionControl;
use crate::registry::ToolRegistry;
use crate::runtime::{AgentRuntime, HarnessConfig, TurnIo};
use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents, block_on};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, DagPayload, EventSink, FinishReason, HrnError, LlmResponse,
    ModelInfo, PermissionMode, Result, Role, SessionStore, ThinkingLevel, Tool, ToolApproval,
    ToolCall, ToolContext, ToolEffect, ToolOutput, ToolSpec, Usage,
};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingTool {
    name: &'static str,
    ran: Arc<AtomicUsize>,
    cancel_when_run: bool,
}

impl Tool for CountingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.to_owned(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }

    fn execute(&self, ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        if self.cancel_when_run {
            ctx.cancel.cancel();
        }
        let mut out = ToolOutput::ok("tool output");
        out.read_key = Some("/f:1-10".into());
        Box::pin(async move { Ok(out) })
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: "{}".into(),
        signature: None,
    }
}

fn response(content: &str, calls: Vec<ToolCall>, finish: FinishReason) -> LlmResponse {
    LlmResponse {
        model_used: "m".into(),
        fallback_from: None,
        content: content.into(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: calls,
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            ..Usage::default()
        }),
        finish_reason: finish,
    }
}

struct Harness {
    rt: AgentRuntime,
    llm: Arc<ScriptedLlm>,
    store: Arc<MemStore>,
    events: Arc<VecEvents>,
    ran: Arc<AtomicUsize>,
}

fn harness(script: Vec<LlmResponse>, cancel_when_run: bool) -> Harness {
    let llm = Arc::new(ScriptedLlm::default());
    llm.responses
        .lock()
        .expect("lock")
        .extend(script.into_iter().map(Ok));
    let store = Arc::new(MemStore::default());
    let ran = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    for name in ["first", "second"] {
        tools.register(Arc::new(CountingTool {
            name,
            ran: Arc::clone(&ran),
            cancel_when_run: cancel_when_run && name == "first",
        }));
    }
    let rt = AgentRuntime::new(
        llm.clone(),
        store.clone(),
        tools,
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    Harness {
        rt,
        llm,
        store,
        events: Arc::new(VecEvents::default()),
        ran,
    }
}

impl Harness {
    fn run(&self, prompt: &str) -> (String, Result<crate::TurnResult>) {
        let meta = self
            .rt
            .create_session(
                "t",
                Path::new("/ws"),
                None,
                ThinkingLevel::Off,
                PermissionMode::Yolo,
            )
            .expect("session");
        let sink: Arc<dyn EventSink> = self.events.clone();
        let io = TurnIo {
            cancel: CancelFlag::new(),
            events: sink,
            approval: None,
            ask: None,
        };
        let result = block_on(
            self.rt
                .run_turn(&meta.id, prompt, &SessionControl::default(), io),
        );
        (meta.id, result)
    }

    fn roles(&self, session: &str) -> Vec<(Role, bool)> {
        let tree = self.store.load_tree(session).expect("tree");
        tree.iter()
            .filter_map(|n| match &n.payload {
                DagPayload::Message { msg, .. } => Some((msg.role, msg.is_error)),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn tool_loop_persists_paired_messages_and_accumulates_usage() {
    let h = harness(
        vec![
            response(
                "",
                vec![call("c1", "first"), call("c2", "second")],
                FinishReason::ToolCalls,
            ),
            response("all done", vec![], FinishReason::Stop),
        ],
        false,
    );
    let (session, result) = h.run("go");
    let done = result.expect("turn ok");
    assert_eq!(done.final_text, "all done");
    assert_eq!(done.steps, 2);
    assert_eq!(done.usage.map(|u| u.total_tokens), Some(30));
    // Every call of the loop is on the books, not only the turn's total.
    let stored = h.store.session_usage(&session).expect("stored usage");
    assert_eq!((stored.prompt_tokens, stored.total_tokens), (20, 30));
    assert_eq!(
        h.roles(&session)
            .iter()
            .map(|(r, _)| *r)
            .collect::<Vec<_>>(),
        [
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Tool,
            Role::Assistant
        ]
    );
    let tree = h.store.load_tree(&session).expect("tree");
    let read_key = tree.iter().find_map(|n| match &n.payload {
        DagPayload::Message { msg, .. } if msg.role == Role::Tool => msg.read_key.clone(),
        _ => None,
    });
    assert_eq!(
        read_key.as_deref(),
        Some("/f:1-10"),
        "read_key must survive into the DAG"
    );
    let events = h.events.0.lock().expect("lock");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnDone { steps: 2, .. }))
    );
}

#[test]
fn cancellation_mid_batch_still_answers_every_tool_call() {
    let h = harness(
        vec![response(
            "",
            vec![call("c1", "first"), call("c2", "second")],
            FinishReason::ToolCalls,
        )],
        true,
    );
    let (session, result) = h.run("go");
    assert!(matches!(result, Err(HrnError::Cancelled)));
    assert_eq!(
        h.ran.load(Ordering::SeqCst),
        1,
        "second call must not run after cancel"
    );
    assert_eq!(
        h.roles(&session),
        [
            (Role::User, false),
            (Role::Assistant, false),
            (Role::Tool, false),
            (Role::Tool, true)
        ]
    );
}

#[test]
fn tool_calls_cut_off_by_the_token_limit_are_not_executed() {
    let h = harness(
        vec![
            response("", vec![call("c1", "first")], FinishReason::Length),
            response("retrying smaller", vec![], FinishReason::Stop),
        ],
        false,
    );
    let (session, result) = h.run("go");
    assert_eq!(result.expect("turn ok").final_text, "retrying smaller");
    assert_eq!(h.ran.load(Ordering::SeqCst), 0);
    assert_eq!(h.roles(&session).get(2), Some(&(Role::Tool, true)));
}

#[test]
fn repeated_empty_responses_are_a_provider_error_not_max_steps() {
    let h = harness(
        vec![
            response("  ", vec![], FinishReason::Stop),
            response("", vec![], FinishReason::Stop),
        ],
        false,
    );
    let (_, result) = h.run("go");
    assert!(
        matches!(result, Err(HrnError::Provider { .. })),
        "{result:?}"
    );
    assert_eq!(h.llm.requests.lock().expect("lock").len(), 2);
}

#[test]
fn fork_remaps_summary_cover_ids_to_the_copied_nodes() {
    let h = harness(vec![response("hi", vec![], FinishReason::Stop)], false);
    let (session, result) = h.run("go");
    result.expect("turn ok");
    let mut meta = h.store.get_session(&session).expect("get").expect("meta");
    let first = h
        .store
        .load_tree(&session)
        .expect("tree")
        .first()
        .expect("node")
        .id
        .clone();
    h.rt.append_payload(
        &mut meta,
        DagPayload::Summary {
            covers_node_id: first.clone(),
            text: "S".into(),
            method: None,
            preserve: None,
        },
    )
    .expect("append");
    let forked = h.rt.fork_session(&session, None).expect("fork");
    let copied = h.store.load_tree(&forked.id).expect("tree");
    let copied_first = copied.first().expect("node").id.clone();
    assert_ne!(copied_first, first);
    let cover = copied.iter().find_map(|n| match &n.payload {
        DagPayload::Summary { covers_node_id, .. } => Some(covers_node_id.clone()),
        _ => None,
    });
    assert_eq!(cover, Some(copied_first));
}

#[test]
fn a_live_permission_mode_is_applied_and_persisted_by_the_turn() {
    let h = harness(vec![response("ok", vec![], FinishReason::Stop)], false);
    let meta =
        h.rt.create_session(
            "t",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    let control = SessionControl::default();
    control.set_permission_mode(PermissionMode::AutoEdit);
    let sink: Arc<dyn EventSink> = h.events.clone();
    let io = TurnIo {
        cancel: CancelFlag::new(),
        events: sink,
        approval: None,
        ask: None,
    };
    block_on(h.rt.run_turn(&meta.id, "go", &control, io)).expect("turn ok");
    let stored = h.store.get_session(&meta.id).expect("get").expect("meta");
    assert_eq!(stored.permission_mode, PermissionMode::AutoEdit);
}

struct ReadOnlyStub;

impl Tool for ReadOnlyStub {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".to_owned(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
        Box::pin(async { Ok(ToolOutput::ok("ok")) })
    }
}

#[test]
fn plan_mode_exposes_only_read_only_tools_to_the_llm() {
    let llm = Arc::new(ScriptedLlm::default());
    llm.responses.lock().expect("lock").push_back(Ok(response(
        "plan ready",
        vec![],
        FinishReason::Stop,
    )));
    let store = Arc::new(MemStore::default());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ReadOnlyStub));
    tools.register(Arc::new(CountingTool {
        name: "bash",
        ran: Arc::new(AtomicUsize::new(0)),
        cancel_when_run: false,
    }));
    let rt = AgentRuntime::new(
        llm.clone(),
        store,
        tools,
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let meta = rt
        .create_session(
            "plan",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Plan,
        )
        .expect("session");
    let io = TurnIo {
        cancel: CancelFlag::new(),
        events: Arc::new(VecEvents::default()),
        approval: None,
        ask: None,
    };
    block_on(rt.run_turn(&meta.id, "plan it", &SessionControl::default(), io)).expect("turn ok");
    let reqs = llm.requests.lock().expect("lock");
    let sent_names: Vec<&str> = reqs[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(sent_names, vec!["glob"]);
}

fn tool_call(id: &str, name: &str, arguments: &Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
        signature: None,
    }
}

fn tool_names(request: &hrn_kernel::LlmRequest) -> Vec<&str> {
    request.tools.iter().map(|t| t.name.as_str()).collect()
}

#[test]
fn tool_search_activates_a_deferred_tool_and_the_session_keeps_it() {
    let llm = Arc::new(ScriptedLlm::default());
    llm.responses.lock().expect("lock").extend(
        [
            response(
                "",
                vec![tool_call("s", "tool_search", &json!({"query": "alpha"}))],
                FinishReason::ToolCalls,
            ),
            response(
                "",
                vec![tool_call("a", "mcp__srv__alpha", &json!({}))],
                FinishReason::ToolCalls,
            ),
            response("done", vec![], FinishReason::Stop),
            response("again", vec![], FinishReason::Stop),
        ]
        .into_iter()
        .map(Ok),
    );
    let store = Arc::new(MemStore::default());
    let ran = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ReadOnlyStub));
    tools.defer(["alpha", "beta"].map(|short| hrn_kernel::DeferredTool {
        group: "srv".into(),
        short_name: short.into(),
        tool: Arc::new(CountingTool {
            name: if short == "alpha" {
                "mcp__srv__alpha"
            } else {
                "mcp__srv__beta"
            },
            ran: Arc::clone(&ran),
            cancel_when_run: false,
        }),
    }));
    let rt = AgentRuntime::new(
        llm.clone(),
        store.clone(),
        tools,
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let meta = rt
        .create_session(
            "t",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    let turn = |prompt: &str| {
        let io = TurnIo {
            cancel: CancelFlag::new(),
            events: Arc::new(VecEvents::default()),
            approval: None,
            ask: None,
        };
        block_on(rt.run_turn(&meta.id, prompt, &SessionControl::default(), io)).expect("turn ok")
    };
    turn("use alpha");
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the activated tool ran");
    turn("and again");

    let reqs = llm.requests.lock().expect("lock");
    assert_eq!(tool_names(&reqs[0]), ["glob", "tool_search"]);
    assert_eq!(
        tool_names(&reqs[1]),
        ["glob", "tool_search", "mcp__srv__alpha"],
        "only the matched tool joins, after the stable prefix"
    );
    let system = &reqs[0].messages[0].content;
    assert!(system.contains("- srv: `mcp__srv__<name>` — alpha, beta"));
    assert_eq!(
        tool_names(&reqs[3]),
        tool_names(&reqs[1]),
        "a later turn of the same session still has it"
    );

    let activations: Vec<Vec<String>> = store
        .load_tree(&meta.id)
        .expect("tree")
        .into_iter()
        .filter_map(|n| match n.payload {
            DagPayload::ToolsActivated { names } => Some(names),
            _ => None,
        })
        .collect();
    assert_eq!(
        activations,
        [["mcp__srv__alpha"]],
        "recorded once, in the DAG"
    );
}

#[test]
fn usage_is_recorded_under_the_provider_slash_id_name() {
    use crate::runtime::model_name;
    let model = |alias: &str, provider: &str, id: &str, default: bool| ModelInfo {
        alias: alias.into(),
        id: id.into(),
        provider: provider.into(),
        default,
    };
    let models = [
        model("flash", "gateway", "example-flash", true),
        model("pro", "example", "org/model-pro", false),
    ];
    // The provider answers with the bare id; the row carries the omp name.
    assert_eq!(
        model_name(&models, Some("flash"), "example-flash"),
        "gateway/example-flash"
    );
    // A provider that answers with the full name keeps it; so does a model nobody configured.
    assert_eq!(
        model_name(&models, None, "example/org/model-pro"),
        "example/org/model-pro"
    );
    assert_eq!(
        model_name(&models, Some("pro"), "org/model-pro"),
        "example/org/model-pro"
    );
    assert_eq!(model_name(&models, None, "gone-model"), "gone-model");
    // Two providers serve one id: the asked alias wins, else the default.
    let shared = [
        model("a", "first", "x", false),
        model("b", "second", "x", true),
    ];
    assert_eq!(model_name(&shared, Some("a"), "x"), "first/x");
    assert_eq!(model_name(&shared, None, "x"), "second/x");
}
