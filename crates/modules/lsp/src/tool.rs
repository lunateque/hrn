//! The `lsp` tool exposed to the agent for fast semantic code intelligence.

use crate::pool::LspManager;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, HrnError, PermissionMode, Result, Tool, ToolApproval, ToolContext, ToolEffect,
    ToolOutput, ToolSpec,
};
use std::path::Path;
use std::sync::Arc;

/// The `lsp` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/lsp.md");

/// Semantic LSP tool (`diagnostics`, `definition`, `type_definition`, `implementation`,
/// `references`, `hover`, `symbols`, `incoming_calls`, `outgoing_calls`, `code_actions`,
/// `rename`, `format`, `status`, `reload`).
pub struct LspTool {
    manager: Arc<LspManager>,
}

impl LspTool {
    /// Wrap a shared [`LspManager`].
    pub fn new(manager: Arc<LspManager>) -> Self {
        Self { manager }
    }
}

impl Tool for LspTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "diagnostics",
                            "definition",
                            "type_definition",
                            "implementation",
                            "references",
                            "hover",
                            "symbols",
                            "incoming_calls",
                            "outgoing_calls",
                            "code_actions",
                            "rename",
                            "format",
                            "status",
                            "reload"
                        ],
                        "description": "LSP operation to run."
                    },
                    "path": {
                        "type": "string",
                        "description": "File path (relative to cwd or absolute). Required for all actions except `status`, `reload`, and workspace `symbols`."
                    },
                    "line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based line number (for definition, type_definition, implementation, references, hover, incoming_calls, outgoing_calls, code_actions, rename)."
                    },
                    "col": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based column number (for definition, type_definition, implementation, references, hover, incoming_calls, outgoing_calls, code_actions, rename)."
                    },
                    "end_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional 1-based end line for `code_actions` range (defaults to `line`)."
                    },
                    "end_col": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional 1-based end column for `code_actions` range (defaults to `col`)."
                    },
                    "query": {
                        "type": "string",
                        "description": "Workspace symbol query (used with `action: symbols` when `path` is omitted)."
                    },
                    "new_name": {
                        "type": "string",
                        "description": "New identifier name (required for `action: rename`)."
                    },
                    "include_declaration": {
                        "type": "boolean",
                        "description": "Include declaration in `references` (default true)."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn approval(&self, args: &Value) -> ToolApproval {
        if args.get("action").and_then(Value::as_str) == Some("format") {
            ToolApproval::write()
        } else {
            ToolApproval::read()
        }
    }

    fn summarize_call(&self, args: &Value) -> String {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("?");
        if let Some(path) = args.get("path").and_then(Value::as_str) {
            if let (Some(l), Some(c)) = (
                args.get("line").and_then(Value::as_u64),
                args.get("col").and_then(Value::as_u64),
            ) {
                return format!("{action} {path}:{l}:{c}");
            }
            return format!("{action} {path}");
        }
        action.to_owned()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let mgr = Arc::clone(&self.manager);
        let cwd = ctx.cwd.clone();
        let mode = ctx.permission_mode;
        Box::pin(async move {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .ok_or_else(|| HrnError::Tool("`lsp`: missing `action`".to_owned()))?;
            let text = dispatch_action(&mgr, &cwd, mode, action, &args).await?;
            Ok(ToolOutput::ok(text))
        })
    }
}

async fn dispatch_action(
    mgr: &LspManager,
    cwd: &Path,
    mode: PermissionMode,
    action: &str,
    args: &Value,
) -> Result<String> {
    match action {
        "status" => Ok(mgr.status().await),
        "reload" => Ok(mgr.reload(cwd).await),
        "diagnostics" => {
            mgr.diagnostics(Path::new(req_str(args, "path")?), cwd)
                .await
        }
        "definition" => {
            let (line, col) = req_pos(args)?;
            mgr.definition(Path::new(req_str(args, "path")?), line, col, cwd)
                .await
        }
        "type_definition" => {
            let (line, col) = req_pos(args)?;
            mgr.type_definition(Path::new(req_str(args, "path")?), line, col, cwd)
                .await
        }
        "implementation" => {
            let (line, col) = req_pos(args)?;
            mgr.implementation(Path::new(req_str(args, "path")?), line, col, cwd)
                .await
        }
        "references" => {
            let (line, col) = req_pos(args)?;
            let inc = args
                .get("include_declaration")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            mgr.references(Path::new(req_str(args, "path")?), line, col, inc, cwd)
                .await
        }
        "hover" => {
            let (line, col) = req_pos(args)?;
            mgr.hover(Path::new(req_str(args, "path")?), line, col, cwd)
                .await
        }
        "symbols" => {
            let path = args.get("path").and_then(Value::as_str).map(Path::new);
            let query = args.get("query").and_then(Value::as_str);
            mgr.symbols(path, query, cwd).await
        }
        "incoming_calls" => {
            let (line, col) = req_pos(args)?;
            mgr.incoming_calls(Path::new(req_str(args, "path")?), line, col, cwd)
                .await
        }
        "outgoing_calls" => {
            let (line, col) = req_pos(args)?;
            mgr.outgoing_calls(Path::new(req_str(args, "path")?), line, col, cwd)
                .await
        }
        "code_actions" => {
            let (line, col) = req_pos(args)?;
            let end_line = opt_pos(args, "end_line")?;
            let end_col = opt_pos(args, "end_col")?;
            mgr.code_actions(
                Path::new(req_str(args, "path")?),
                line,
                col,
                end_line,
                end_col,
                cwd,
            )
            .await
        }
        "rename" => {
            let (line, col) = req_pos(args)?;
            let new_name = req_str(args, "new_name")?;
            mgr.rename_plan(Path::new(req_str(args, "path")?), line, col, new_name, cwd)
                .await
        }
        "format" => {
            if mode == PermissionMode::Plan {
                return Err(HrnError::Tool(
                    "`lsp`: `format` is disabled in plan (read-only) mode".to_owned(),
                ));
            }
            mgr.format_document(Path::new(req_str(args, "path")?), cwd)
                .await
        }
        other => Err(HrnError::Tool(format!("`lsp`: unknown action `{other}`"))),
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| HrnError::Tool(format!("`lsp`: missing `{key}`")))
}

fn req_pos(args: &Value) -> Result<(u32, u32)> {
    let line = args
        .get("line")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v >= 1)
        .ok_or_else(|| HrnError::Tool("`lsp`: missing valid 1-based `line`".to_owned()))?;
    let col = args
        .get("col")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v >= 1)
        .ok_or_else(|| HrnError::Tool("`lsp`: missing valid 1-based `col`".to_owned()))?;
    Ok((line, col))
}

fn opt_pos(args: &Value, key: &str) -> Result<Option<u32>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n >= 1)
            .map(Some)
            .ok_or_else(|| HrnError::Tool(format!("`lsp`: invalid 1-based `{key}`"))),
    }
}
