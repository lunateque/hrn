//! Live smoke tests against a locally installed and authenticated `agy` CLI (`>= 1.5.0`).
//!
//! Run explicitly with:
//! `cargo test --test live_agy_smoke -- --ignored --nocapture`

use hrn_edge_llm::agy_acp::agent_profile::{AGENT_DIR_PREFIX, EphemeralAgentProfile};
use hrn_edge_llm::agy_acp::binary::discover_agy_binary;
use hrn_edge_llm::agy_acp::config::AgyProviderConfig;
use hrn_edge_llm::agy_acp::discover::{DEFAULT_DISCOVER_TIMEOUT, discover_agy_models};
use hrn_edge_llm::agy_acp::login::doctor_agy;
use hrn_edge_llm::agy_acp::runtime::{AgyAcpRuntime, RouteKind, RuntimeOptions};
use hrn_edge_llm::agy_acp::usage::{DEFAULT_USAGE_TIMEOUT, fetch_agy_usage};
use hrn_kernel::{
    BoxFuture, CancelFlag, ChunkSink, FinishReason, LlmChunk, LlmRequest, Msg, ThinkingLevel,
    ToolSpec,
};
use serde_json::json;
use std::error::Error;
use std::sync::Mutex;
use std::time::Duration;

type TestResult<T = ()> = std::result::Result<T, Box<dyn Error>>;

struct CollectSink(Mutex<String>);

impl ChunkSink for CollectSink {
    fn on_chunk(&self, chunk: LlmChunk) -> BoxFuture<()> {
        if let LlmChunk::ContentDelta(delta) = chunk {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_str(&delta);
        }
        Box::pin(async {})
    }
}

async fn pick_fast_model() -> TestResult<String> {
    let bin = discover_agy_binary(None).ok_or("agy binary not found")?;
    let cwd = std::env::current_dir()?;
    let catalog = discover_agy_models(&bin, &cwd, DEFAULT_DISCOVER_TIMEOUT).await?;
    catalog
        .models
        .iter()
        .find(|m| m.value.contains("flash"))
        .or_else(|| catalog.models.first())
        .map(|m| m.value.clone())
        .ok_or_else(|| "no models returned by agy discovery".into())
}

fn assert_no_leaked_agent_dirs() -> TestResult {
    let agents_root = EphemeralAgentProfile::default_agents_root()?;
    let my_prefix = format!("{AGENT_DIR_PREFIX}{}-", std::process::id());
    if let Ok(entries) = std::fs::read_dir(&agents_root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Some(s) = name.to_str() {
                assert!(
                    !s.starts_with(&my_prefix),
                    "ephemeral agent dir leaked after Drop: {s}"
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires locally installed and authenticated agy CLI (>= 1.5.0)"]
async fn live_doctor_usage_and_discover() -> TestResult {
    let cwd = std::env::current_dir()?;
    let report = doctor_agy(None, &cwd, Duration::from_secs(20)).await;
    assert!(report.binary.is_some(), "agy binary not found");
    assert!(
        report.version_supported,
        "agy version unsupported: {report:?}"
    );
    assert!(report.acp_ready, "agy acp initialize failed: {report:?}");
    assert_eq!(
        report.authenticated,
        Some(true),
        "agy not authenticated: {report:?}"
    );

    let bin = discover_agy_binary(None).ok_or("agy binary disappeared")?;
    let usage = fetch_agy_usage(&bin, DEFAULT_USAGE_TIMEOUT).await;
    assert!(usage.problem.is_none(), "usage error: {:?}", usage.problem);
    assert!(
        !usage.windows.is_empty(),
        "expected non-empty usage windows"
    );

    let catalog = discover_agy_models(&bin, &cwd, DEFAULT_DISCOVER_TIMEOUT).await?;
    assert!(
        !catalog.models.is_empty(),
        "expected non-empty model catalog"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires locally installed and authenticated agy CLI (>= 1.5.0)"]
async fn live_stream_chat_and_loopback_mcp_bridge_tool() -> TestResult {
    let cwd = std::env::current_dir()?;
    let model_id = pick_fast_model().await?;
    let runtime = AgyAcpRuntime::new(
        AgyProviderConfig::default(),
        RuntimeOptions {
            cwd: Some(cwd),
            ..RuntimeOptions::default()
        },
    );
    let cancel = CancelFlag::new();
    let sink = CollectSink(Mutex::new(String::new()));
    let magic_tool = ToolSpec {
        name: "get_magic_code".to_owned(),
        description: "Returns the secret HRN verification code for this test.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    };

    let mut messages = vec![Msg::user(
        "Do not call any tools. Reply with the single word PONG.",
    )];
    let req1 = LlmRequest {
        session_id: Some("live-smoke-1".to_owned()),
        messages: messages.clone(),
        tools: vec![magic_tool.clone()],
        thinking: ThinkingLevel::Low,
        ..empty_request()
    };
    let resp1 = runtime
        .stream_chat(&model_id, &req1, &cancel, &sink, None)
        .await?;
    assert_eq!(resp1.finish_reason, FinishReason::Stop);
    assert!(
        resp1.content.to_ascii_uppercase().contains("PONG"),
        "got: {}",
        resp1.content
    );
    messages.push(Msg::assistant(resp1.content));

    messages.push(Msg::user(
        "Now call the `hrn__get_magic_code` tool and reply with the exact code it returns.",
    ));
    let req2 = LlmRequest {
        session_id: Some("live-smoke-1".to_owned()),
        messages: messages.clone(),
        tools: vec![magic_tool.clone()],
        thinking: ThinkingLevel::Low,
        ..empty_request()
    };
    let step = runtime
        .stream_chat(&model_id, &req2, &cancel, &sink, None)
        .await?;
    assert_eq!(
        step.finish_reason,
        FinishReason::ToolCalls,
        "content: {}",
        step.content
    );
    assert!(step.tool_calls.iter().any(|tc| tc.name == "get_magic_code"));

    messages.push(Msg::assistant_tool_calls(
        step.content,
        None,
        step.tool_calls.clone(),
    ));
    for tc in &step.tool_calls {
        messages.push(Msg::tool_result(&tc.id, &tc.name, "HRN-BRIDGE-OK-7742"));
    }
    let req3 = LlmRequest {
        session_id: Some("live-smoke-1".to_owned()),
        messages,
        tools: vec![magic_tool],
        thinking: ThinkingLevel::Low,
        ..empty_request()
    };
    let final_resp = runtime
        .stream_chat(&model_id, &req3, &cancel, &sink, None)
        .await?;
    assert_eq!(runtime.last_route(), Some(RouteKind::ResumeToolCall));
    assert_eq!(final_resp.finish_reason, FinishReason::Stop);
    assert!(
        final_resp.content.contains("HRN-BRIDGE-OK-7742"),
        "got: {}",
        final_resp.content
    );

    drop(runtime);
    assert_no_leaked_agent_dirs()
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
