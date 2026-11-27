//! End-to-end deterministic tests for `AgyAcpRuntime`, `discover_from_spawn_spec`, and
//! `login_from_spawn_spec` using the self-re-exec mock ACP v1 server (zero external `agy` required).

mod common;

use hrn_edge_llm::agy_acp::bridge::McpBridgeOptions;
use hrn_edge_llm::agy_acp::client::AcpSpawnSpec;
use hrn_edge_llm::agy_acp::config::AgyProviderConfig;
use hrn_edge_llm::agy_acp::discover::discover_from_spawn_spec;
use hrn_edge_llm::agy_acp::handoff::HandoffSummarizer;
use hrn_edge_llm::agy_acp::login::{AgyAuthMethod, login_from_spawn_spec};
use hrn_edge_llm::agy_acp::runtime::{AgyAcpRuntime, RouteKind, RuntimeOptions};
use hrn_kernel::{
    BoxFuture, CancelFlag, ChunkSink, FinishReason, HrnError, LlmChunk, LlmRequest, Msg,
    Result as HrnResult, ScopedFuture, ThinkingLevel, ToolSpec,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

type TestResult<T = ()> = std::result::Result<T, Box<dyn Error>>;

/// Hidden self-re-exec entry point invoked by tests when `HRN_MOCK_ACP_MODE` is set.
#[test]
fn mock_acp_entrypoint() {
    if let Ok(mode) = std::env::var("HRN_MOCK_ACP_MODE") {
        common::mock_acp::run_mock_acp_server(&mode);
    }
}

struct VecSink(Mutex<Vec<LlmChunk>>);

impl VecSink {
    fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }
}

impl ChunkSink for VecSink {
    fn on_chunk(&self, chunk: LlmChunk) -> BoxFuture<()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(chunk);
        Box::pin(async {})
    }
}

struct RecordingSummarizer(Mutex<Vec<String>>);

impl HandoffSummarizer for RecordingSummarizer {
    fn summarize<'a>(
        &'a self,
        model: &'a str,
        _messages: &'a [Msg],
        _cancel: &'a CancelFlag,
    ) -> ScopedFuture<'a, HrnResult<String>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(model.to_owned());
        Box::pin(async { Ok("DENSE_SMART_CHECKPOINT".to_owned()) })
    }
}

fn unique_path(label: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("hrn-agy-e2e-{label}-{}-{n}", std::process::id()))
}

fn mock_spawn_spec(mode: &str, log_path: &Path) -> TestResult<AcpSpawnSpec> {
    let exe = std::env::current_exe()?;
    let mut env = BTreeMap::new();
    env.insert("HRN_MOCK_ACP_MODE".to_owned(), mode.to_owned());
    env.insert(
        "HRN_MOCK_LOG".to_owned(),
        log_path.to_string_lossy().into_owned(),
    );
    Ok(AcpSpawnSpec {
        binary: exe,
        args: vec![
            "--exact".to_owned(),
            "mock_acp_entrypoint".to_owned(),
            "--nocapture".to_owned(),
        ],
        cwd: std::env::current_dir()?,
        env,
    })
}

fn build_runtime(
    config: AgyProviderConfig,
    mode: &str,
    log_path: &Path,
    agents_root: PathBuf,
) -> TestResult<AgyAcpRuntime> {
    let spec = mock_spawn_spec(mode, log_path)?;
    Ok(AgyAcpRuntime::new(
        config,
        RuntimeOptions {
            cwd: Some(std::env::current_dir()?),
            agents_root: Some(agents_root),
            bridge: McpBridgeOptions {
                progress_interval: Duration::from_secs(5),
                call_timeout: Duration::from_secs(10),
            },
            tool_batch_window: Some(Duration::from_millis(60)),
            spawn_override: Some(spec),
        },
    ))
}

#[tokio::test]
async fn e2e_parallel_tool_roundtrip_and_permission_isolation() -> TestResult {
    let log = unique_path("tool_log");
    let agents_root = unique_path("tool_agents");
    let runtime = build_runtime(
        AgyProviderConfig::default(),
        "tool_roundtrip",
        &log,
        agents_root.clone(),
    )?;
    let cancel = CancelFlag::new();
    let sink = VecSink::new();

    let mut req = LlmRequest {
        session_id: Some("hrn-1".to_owned()),
        messages: vec![Msg::user("read both files")],
        tools: vec![ToolSpec {
            name: "read".to_owned(),
            description: "Read file".to_owned(),
            parameters: json!({ "type": "object" }),
        }],
        thinking: ThinkingLevel::Medium,
        ..empty_request()
    };

    // Step 1: returns 2 coalesced parallel tool calls.
    let resp1 = runtime
        .stream_chat("gemini-3.8-flash", &req, &cancel, &sink, None)
        .await?;
    assert_eq!(resp1.finish_reason, FinishReason::ToolCalls);
    assert_eq!(resp1.tool_calls.len(), 2);

    // Step 2: hrn executes tools and calls stream_chat again with trailing Role::Tool messages.
    req.messages.push(Msg::assistant_tool_calls(
        resp1.content,
        None,
        resp1.tool_calls.clone(),
    ));
    for tc in &resp1.tool_calls {
        let body = if tc.arguments.contains("a.txt") {
            "ALPHA"
        } else {
            "BETA"
        };
        req.messages.push(Msg::tool_result(&tc.id, &tc.name, body));
    }

    let resp2 = runtime
        .stream_chat("gemini-3.8-flash", &req, &cancel, &sink, None)
        .await?;
    assert_eq!(runtime.last_route().await, Some(RouteKind::ResumeToolCall));
    assert_eq!(resp2.finish_reason, FinishReason::Stop);
    assert!(resp2.content.contains("ALPHA"));
    assert!(resp2.content.contains("BETA"));

    let transcript = std::fs::read_to_string(&log)?;
    assert!(
        transcript.contains("PERM_NATIVE:reject_once"),
        "native tool permission MUST be denied in Hrn mode: {transcript}"
    );
    assert!(
        transcript.contains("PERM_BRIDGE:allow_once"),
        "hrn-bridge tool permission MUST be allowed: {transcript}"
    );

    drop(runtime);
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_dir_all(&agents_root);
    Ok(())
}

async fn run_text_turn(
    runtime: &AgyAcpRuntime,
    model: &str,
    sid: &str,
    messages: Vec<Msg>,
    thinking: ThinkingLevel,
    summarizer: Option<&dyn HandoffSummarizer>,
) -> TestResult<String> {
    let sink = VecSink::new();
    let resp = runtime
        .stream_chat(
            model,
            &LlmRequest {
                session_id: Some(sid.to_owned()),
                messages,
                thinking,
                ..empty_request()
            },
            &CancelFlag::new(),
            &sink,
            summarizer,
        )
        .await?;
    Ok(resp.content)
}

#[tokio::test]
async fn e2e_linear_branch_resume_rewind_direct_and_smart_handoff() -> TestResult {
    let log = unique_path("dag_log");
    let agents_root = unique_path("dag_agents");
    let cfg = AgyProviderConfig {
        handoff_threshold_tokens: 120,
        handoff_model: "smol".to_owned(),
        ..AgyProviderConfig::default()
    };
    let runtime = build_runtime(cfg, "normal", &log, agents_root.clone())?;
    let summarizer = RecordingSummarizer(Mutex::new(Vec::new()));

    // Turn 1 (Branch A start) -> BootstrapDirect (Fresh).
    let mut branch_a = vec![Msg::user("hello A")];
    let r1 = run_text_turn(
        &runtime,
        "gemini-3.8-flash",
        "dag-1",
        branch_a.clone(),
        ThinkingLevel::Medium,
        Some(&summarizer),
    )
    .await?;
    assert_eq!(runtime.last_route().await, Some(RouteKind::BootstrapDirect));
    branch_a.push(Msg::assistant(r1));

    // Turn 2 (Linear continuation on Branch A + model/effort change).
    branch_a.push(Msg::user("delta A2"));
    let r2 = run_text_turn(
        &runtime,
        "claude-opus-4-6-thinking",
        "dag-1",
        branch_a.clone(),
        ThinkingLevel::High,
        Some(&summarizer),
    )
    .await?;
    assert_eq!(runtime.last_route().await, Some(RouteKind::Linear));
    assert!(r2.contains("delta A2") && !r2.contains("hello A"));
    branch_a.push(Msg::assistant(r2));

    // Turn 3 (Rewind to Branch B below threshold -> BootstrapDirect with <restored_branch_context>).
    let branch_b = vec![
        Msg::user("hello A"),
        Msg::assistant("alt reply"),
        Msg::user("branch B prompt"),
    ];
    let r3 = run_text_turn(
        &runtime,
        "gemini-3.8-flash",
        "dag-1",
        branch_b,
        ThinkingLevel::Low,
        Some(&summarizer),
    )
    .await?;
    assert_eq!(runtime.last_route().await, Some(RouteKind::BootstrapDirect));
    assert!(r3.contains("<restored_branch_context>"));

    // Turn 4 (Return to Branch A -> ResumeBranch via session/resume!).
    branch_a.push(Msg::user("back on A"));
    let r4 = run_text_turn(
        &runtime,
        "claude-opus-4-6-thinking",
        "dag-1",
        branch_a,
        ThinkingLevel::High,
        Some(&summarizer),
    )
    .await?;
    assert_eq!(runtime.last_route().await, Some(RouteKind::ResumeBranch));
    assert!(r4.contains("back on A"));

    // Turn 5 (Rewind with large history > 120 tokens -> SmartHandoff using `smol`).
    let large_branch = vec![
        Msg::user("x".repeat(800)),
        Msg::assistant("y".repeat(800)),
        Msg::user("resume huge branch"),
    ];
    let r5 = run_text_turn(
        &runtime,
        "gemini-3.8-flash",
        "dag-1",
        large_branch,
        ThinkingLevel::Medium,
        Some(&summarizer),
    )
    .await?;
    assert_eq!(runtime.last_route().await, Some(RouteKind::SmartHandoff));
    assert!(r5.contains("DENSE_SMART_CHECKPOINT"));
    let used_models = summarizer
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(used_models, vec!["smol".to_owned()]);

    let transcript = std::fs::read_to_string(&log)?;
    assert!(transcript.contains("RESUME:mock-sid-1"));
    assert!(transcript.contains("CONFIG:model=claude-opus-4-6-thinking"));
    assert!(transcript.contains("CONFIG:thought_level=high"));

    drop(runtime);
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_dir_all(&agents_root);
    Ok(())
}

#[tokio::test]
async fn e2e_native_fork_capability_gate() -> TestResult {
    let log = unique_path("fork_log");
    let agents_root = unique_path("fork_agents");
    let runtime = build_runtime(
        AgyProviderConfig::default(),
        "fork_capable",
        &log,
        agents_root.clone(),
    )?;
    let cancel = CancelFlag::new();
    let sink = VecSink::new();

    let _ = runtime
        .stream_chat(
            "gemini-3.8-flash",
            &LlmRequest {
                session_id: Some("fk-1".to_owned()),
                messages: vec![Msg::user("initial")],
                ..empty_request()
            },
            &cancel,
            &sink,
            None,
        )
        .await?;

    let _ = runtime
        .stream_chat(
            "gemini-3.8-flash",
            &LlmRequest {
                session_id: Some("fk-1".to_owned()),
                messages: vec![Msg::user("forked prompt")],
                ..empty_request()
            },
            &cancel,
            &sink,
            None,
        )
        .await?;
    assert_eq!(runtime.last_route().await, Some(RouteKind::NativeFork));
    let transcript = std::fs::read_to_string(&log)?;
    assert!(transcript.contains("METHOD:session/fork"));

    drop(runtime);
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_dir_all(&agents_root);
    Ok(())
}

#[tokio::test]
async fn e2e_mid_tool_cancel_and_error_classification() -> TestResult {
    // 1. Mid-tool cancellation fails bridge closed + sends session/cancel.
    let log = unique_path("cancel_log");
    let agents_root = unique_path("cancel_agents");
    let runtime = build_runtime(
        AgyProviderConfig::default(),
        "cancel_mid_tool",
        &log,
        agents_root.clone(),
    )?;
    let cancel = CancelFlag::new();
    let sink = VecSink::new();

    let mut req = LlmRequest {
        session_id: Some("can-1".to_owned()),
        messages: vec![Msg::user("run slow tool")],
        ..empty_request()
    };
    let step1 = runtime
        .stream_chat("gemini-3.8-flash", &req, &cancel, &sink, None)
        .await?;
    assert_eq!(step1.finish_reason, FinishReason::ToolCalls);

    // Diverging turn (or cancelling right before resuming) aborts cleanly.
    req.messages.push(Msg::user("actually cancel that"));
    let cancel2 = CancelFlag::new();
    let trigger = cancel2.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        trigger.cancel();
    });
    let res = runtime
        .stream_chat("gemini-3.8-flash", &req, &cancel2, &sink, None)
        .await;
    assert!(matches!(res, Err(HrnError::Cancelled)));

    tokio::time::sleep(Duration::from_millis(80)).await;
    let transcript = std::fs::read_to_string(&log)?;
    assert!(transcript.contains("BRIDGE_CANCEL:true:Cancelled by user"));
    assert!(transcript.contains("CANCEL_RECEIVED"));
    drop(runtime);
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_dir_all(&agents_root);

    // 2. Auth error (-32000) -> HrnError::Auth.
    let log_auth = unique_path("auth_log");
    let root_auth = unique_path("auth_agents");
    let rt_auth = build_runtime(
        AgyProviderConfig::default(),
        "auth_error",
        &log_auth,
        root_auth.clone(),
    )?;
    let err = rt_auth
        .stream_chat(
            "gemini-3.8-flash",
            &LlmRequest {
                session_id: Some("a1".to_owned()),
                messages: vec![Msg::user("hi")],
                ..empty_request()
            },
            &CancelFlag::new(),
            &sink,
            None,
        )
        .await;
    assert!(matches!(err, Err(HrnError::Auth(_))));
    drop(rt_auth);
    let _ = std::fs::remove_file(&log_auth);
    let _ = std::fs::remove_dir_all(&root_auth);

    // 3. Plain-text Usage Limit Reached -> retryable Provider error with retry_after_secs.
    let log_q = unique_path("quota_log");
    let root_q = unique_path("quota_agents");
    let rt_q = build_runtime(
        AgyProviderConfig::default(),
        "quota_plaintext",
        &log_q,
        root_q.clone(),
    )?;
    let err_q = rt_q
        .stream_chat(
            "gemini-3.8-flash",
            &LlmRequest {
                session_id: Some("q1".to_owned()),
                messages: vec![Msg::user("hi")],
                ..empty_request()
            },
            &CancelFlag::new(),
            &sink,
            None,
        )
        .await;
    match err_q {
        Err(ref e) if e.is_retryable() => {
            assert_eq!(e.retry_after_secs(), Some(120));
        }
        other => return Err(format!("expected retryable quota error, got {other:?}").into()),
    }
    drop(rt_q);
    let _ = std::fs::remove_file(&log_q);
    let _ = std::fs::remove_dir_all(&root_q);
    Ok(())
}

#[tokio::test]
async fn e2e_discover_and_login_helpers() -> TestResult {
    let log = unique_path("disc_log");
    let cwd = std::env::current_dir()?;
    let spec = mock_spawn_spec("normal", &log)?;
    let discovered = discover_from_spawn_spec(spec, &cwd, Duration::from_secs(10)).await?;
    assert_eq!(discovered.agent_version, "1.5.18");
    assert_eq!(discovered.tier_name.as_deref(), Some("Google AI Ultra"));
    assert_eq!(discovered.models.len(), 2);

    let spec_login = mock_spawn_spec("normal", &log)?;
    let msg = login_from_spawn_spec(
        spec_login,
        AgyAuthMethod::OauthPersonal,
        Duration::from_secs(10),
    )
    .await?;
    assert!(msg.contains("oauth-personal"));
    let _ = std::fs::remove_file(&log);
    Ok(())
}

fn empty_request() -> LlmRequest {
    LlmRequest {
        model: None,
        messages: Vec::new(),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        thinking_budgets: None,
        temperature: None,
        max_tokens: None,
        session_id: None,
        service_tier: None,
    }
}
