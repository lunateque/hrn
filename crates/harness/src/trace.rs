//! Structured `/trace` span recording for LLM calls and tool executions.

use hrn_kernel::{Clock, LlmResponse, SessionStore, ToolCall, TraceRecord};
use serde_json::json;

/// Store + clock handle threaded into [`crate::batch::BatchEnv`] so each tool call writes a span.
#[derive(Clone, Copy)]
pub(crate) struct TraceSink<'a> {
    pub(crate) store: &'a dyn SessionStore,
    pub(crate) clock: &'a dyn Clock,
}

/// Record one `kind = "llm"` span for a completed LLM call (best-effort; never aborts a turn).
#[expect(
    clippy::too_many_arguments,
    reason = "explicit span fields avoid an extra throwaway wrapper struct"
)]
pub(crate) fn record_llm_span(
    store: &dyn SessionStore,
    session_id: &str,
    turn_id: &str,
    step: usize,
    started_ms: i64,
    finished_ms: i64,
    response: &LlmResponse,
    first_token_ms: Option<u64>,
) {
    let usage = response.usage.unwrap_or_default();
    let rec = TraceRecord {
        session_id: session_id.to_owned(),
        node_id: None,
        kind: "llm".to_owned(),
        started_ms,
        dur_ms: (finished_ms - started_ms).max(0),
        payload: json!({
            "turn_id": turn_id,
            "step": step,
            "model": response.model_used,
            "finish_reason": format!("{:?}", response.finish_reason),
            "first_token_ms": first_token_ms,
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "reasoning_tokens": usage.reasoning_tokens,
            "cache_read_tokens": usage.cache_read_tokens,
            "cache_write_tokens": usage.cache_write_tokens,
            "cost_micros": usage.cost_micros,
        }),
    };
    let _ = store.record_trace(&rec);
}

/// Record one `kind = "tool"` span for a completed tool call (best-effort; never aborts a turn).
#[expect(
    clippy::too_many_arguments,
    reason = "explicit span fields avoid an extra throwaway wrapper struct"
)]
pub(crate) fn record_tool_span(
    store: &dyn SessionStore,
    session_id: &str,
    turn_id: &str,
    call: &ToolCall,
    started_ms: i64,
    finished_ms: i64,
    is_error: bool,
    cancelled: bool,
) {
    let rec = TraceRecord {
        session_id: session_id.to_owned(),
        node_id: None,
        kind: "tool".to_owned(),
        started_ms,
        dur_ms: (finished_ms - started_ms).max(0),
        payload: json!({
            "turn_id": turn_id,
            "call_id": call.id,
            "tool": call.name,
            "is_error": is_error,
            "cancelled": cancelled,
        }),
    };
    let _ = store.record_trace(&rec);
}

#[cfg(test)]
mod tests {
    use crate::control::SessionControl;
    use crate::registry::ToolRegistry;
    use crate::runtime::{AgentRuntime, HarnessConfig, TurnIo};
    use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents, block_on};
    use hrn_kernel::{
        BoxFuture, CancelFlag, Clock, FinishReason, LlmResponse, PermissionMode, Result,
        SessionStore, ThinkingLevel, Tool, ToolApproval, ToolCall, ToolContext, ToolEffect,
        ToolOutput, ToolSpec, Usage,
    };
    use serde_json::{Value, json};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    struct TickClock(AtomicI64);

    impl Clock for TickClock {
        fn now_ms(&self) -> i64 {
            self.0.fetch_add(25, Ordering::SeqCst)
        }
    }

    struct PingTool;

    impl Tool for PingTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "ping".into(),
                description: "echo pong".into(),
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
            Box::pin(async { Ok(ToolOutput::ok("pong")) })
        }
    }

    #[test]
    fn trace_records_llm_and_tool_spans() {
        let store = Arc::new(MemStore::default());
        let llm = Arc::new(ScriptedLlm::default());
        llm.responses.lock().unwrap().push_back(Ok(LlmResponse {
            content: String::new(),
            reasoning: String::new(),
            thinking_blocks: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_ping_1".into(),
                name: "ping".into(),
                arguments: "{}".into(),
                signature: None,
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                prompt_tokens: 120,
                completion_tokens: 30,
                cost_micros: Some(450),
                ..Usage::default()
            }),
            model_used: "mock/fast".into(),
            fallback_from: None,
        }));
        llm.responses.lock().unwrap().push_back(Ok(LlmResponse {
            content: "all done".into(),
            reasoning: String::new(),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: Some(Usage {
                prompt_tokens: 200,
                completion_tokens: 40,
                reasoning_tokens: Some(12),
                cost_micros: Some(800),
                ..Usage::default()
            }),
            model_used: "mock/fast".into(),
            fallback_from: None,
        }));

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(PingTool));

        let runtime = AgentRuntime::new(
            llm,
            store.clone(),
            tools,
            Arc::new(MemFs::default()),
            HarnessConfig::default(),
        )
        .with_clock(Arc::new(TickClock(AtomicI64::new(1000))));

        let meta = runtime
            .create_session(
                "trace test",
                Path::new("/ws"),
                Some("mock/fast".into()),
                ThinkingLevel::Off,
                PermissionMode::Yolo,
            )
            .expect("create session");
        let io = TurnIo {
            cancel: CancelFlag::new(),
            events: Arc::new(VecEvents::default()),
            approval: None,
            ask: None,
        };
        let control = SessionControl::default();
        let res = block_on(runtime.run_turn(&meta.id, "run ping", &control, io)).expect("turn");
        assert_eq!(res.final_text, "all done");

        let traces = store.list_traces(&meta.id).expect("list traces");
        let llm_spans: Vec<_> = traces.iter().filter(|t| t.kind == "llm").collect();
        let tool_spans: Vec<_> = traces.iter().filter(|t| t.kind == "tool").collect();
        assert_eq!(llm_spans.len(), 2, "expected 2 llm spans: {traces:?}");
        assert_eq!(tool_spans.len(), 1, "expected 1 tool span: {traces:?}");

        assert_eq!(llm_spans[0].payload["model"], "mock/fast");
        assert_eq!(llm_spans[0].payload["prompt_tokens"], 120);
        assert_eq!(llm_spans[0].payload["completion_tokens"], 30);
        assert_eq!(llm_spans[0].payload["cost_micros"], 450);
        assert!(llm_spans[0].dur_ms >= 25);

        assert_eq!(llm_spans[1].payload["reasoning_tokens"], 12);
        assert_eq!(llm_spans[1].payload["cost_micros"], 800);

        assert_eq!(tool_spans[0].payload["tool"], "ping");
        assert_eq!(tool_spans[0].payload["call_id"], "call_ping_1");
        assert_eq!(tool_spans[0].payload["is_error"], false);
        assert!(tool_spans[0].dur_ms >= 25);
        assert_eq!(
            llm_spans[0].payload["turn_id"],
            tool_spans[0].payload["turn_id"]
        );
    }

    #[test]
    fn init_prompt_is_present_and_not_injected_into_cold_turn_system_prompt() {
        let prompt = crate::prompt_data::INIT_PROMPT;
        assert!(prompt.contains("AGENTS.md"), "{prompt}");
        assert!(prompt.contains("boilerplate"), "{prompt}");
        let runtime = AgentRuntime::new(
            Arc::new(ScriptedLlm::default()),
            Arc::new(MemStore::default()),
            ToolRegistry::new(),
            Arc::new(MemFs::default()),
            HarnessConfig::default(),
        );
        let cold = runtime.system_prompt_for(Path::new("/ws"), &SessionControl::default());
        assert!(
            !cold.contains("Do NOT include generic LLM boilerplate"),
            "init prompt must never be injected into cold-turn system prompts"
        );
    }
}
