//! `web_search`: the tool over the `hrn-edge-tools` backend chain.

use hrn_edge_http::HttpClient;
use hrn_edge_tools::args::{bounded_usize, opt_str, required_str, summary_of};
use hrn_edge_tools::{DEFAULT_RESULTS, MAX_RESULTS, WebSearchConfig, search_web};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{BoxFuture, Result, Tool, ToolContext, ToolEffect, ToolOutput, ToolSpec};

/// The `web_search` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/web_search.md");

/// Web search with ordered failover across the configured backends.
pub(crate) struct WebSearchTool {
    http: HttpClient,
    cfg: WebSearchConfig,
}

impl WebSearchTool {
    pub(crate) const fn new(http: HttpClient, cfg: WebSearchConfig) -> Self {
        Self { http, cfg }
    }
}

impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query." },
                    "recency": { "type": "string", "enum": ["day", "week", "month", "year"], "description": "Only results from the last day/week/month/year (where the provider supports it)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "description": "Max results (default 10)." }
                },
                "required": ["query"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn summarize_call(&self, args: &Value) -> String {
        summary_of(args, "query")
    }

    fn execute(&self, _ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let http = self.http.clone();
        let cfg = self.cfg.clone();
        Box::pin(async move {
            let text = required_str(&args, "query", "web_search")?;
            let limit = bounded_usize(&args, "limit", DEFAULT_RESULTS, 1, MAX_RESULTS);
            let recency = opt_str(&args, "recency");
            search_web(&http, &cfg, text, limit, recency)
                .await
                .map(ToolOutput::ok)
        })
    }
}
