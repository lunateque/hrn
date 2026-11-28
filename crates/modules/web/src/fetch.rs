//! `web_fetch`: the tool surface over the shared [`PageFetcher`] engine (the same one
//! `read https://...` uses).

use hrn_edge_tools::PageFetcher;
use hrn_edge_tools::args::{bounded_usize, required_str, summary_of};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{BoxFuture, Result, Tool, ToolContext, ToolEffect, ToolOutput, ToolSpec};
use std::sync::Arc;

const DEFAULT_CHARS: usize = 24_000;
const MAX_CHARS: usize = 40_000;

/// The `web_fetch` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/web_fetch.md");

/// Public-web page fetcher returning readable text.
pub(crate) struct WebFetchTool {
    fetcher: Arc<PageFetcher>,
}

impl WebFetchTool {
    pub(crate) fn new(fetcher: Arc<PageFetcher>) -> Self {
        Self { fetcher }
    }
}

impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Public http:// or https:// URL." },
                    "max_chars": { "type": "integer", "minimum": 500, "maximum": MAX_CHARS, "description": "Max characters returned (default 24000)." }
                },
                "required": ["url"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn summarize_call(&self, args: &Value) -> String {
        summary_of(args, "url")
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let fetcher = Arc::clone(&self.fetcher);
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            let url = required_str(&args, "url", "web_fetch")?.to_owned();
            let max_chars = bounded_usize(&args, "max_chars", DEFAULT_CHARS, 500, MAX_CHARS);
            let text = cancel.race(fetcher.fetch(&url, false)).await?;
            Ok(ToolOutput::ok(clip(&text, max_chars)))
        })
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!(
            "{}\n\n[truncated at {max_chars} characters]",
            text.get(..cut).unwrap_or_default()
        ),
        None => text.to_owned(),
    }
}
