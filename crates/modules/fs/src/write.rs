//! `write`: create or overwrite a file, one archive member, one `SQLite` row, or a writable
//! internal URL (`local://`, `conflict://`).

use hrn_edge_tools::args::{blocking, required_str, summary_of};
use hrn_edge_tools::{EditModeHint, FileReadTracker, conflict_write, write_target_sync_with_opts};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput, ToolSpec, UrlRouter,
};
use std::sync::Arc;

/// The `write` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/write.md");

/// Whole-file, archive-member and database-row writer.
pub(crate) struct WriteTool {
    tracker: Arc<FileReadTracker>,
    edit_hint: Arc<EditModeHint>,
}

impl WriteTool {
    pub(crate) fn new(tracker: Arc<FileReadTracker>, edit_hint: Arc<EditModeHint>) -> Self {
        Self { tracker, edit_hint }
    }
}

impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Destination: a file path or a part-of-file target (`a.zip:inner/path`, `db.sqlite:table[:key]`, `local://…`, `conflict://N`)." },
                    "content": { "type": "string", "description": "Complete content: file text, archive-member text, or a JSON5 row object (empty deletes a keyed row)." }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, args: &Value) -> ToolApproval {
        if let Some(raw) = args.get("path").and_then(Value::as_str)
            && let Some((scheme, _)) = UrlRouter::split(raw)
            && scheme.eq_ignore_ascii_case("out")
        {
            return ToolApproval::read();
        }
        ToolApproval::write()
    }

    fn summarize_call(&self, args: &Value) -> String {
        summary_of(args, "path")
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let tracker = Arc::clone(&self.tracker);
        let blocked_globs = self.edit_hint.blocked_globs();
        let ctx = ctx.clone();
        Box::pin(async move {
            let raw = required_str(&args, "path", "write")?.to_owned();
            let content = required_str(&args, "content", "write")?.to_owned();
            blocking("write", move || {
                if let Some((scheme, rest)) = UrlRouter::split(&raw) {
                    if scheme.eq_ignore_ascii_case("conflict") {
                        return conflict_write(&tracker, &ctx.cwd, rest, &content);
                    }
                    ctx.urls
                        .write(&ctx, scheme, rest, &content)
                        .map_err(|err| {
                            if let hrn_kernel::HrnError::Tool(msg) = err {
                                if msg.contains("unknown URL scheme") {
                                    hrn_kernel::HrnError::Tool(msg.replace(
                                        "; web pages go through `web_fetch`",
                                        "; internal URLs cannot be written as local files",
                                    ))
                                } else {
                                    hrn_kernel::HrnError::Tool(msg)
                                }
                            } else {
                                err
                            }
                        })?;
                    return Ok(ToolOutput::ok(format!(
                        "Wrote `{raw}` ({} lines, {} bytes).",
                        content.lines().count(),
                        content.len()
                    )));
                }
                write_target_sync_with_opts(&tracker, &ctx.cwd, &raw, &content, &blocked_globs)
            })
            .await
        })
    }
}
