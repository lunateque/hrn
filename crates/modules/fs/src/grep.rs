//! `grep`: regex content search (omp's schema: `pattern`, `path`, `case`, `gitignore`, `skip`).

use hrn_edge_tools::args::{blocking, opt_str, required_str, summary_of};
use hrn_edge_tools::{EditModeHint, GrepRequest, GrepStyle, run_grep};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, HrnError, Result, Tool, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::sync::Arc;

/// The `grep` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/grep.md");

/// Regex content search over files, directories, globs and `;`-separated lists of those.
pub(crate) struct GrepTool {
    context_before: usize,
    context_after: usize,
    edit_hint: Arc<EditModeHint>,
}

impl GrepTool {
    pub(crate) fn new(
        context_before: usize,
        context_after: usize,
        edit_hint: Arc<EditModeHint>,
    ) -> Self {
        Self {
            context_before,
            context_after,
            edit_hint,
        }
    }
}

/// `skip` as a non-negative whole number; `null` and absent mean 0.
fn skip_of(args: &Value) -> Result<usize> {
    match args.get("skip") {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| HrnError::Tool("Skip must be a whole number of files".into())),
        Some(_) => Err(HrnError::Tool("Skip must be a number".into())),
    }
}

impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern." },
                    "path": { "type": "string", "description": "File, directory, glob, or `;`-separated list (default `.`); exclusions start with `!`; `file:N-M` windows a single file." },
                    "case": { "type": "boolean", "description": "Case-sensitive search (default true)." },
                    "gitignore": { "type": "boolean", "description": "Respect .gitignore during directory scans (default true)." },
                    "skip": { "type": ["number", "null"], "description": "Files to skip (the page offset); take it from the previous result's `Use skip=N`." }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn summarize_call(&self, args: &Value) -> String {
        summary_of(args, "pattern")
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let cwd = ctx.cwd.clone();
        let cancel = ctx.cancel.clone();
        let style = GrepStyle {
            hashline: self.edit_hint.hashline_anchors(),
            context_before: self.context_before,
            context_after: self.context_after,
        };
        Box::pin(async move {
            let pattern = required_str(&args, "pattern", "grep")?.to_owned();
            let path = opt_str(&args, "path").map(str::to_owned);
            let skip = skip_of(&args)?;
            let case_sensitive = args.get("case").and_then(Value::as_bool).unwrap_or(true);
            let gitignore = args
                .get("gitignore")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            blocking("grep", move || {
                let req = GrepRequest {
                    pattern: &pattern,
                    path: path.as_deref(),
                    case_sensitive,
                    gitignore,
                    skip,
                };
                run_grep(&cwd, &req, style, &cancel)
            })
            .await
        })
    }
}
