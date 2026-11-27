//! The hidden `think` and `yield` tools (omp's `HIDDEN_TOOLS`: `goal`, `think`, `yield`).
//! They are registered like any tool but their schema only joins the request when the runtime
//! activates them for the session's state.

use hrn_kernel::serde_json::Value;
use hrn_kernel::{
    AgentEvent, BoxFuture, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput,
    ToolSpec,
};

/// The hidden-tool descriptions, as data.
const DESCRIPTION_THINK: &str = include_str!("../prompts/tools/think.md");
const DESCRIPTION_YIELD: &str = include_str!("../prompts/tools/yield.md");

/// Hidden scratchpad tool (omp's `tools/think.ts`): files a thought that is neither shown to
/// the user nor kept in the model's visible context; the TUI renders it as a collapsed memo.
pub struct ThinkTool;

impl Tool for ThinkTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "think".to_owned(),
            description: DESCRIPTION_THINK.to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "thought": {
                        "type": "string",
                        "description": "Your reasoning or thinking text"
                    }
                },
                "required": ["thought"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }

    fn hidden(&self) -> bool {
        true
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let session_id = ctx.session_id.clone();
        let turn_id = ctx.turn_id.clone();
        let events = ctx.events.clone();
        let thought = args
            .get("thought")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(str::to_owned);
        Box::pin(async move {
            let Some(thought) = thought else {
                return Ok(ToolOutput::err(
                    "Invalid arguments: provide a non-empty `thought` string.",
                ));
            };
            // Rendered like reasoning in the transcript (omp sends it as a thinking chunk).
            let _ = events
                .emit(AgentEvent::ThinkingDelta {
                    delta: format!("[think:{session_id}/{turn_id}] {thought}\n"),
                })
                .await;
            Ok(ToolOutput::ok("Thought recorded."))
        })
    }
}

/// Hidden completion tool (omp's `tools/yield.ts`): a subagent's structured final answer. Its
/// `result` becomes the agent's result delivered to the caller; ordinary text remains the
/// fallback when the subagent never yields.
pub struct YieldTool;

impl Tool for YieldTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "yield".to_owned(),
            description: DESCRIPTION_YIELD.to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "result": {
                        "type": "object",
                        "description": "Structured JSON result with fields matching the schema specified in the prompt",
                        "additionalProperties": true
                    },
                    "notice": {
                        "type": "string",
                        "description": "One-line note for the parent agent - caveats, assumptions, or follow-ups (max 300 chars)"
                    }
                },
                "required": ["result"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }

    fn hidden(&self) -> bool {
        true
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let slot = ctx.yield_data.clone();
        let outcome: std::result::Result<String, String> = match args.get("result") {
            Some(result) if result.is_object() => serde_json::to_string_pretty(result)
                .map_err(|e| format!("yield: result must serialize: {e}")),
            Some(Value::String(text)) => Ok(text.clone()),
            Some(other) => Ok(other.to_string()),
            None => Err("Invalid arguments: `result` (an object) is required.".to_owned()),
        };
        Box::pin(async move {
            match outcome {
                Ok(json) => {
                    *slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(json);
                    Ok(ToolOutput::ok(
                        "Result received. End your turn now - no further tool calls.",
                    ))
                }
                Err(message) => Ok(ToolOutput::err(message)),
            }
        })
    }
}
