//! The `wait` tool: block until the first of the caller's own background work settles.

use crate::settle::wait_change;
use hrn_kernel::{
    BoxFuture, HrnError, ProcTable, Result, Tool, ToolApproval, ToolContext, ToolEffect,
    ToolOutput, ToolSpec, format_inbox,
};
use serde_json::{Value, json};
use std::sync::Arc;

/// The `wait` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/tools/wait.md");

/// Blocks the calling agent on its own background work (agents and `bash` jobs).
pub struct WaitTool {
    procs: Arc<ProcTable>,
}

impl WaitTool {
    /// A `wait` tool over `procs`.
    pub fn new(procs: Arc<ProcTable>) -> Self {
        Self { procs }
    }
}

impl Tool for WaitTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "wait".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }

    fn summarize_call(&self, _args: &Value) -> String {
        "wait for background work".to_owned()
    }

    fn execute(&self, ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
        let procs = Arc::clone(&self.procs);
        let inbox = Arc::clone(&ctx.inbox);
        let owner = ctx.session_id.clone();
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            loop {
                let seen = inbox.version();
                let mine = procs.owned(&owner);
                let mut settled: Vec<_> =
                    mine.iter().filter(|e| e.waitable && !e.running).collect();
                settled.sort_by_key(|e| e.finished_ms);
                for entry in settled {
                    // Consuming the delivery keeps the result from arriving a second time.
                    if let Some(item) = inbox.take_source(&owner, &entry.id) {
                        let mut text = item.text;
                        let running = mine
                            .iter()
                            .filter(|e| e.waitable && e.running)
                            .map(|e| e.id.as_str())
                            .collect::<Vec<_>>();
                        if !running.is_empty() {
                            text = format!("{text}\n\nStill running: {}", running.join(", "));
                        }
                        return Ok(ToolOutput::ok(text));
                    }
                }
                let messages = inbox.take(&owner);
                if !messages.is_empty() {
                    return Ok(ToolOutput::ok(format!(
                        "Wait interrupted by message.\n\n{}",
                        format_inbox(&messages)
                    )));
                }
                if !mine.iter().any(|e| e.waitable && e.running) {
                    return Ok(ToolOutput::ok(
                        "Nothing to wait for: none of your background work is pending.",
                    ));
                }
                if wait_change(&inbox, seen, &cancel).await {
                    return Err(HrnError::Cancelled);
                }
            }
        })
    }
}
