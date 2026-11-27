//! End-to-end integration and security tests for `RuntimeEvalHost` and `SharedDynamicToolResolver`
//! (approval ladder enforcement, `ScopeGuardHook`, subagent `tools: [...]` sharing, and agent
//! handle lifecycle without compile-time module feature references).

use super::{RuntimeEvalHost, SharedDynamicToolResolver};
use hrn_edge_store::SqliteStore;
use hrn_edge_tools::{DynamicToolLookup, EvalDynamicToolsSlot, LocalScheme, StdWorkspaceFs};
use hrn_harness::{
    AgentRegistry, AgentRuntime, HarnessConfig, JobStatus, ScopeGuardHook, Spawn, SubagentTool,
    ToolRegistry, UserApprovalPolicy,
};
use hrn_kernel::{
    AgentEvent, ApprovalDecision, ApprovalGate, ApprovalRequest, BoxFuture, CancelFlag, ChunkSink,
    EvalHost, EventSink, FinishReason, LlmProvider, LlmRequest, LlmResponse, ModelInfo,
    PermissionMode, Result, ScopedFuture, ThinkingLevel, Tool, ToolApproval, ToolApprovalPolicy,
    ToolCall, ToolContext, ToolEffect, ToolOutput, ToolSpec, UrlRouter,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

pub(super) struct TokioTestSpawner;
impl Spawn for TokioTestSpawner {
    fn spawn(&self, task: BoxFuture<()>) -> Result {
        drop(tokio::spawn(task));
        Ok(())
    }
}

pub(super) struct ScriptedLlm {
    responses: Mutex<VecDeque<LlmResponse>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl ScriptedLlm {
    pub(super) fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl LlmProvider for ScriptedLlm {
    fn stream_chat<'a>(
        &'a self,
        request: &'a LlmRequest,
        _cancel: &'a CancelFlag,
        _sink: &'a dyn ChunkSink,
    ) -> ScopedFuture<'a, Result<LlmResponse>> {
        self.requests.lock().expect("lock").push(request.clone());
        let next = self.responses.lock().expect("lock").pop_front();
        Box::pin(async move {
            Ok(next.unwrap_or(LlmResponse {
                model_used: "mock/test".to_owned(),
                fallback_from: None,
                content: "default-reply".to_owned(),
                reasoning: String::new(),
                thinking_blocks: Vec::new(),
                tool_calls: Vec::new(),
                usage: None,
                finish_reason: FinishReason::Stop,
            }))
        })
    }

    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            alias: "default".to_owned(),
            id: "test".to_owned(),
            provider: "mock".to_owned(),
            default: true,
        }]
    }
}

pub(super) struct ExecTool(pub(super) &'static str);
impl Tool for ExecTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.to_owned(),
            description: "test exec tool".to_owned(),
            parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::exec()
    }

    fn execute(&self, _ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let name = self.0;
        Box::pin(async move { Ok(ToolOutput::ok(format!("ran:{name}:{args}"))) })
    }
}

struct DenyAllGate;
impl ApprovalGate for DenyAllGate {
    fn await_decision(
        &self,
        _request: ApprovalRequest,
        _cancel: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

pub(super) fn test_ctx(
    runtime: &AgentRuntime,
    session_id: &str,
    cwd: PathBuf,
    mode: PermissionMode,
) -> ToolContext {
    let urls =
        Arc::new(UrlRouter::new().with_builtin(Arc::new(LocalScheme::new(cwd.join("local")))));
    ToolContext {
        session_id: session_id.to_owned(),
        turn_id: "t1".to_owned(),
        call_id: "c1".to_owned(),
        cwd,
        permission_mode: mode,
        cancel: CancelFlag::new(),
        events: Arc::new(NullSink),
        ask: None,
        approval: None,
        urls,
        root_session_id: session_id.to_owned(),
        inbox: Arc::clone(runtime.agents().inbox()),
        caller: None,
        todos: Arc::clone(runtime.todos()),
        yield_data: Arc::default(),
        is_subagent: false,
    }
}

#[tokio::test]
async fn eval_tool_bridge_enforces_approval_ladder() {
    let dir = std::env::temp_dir().join(format!("hrn-eval-sec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ExecTool("danger")));
    tools.register(Arc::new(ExecTool("write")));

    let mut policies = BTreeMap::new();
    policies.insert("danger".to_owned(), ToolApprovalPolicy::Deny);
    let cfg = HarnessConfig {
        approval_policy: UserApprovalPolicy {
            restricted_folders: vec![dir.join("restricted").to_string_lossy().into_owned()],
            tool_policies: policies,
        },
        ..HarnessConfig::default()
    };
    let agents = Arc::new(AgentRegistry::new(Arc::new(TokioTestSpawner)));
    let runtime = AgentRuntime::new(llm, store, tools, Arc::new(StdWorkspaceFs), cfg)
        .with_agents(agents)
        .with_hook(Arc::new(ScopeGuardHook::new(
            dir.clone(),
            vec!["allowed/**".to_owned()],
            vec!["secret/**".to_owned()],
        )));
    let session = runtime
        .create_session("sec", &dir, None, ThinkingLevel::Off, PermissionMode::Yolo)
        .expect("session");

    let host = RuntimeEvalHost::new();
    host.bind(&runtime);

    // 1. User deny policy beats Yolo mode.
    let yolo_ctx = test_ctx(&runtime, &session.id, dir.clone(), PermissionMode::Yolo);
    let denied = host
        .call_tool(&yolo_ctx, "danger", json!({}))
        .await
        .expect("call");
    assert!(
        denied.is_error,
        "denied tool must stay denied: {}",
        denied.content
    );
    assert!(!denied.content.contains("ran:danger"), "{}", denied.content);

    // 2. Interactive ApprovalGate denial blocks exec tool in AlwaysAsk mode.
    let mut ask_ctx = test_ctx(&runtime, &session.id, dir.clone(), PermissionMode::Default);
    ask_ctx.approval = Some(Arc::new(DenyAllGate));
    let refused = host
        .call_tool(&ask_ctx, "write", json!({ "path": "allowed/a.txt" }))
        .await
        .expect("call");
    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused.content.contains("user denied"),
        "{}",
        refused.content
    );

    // 3. Headless AlwaysAsk fails closed (no approval channel).
    let headless_ctx = test_ctx(&runtime, &session.id, dir.clone(), PermissionMode::Default);
    let headless = host
        .call_tool(&headless_ctx, "write", json!({ "path": "allowed/a.txt" }))
        .await
        .expect("call");
    assert!(headless.is_error, "{}", headless.content);
    assert!(
        !headless.content.contains("ran:write"),
        "{}",
        headless.content
    );

    // 4. Restricted folder forces prompt even in Yolo mode -> fails closed headless.
    let restricted_path = dir.join("restricted").join("key.txt");
    let restricted = host
        .call_tool(
            &yolo_ctx,
            "write",
            json!({ "path": restricted_path.to_string_lossy() }),
        )
        .await
        .expect("call");
    assert!(restricted.is_error, "{}", restricted.content);
    assert!(
        !restricted.content.contains("ran:write"),
        "{}",
        restricted.content
    );

    // 5. ScopeGuardHook blocks denied_paths even in Yolo mode.
    let out_of_scope = host
        .call_tool(&yolo_ctx, "write", json!({ "path": "secret/creds.txt" }))
        .await
        .expect("call");
    assert!(out_of_scope.is_error, "{}", out_of_scope.content);
    assert!(
        out_of_scope.content.contains("scope violation"),
        "{}",
        out_of_scope.content
    );

    // 6. Explicitly refused session-control tools stay blocked at EvalHost too.
    let ckpt = host
        .call_tool(&yolo_ctx, "checkpoint", json!({}))
        .await
        .expect("ckpt");
    assert!(ckpt.is_error && ckpt.content.contains("cannot be called from eval"));

    let _ = std::fs::remove_dir_all(&dir);
}

pub(super) struct MultiplyTool;
impl Tool for MultiplyTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "multiply".to_owned(),
            description: "Multiply a and b".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
                "required": ["a", "b"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::exec()
    }

    fn execute(&self, _ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        Box::pin(async move {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok(ToolOutput::ok((a * b).to_string()))
        })
    }
}

pub(super) struct StubLookup;
impl DynamicToolLookup for StubLookup {
    fn resolve(&self, _session_id: &str, names: &[String]) -> Result<Vec<Arc<dyn Tool>>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for name in names {
            if name == "multiply" {
                out.push(Arc::new(MultiplyTool));
            } else {
                return Err(hrn_kernel::HrnError::Tool(format!("unknown tool `{name}`")));
            }
        }
        Ok(out)
    }
}

#[tokio::test]
async fn eval_kernel_tool_shared_to_task_child() {
    let dir = std::env::temp_dir().join(format!("hrn-eval-dyntool-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    // Child calls the dynamic `multiply` tool on step 1, then replies with the result on step 2.
    let llm = Arc::new(ScriptedLlm::new(vec![
        LlmResponse {
            model_used: "mock/test".to_owned(),
            fallback_from: None,
            content: String::new(),
            reasoning: String::new(),
            thinking_blocks: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "tc1".to_owned(),
                name: "multiply".to_owned(),
                arguments: json!({ "a": 6, "b": 7 }).to_string(),
                signature: None,
            }],
            usage: None,
            finish_reason: FinishReason::ToolCalls,
        },
        LlmResponse {
            model_used: "mock/test".to_owned(),
            fallback_from: None,
            content: "product=42".to_owned(),
            reasoning: String::new(),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            usage: None,
            finish_reason: FinishReason::Stop,
        },
    ]));
    let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
    let agents = Arc::new(AgentRegistry::new(Arc::new(TokioTestSpawner)));
    let slot = Arc::new(EvalDynamicToolsSlot::default());
    slot.set(Arc::new(StubLookup));

    let base = AgentRuntime::new(
        Arc::clone(&llm) as _,
        store,
        ToolRegistry::new(),
        Arc::new(StdWorkspaceFs),
        HarnessConfig::default(),
    )
    .with_agents(agents)
    .with_dynamic_tools(Arc::new(SharedDynamicToolResolver::new(slot)));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SubagentTool::new(base.clone())));
    let runtime = base.with_tools(tools);

    let session = runtime
        .create_session("dyn", &dir, None, ThinkingLevel::Off, PermissionMode::Yolo)
        .expect("session");
    let host = RuntimeEvalHost::new();
    host.bind(&runtime);
    let ctx = test_ctx(&runtime, &session.id, dir.clone(), PermissionMode::Yolo);

    let spawned = host
        .spawn_agent(
            &ctx,
            json!({
                "prompt": "Compute 6*7 using multiply",
                "opts": { "agent": "task", "tools": ["multiply"] }
            }),
        )
        .await
        .expect("spawn");
    let id = spawned["id"].as_str().expect("id").to_owned();
    let waited = host
        .wait_agents(&ctx, vec![id.clone()], Some(5_000))
        .await
        .expect("wait");
    assert_eq!(waited[0]["status"], JobStatus::Idle.as_str());
    assert!(
        waited[0]["output"]
            .as_str()
            .unwrap_or("")
            .contains("product=42"),
        "{waited}"
    );
    // Confirm the child LLM request actually received the `multiply` tool schema and tool result.
    {
        let reqs = llm.requests.lock().expect("lock");
        assert!(reqs[0].tools.iter().any(|t| t.name == "multiply"));
        assert!(
            reqs[1]
                .messages
                .iter()
                .any(|m| m.tool_name.as_deref() == Some("multiply") && m.content == "42")
        );
    }
    // Consumed by wait_agents so inbox is empty.
    assert_eq!(ctx.inbox.queued(&session.id), 0);

    // Also verify complete() works with no tools and defaults to judge role.
    let judged = host
        .complete(&ctx, "rate this".to_owned(), None)
        .await
        .expect("complete");
    assert_eq!(judged, "default-reply");
    let reqs = llm.requests.lock().expect("lock");
    let last = reqs.last().expect("last req");
    assert!(last.tools.is_empty());
    assert_eq!(last.model.as_deref(), Some("judge"));
    drop(reqs);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eval_host_full_python_to_task_child_roundtrip_when_eval_enabled() {
    if hrn_edge_proc::resolve_on_path("python").is_none()
        && hrn_edge_proc::resolve_on_path("python3").is_none()
    {
        return;
    }
    let dir = std::env::temp_dir().join(format!("hrn-eval-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    let cfg = crate::promptstats::fixture_config().expect("fixture cfg");
    let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
    let bg = crate::wiring::Background::new();
    let runtime = crate::wiring::build_runtime_with_tools(
        &cfg,
        store,
        &dir,
        &hrn_edge_mcp::McpToolSet::default(),
        &bg,
    )
    .expect("build runtime");
    // When `eval` feature is removed during removability tests, skip gracefully.
    let Some(eval_tool) = runtime.tools().get("eval") else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let session = runtime
        .create_session("e2e", &dir, None, ThinkingLevel::Off, PermissionMode::Yolo)
        .expect("session");
    let ctx = test_ctx(&runtime, &session.id, dir.clone(), PermissionMode::Yolo);
    let run = async {
        let out = eval_tool
            .execute(
                &ctx,
                json!({
                    "language": "py",
                    "code": "@tool\ndef add_two(x: int) -> int:\n    return x + 2\n'registered'"
                }),
            )
            .await
            .expect("eval");
        assert!(!out.is_error, "{}", out.content);
        let resolver = runtime.dynamic_tools().expect("dynamic_tools wired");
        let tools = resolver
            .resolve(&session.id, &["add_two".to_owned()])
            .expect("resolve");
        let res = tools[0]
            .execute(&ctx, json!({ "x": 40 }))
            .await
            .expect("call kernel tool");
        assert_eq!(res.content, "42");
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), run)
        .await
        .expect("python @tool roundtrip timed out");
    let weak_eval = Arc::downgrade(&eval_tool);
    drop(eval_tool);
    drop(ctx);
    drop(runtime);
    drop(bg);
    assert_eq!(
        weak_eval.strong_count(),
        0,
        "EvalTool must drop cleanly with zero Arc cycles"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
