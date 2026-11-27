//! `out://`: write-only internal URL scheme for emitting user-visible assistant prose mid-turn
//! without ending the turn (`AgentEvent::AssistantNote`).
//!
//! Security & runtime invariants:
//! - Strictly write-only (`read` always errors with ``"`out://` is write-only"``).
//! - Hard 64 KiB byte cap ([`MAX_OUT_BYTES`]).
//! - Strictly refused for subagents (`ctx.is_subagent` or `ctx.session_id != ctx.root_session_id`).
//! - Zero async-runtime dependencies: uses an optional host [`EventEmitter`] port or polls ready
//!   [`EventSink`] futures in-place via `Waker::noop()`.

use hrn_kernel::{AgentEvent, EventSink, HrnError, Result, ToolContext, UrlBody, UrlScheme};
use std::sync::Arc;
use std::task::{Context, Waker};

/// Hard maximum payload size (64 KiB) accepted by `write out://`.
pub const MAX_OUT_BYTES: usize = 65_536;

/// Host port driving an async [`EventSink::emit`] call from synchronous [`UrlScheme::write`].
///
/// Keeps `hrn-harness` free of async-runtime dependencies while still supporting async sinks
/// (e.g. RPC/ACP stdio writers) when wired by the binary.
pub type EventEmitter = Arc<dyn Fn(&Arc<dyn EventSink>, AgentEvent) -> Result + Send + Sync>;

/// `out://` internal URL scheme: emits [`AgentEvent::AssistantNote`] to the caller's event sink.
#[derive(Default)]
pub struct OutScheme {
    emitter: Option<EventEmitter>,
}

impl OutScheme {
    /// Construct an `out://` scheme that polls ready [`EventSink`] futures directly in-place.
    #[must_use]
    pub fn new() -> Self {
        Self { emitter: None }
    }

    /// Construct an `out://` scheme using a host-supplied [`EventEmitter`] bridge.
    #[must_use]
    pub fn with_emitter(emitter: EventEmitter) -> Self {
        Self {
            emitter: Some(emitter),
        }
    }

    fn emit_note(&self, events: &Arc<dyn EventSink>, text: String) -> Result {
        let event = AgentEvent::AssistantNote { text };
        if let Some(emitter) = &self.emitter {
            return emitter(events, event);
        }
        let mut fut = events.emit(event);
        let mut cx = Context::from_waker(Waker::noop());
        let _ = fut.as_mut().poll(&mut cx);
        Ok(())
    }
}

impl UrlScheme for OutScheme {
    fn scheme(&self) -> &'static str {
        "out"
    }

    fn about(&self) -> &'static str {
        "mid-turn assistant prose (write-only, <=64 KiB, main agent only); `write out://<label>` \
         renders markdown directly to the user without ending the turn"
    }

    fn read(&self, _ctx: &ToolContext, _rest: &str) -> Result<UrlBody> {
        Err(HrnError::Tool("`out://` is write-only".to_owned()))
    }

    fn write(&self, ctx: &ToolContext, _rest: &str, content: &str) -> Result {
        if ctx.is_subagent || ctx.session_id != ctx.root_session_id {
            return Err(HrnError::Tool(
                "`out://` is refused for subagents".to_owned(),
            ));
        }
        if content.len() > MAX_OUT_BYTES {
            return Err(HrnError::Tool(format!(
                "`out://` content exceeds {MAX_OUT_BYTES}-byte limit ({} bytes)",
                content.len()
            )));
        }
        self.emit_note(&ctx.events, content.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::SessionControl;
    use crate::registry::ToolRegistry;
    use crate::runtime::{AgentRuntime, HarnessConfig, TurnIo};
    use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents};
    use hrn_kernel::{
        BoxFuture, CancelFlag, FinishReason, LlmResponse, PermissionMode, ThinkingLevel, Tool,
        ToolApproval, ToolCall, ToolEffect, ToolOutput, ToolSpec, UrlRouter,
    };
    use serde_json::{Value, json};
    use std::path::PathBuf;

    struct WriteUrlTool;

    impl Tool for WriteUrlTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "write".to_owned(),
                description: "test writer".to_owned(),
                parameters: json!({"type": "object"}),
            }
        }

        fn effect(&self) -> ToolEffect {
            ToolEffect::Mutating
        }

        fn approval(&self, args: &Value) -> ToolApproval {
            if args
                .get("path")
                .and_then(Value::as_str)
                .and_then(UrlRouter::split)
                .is_some_and(|(s, _)| s.eq_ignore_ascii_case("out"))
            {
                return ToolApproval::read();
            }
            ToolApproval::write()
        }

        fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
            let ctx = ctx.clone();
            Box::pin(async move {
                let raw = args["path"].as_str().unwrap_or("");
                let content = args["content"].as_str().unwrap_or("");
                let (scheme, rest) = UrlRouter::split(raw)
                    .ok_or_else(|| HrnError::Tool(format!("not a URL: {raw}")))?;
                ctx.urls.write(&ctx, scheme, rest, content)?;
                Ok(ToolOutput::ok(format!("Wrote `{raw}`")))
            })
        }
    }

    fn make_ctx(events: Arc<dyn EventSink>, is_subagent: bool) -> ToolContext {
        ToolContext {
            session_id: if is_subagent {
                "child-1".to_owned()
            } else {
                "root-1".to_owned()
            },
            turn_id: "turn-1".to_owned(),
            call_id: "call-1".to_owned(),
            cwd: PathBuf::from("."),
            permission_mode: PermissionMode::Default,
            cancel: CancelFlag::new(),
            events,
            ask: None,
            approval: None,
            urls: Arc::new(UrlRouter::new().with_builtin(Arc::new(OutScheme::new()))),
            root_session_id: "root-1".to_owned(),
            inbox: Arc::default(),
            caller: None,
            todos: Arc::default(),
            yield_data: Arc::default(),
            is_subagent,
        }
    }

    #[test]
    fn out_read_is_write_only() {
        let events = Arc::new(VecEvents::default());
        let ctx = make_ctx(events, false);
        let scheme = OutScheme::new();
        let err = scheme
            .read(&ctx, "progress")
            .expect_err("read must fail")
            .to_string();
        assert!(err.contains("`out://` is write-only"), "{err}");
    }

    #[test]
    fn out_scheme_refused_for_subagents() {
        let events = Arc::new(VecEvents::default());
        let scheme = OutScheme::new();

        // Flagged subagent (`is_subagent == true`).
        let mut sub_ctx = make_ctx(Arc::clone(&events) as Arc<dyn EventSink>, true);
        sub_ctx.session_id = "root-1".to_owned();
        let err = scheme
            .write(&sub_ctx, "status", "hi")
            .expect_err("subagent flag must fail")
            .to_string();
        assert!(err.contains("subagents"), "{err}");

        // Child session id distinct from root even if flag were false.
        let mut child_ctx = make_ctx(Arc::clone(&events) as Arc<dyn EventSink>, false);
        child_ctx.session_id = "child-2".to_owned();
        let err = scheme
            .write(&child_ctx, "status", "hi")
            .expect_err("child session must fail")
            .to_string();
        assert!(err.contains("subagents"), "{err}");

        assert!(events.0.lock().unwrap().is_empty());
    }

    #[test]
    fn out_scheme_enforces_64kib_cap_and_ignores_path() {
        let events = Arc::new(VecEvents::default());
        let ctx = make_ctx(Arc::clone(&events) as Arc<dyn EventSink>, false);
        let scheme = OutScheme::new();

        let exact = "a".repeat(MAX_OUT_BYTES);
        scheme.write(&ctx, "", &exact).expect("64 KiB exact ok");
        let over = "a".repeat(MAX_OUT_BYTES + 1);
        let err = scheme
            .write(&ctx, "any/path", &over)
            .expect_err("over 64 KiB must fail")
            .to_string();
        assert!(err.contains("65536"), "{err}");
        assert_eq!(events.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn out_scheme_renders_and_continues() {
        let llm = Arc::new(ScriptedLlm::default());
        llm.responses.lock().unwrap().push_back(Ok(LlmResponse {
            model_used: "m".to_owned(),
            fallback_from: None,
            content: String::new(),
            reasoning: String::new(),
            thinking_blocks: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "c1".to_owned(),
                name: "write".to_owned(),
                arguments: json!({
                    "path": "out://update",
                    "content": "Checking **progress** mid-turn."
                })
                .to_string(),
                signature: None,
            }],
            usage: None,
            finish_reason: FinishReason::ToolCalls,
        }));
        llm.responses.lock().unwrap().push_back(Ok(LlmResponse {
            model_used: "m".to_owned(),
            fallback_from: None,
            content: "Final summary after continuing.".to_owned(),
            reasoning: String::new(),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            usage: None,
            finish_reason: FinishReason::Stop,
        }));

        let store = Arc::new(MemStore::default());
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(WriteUrlTool));
        let urls = Arc::new(UrlRouter::new().with_builtin(Arc::new(OutScheme::new())));
        let rt = AgentRuntime::new(
            llm,
            store,
            tools,
            Arc::new(MemFs::default()),
            HarnessConfig::default(),
        )
        .with_urls(urls);

        // Even in Plan mode with no approval gate, `write out://` is read-approved and succeeds.
        let session = rt
            .create_session(
                "out test".to_owned(),
                std::path::Path::new("."),
                None,
                ThinkingLevel::Off,
                PermissionMode::Plan,
            )
            .expect("create session");
        let events = Arc::new(VecEvents::default());
        let io = TurnIo {
            cancel: CancelFlag::new(),
            events: Arc::clone(&events) as Arc<dyn EventSink>,
            approval: None,
            ask: None,
        };
        let done = crate::testutil::block_on(rt.run_turn(
            &session.id,
            "run check",
            &SessionControl::default(),
            io,
        ))
        .expect("turn completes");

        assert_eq!(done.steps, 2, "turn must continue after write out://");
        assert_eq!(done.final_text, "Final summary after continuing.");
        let recorded = events.0.lock().unwrap();
        let notes: Vec<&str> = recorded
            .iter()
            .filter_map(|e| match e {
                AgentEvent::AssistantNote { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(notes, vec!["Checking **progress** mid-turn."]);
    }
}
