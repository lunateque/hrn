//! `glob`: file discovery by glob (omp's schema: `path`, `hidden`, `gitignore`, `limit`).

use hrn_edge_tools::args::{blocking, bounded_usize, opt_str, summary_of};
use hrn_edge_tools::{DEFAULT_LIMIT, GlobRequest, MAX_LIMIT, run_glob};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{BoxFuture, Result, Tool, ToolContext, ToolEffect, ToolOutput, ToolSpec};

/// The `glob` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/glob.md");

/// Path finder over globs and `;`-separated lists, newest first.
pub(crate) struct GlobTool;

impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Glob, file, directory, or `;`-separated list (default `.`)." },
                    "hidden": { "type": "boolean", "description": "Include hidden files (default true)." },
                    "gitignore": { "type": "boolean", "description": "Respect .gitignore (default true); false includes ignored files." },
                    "limit": { "type": "integer", "minimum": 1, "description": format!("Max returned paths (default {DEFAULT_LIMIT}, at most {MAX_LIMIT}).") }
                }
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn summarize_call(&self, args: &Value) -> String {
        opt_str(args, "path").map_or_else(|| summary_of(args, "path"), str::to_owned)
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let cwd = ctx.cwd.clone();
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            let path = opt_str(&args, "path").map(str::to_owned);
            let limit = bounded_usize(&args, "limit", DEFAULT_LIMIT, 1, usize::MAX);
            let hidden = args.get("hidden").and_then(Value::as_bool).unwrap_or(true);
            let gitignore = args
                .get("gitignore")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            blocking("glob", move || {
                let req = GlobRequest {
                    path: path.as_deref(),
                    hidden,
                    gitignore,
                    limit,
                };
                run_glob(&cwd, &req, &cancel)
            })
            .await
        })
    }
}
