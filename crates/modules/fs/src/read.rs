//! `read`: files, directories, archives, databases, documents, web pages and internal URLs.

use hrn_edge_tools::args::{blocking, required_str, summary_of};
use hrn_edge_tools::{
    EditModeHint, FileReadTracker, PageFetcher, is_web_url, read_sync_with_anchors, read_url,
};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::sync::Arc;

/// The `read` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/read.md");

/// Reader for files, directories, web pages and internal URLs with trailing line selectors.
pub(crate) struct ReadTool {
    tracker: Arc<FileReadTracker>,
    fetcher: Arc<PageFetcher>,
    edit_hint: Arc<EditModeHint>,
}

impl ReadTool {
    pub(crate) fn new(
        tracker: Arc<FileReadTracker>,
        fetcher: Arc<PageFetcher>,
        edit_hint: Arc<EditModeHint>,
    ) -> Self {
        Self {
            tracker,
            fetcher,
            edit_hint,
        }
    }
}

impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File, directory or internal URL with an optional trailing selector (`:50-80`, `:-20`, `:raw`, `a.zip:inner`, `db.sqlite:table`, `?q=...`)."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }

    fn summarize_call(&self, args: &Value) -> String {
        summary_of(args, "path")
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let tracker = Arc::clone(&self.tracker);
        let fetcher = Arc::clone(&self.fetcher);
        let hashline_anchors = self.edit_hint.hashline_anchors();
        let ctx = ctx.clone();
        Box::pin(async move {
            let raw = required_str(&args, "path", "read")?.to_owned();
            if is_web_url(&raw) {
                return read_url(&fetcher, &ctx.cwd, &raw).await;
            }
            blocking("read", move || {
                read_sync_with_anchors(&tracker, &ctx, &raw, hashline_anchors)
            })
            .await
        })
    }
}
