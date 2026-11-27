//! Small ACP value shapes shared by the request loop.

use crate::backend::TurnSummary;
use serde_json::Value;

/// The `stopReason` of a settled run.
pub(crate) fn stop_reason(summary: &TurnSummary) -> &'static str {
    if summary.aborted {
        "cancelled"
    } else if summary.failed {
        "refusal"
    } else {
        "end_turn"
    }
}

/// The text a `session/prompt` carries: text blocks joined, resource links inlined.
pub(crate) fn join_blocks(blocks: &[Value]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_owned());
                }
            }
            Some("resource_link") => {
                if let (Some(uri), Some(name)) = (
                    block.get("uri").and_then(Value::as_str),
                    block.get("name").and_then(Value::as_str),
                ) {
                    parts.push(format!("[{name}]({uri})"));
                }
            }
            Some("resource") => {
                if let Some(text) = block
                    .get("resource")
                    .and_then(|r| r.get("text"))
                    .and_then(Value::as_str)
                {
                    parts.push(text.to_owned());
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}
