//! The `debug` tool: argument checking and the DAP requests each action stands for.

use crate::actions::dispatch;
use crate::format;
use crate::manager::DebugManager;
use hrn_edge_rt::tokio;
use hrn_edge_tools::args::{opt_str, required_str};
use hrn_kernel::serde_json::{Map, Value, json};
use hrn_kernel::{
    BoxFuture, HrnError, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Smallest per-request timeout the tool accepts (seconds).
const TIMEOUT_MIN: u64 = 5;
/// Largest per-request timeout the tool accepts (seconds).
const TIMEOUT_MAX: u64 = 300;
/// Default per-request timeout (seconds) when the config says nothing.
pub(crate) const TIMEOUT_DEFAULT: u64 = 30;

/// The `debug` description and its per-action reference page, as data.
pub(crate) const DESCRIPTION: &str = include_str!("../prompts/debug.md");
pub(crate) const HELP: &str = include_str!("../prompts/help-debug.md");

pub(crate) fn bad(msg: impl Into<String>) -> HrnError {
    HrnError::Tool(msg.into())
}

pub(crate) fn number(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

pub(crate) fn absolute(cwd: &Path, raw: &str) -> PathBuf {
    hrn_edge_tools::paths::resolve(cwd, raw)
}

pub(crate) fn duration(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

pub(crate) fn with_warnings(text: &str, warnings: &[String]) -> String {
    let suffix = format::notes(warnings);
    if suffix.is_empty() {
        text.to_owned()
    } else {
        format!("{text}{suffix}")
    }
}

/// The `[debug]` table of the config file.
#[derive(Debug, Default, hrn_kernel::serde::Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
pub struct DebugConfig {
    /// Per-request timeout in seconds, clamped to 5..=300 (default 30).
    pub timeout_secs: Option<u64>,
    /// Adapter ids merged over the built-in descriptors.
    pub adapters: Option<Map<String, Value>>,
}

/// The DAP debugger tool.
pub struct DebugTool {
    manager: Arc<DebugManager>,
    default_secs: u64,
}

impl DebugTool {
    /// A tool over a shared session manager.
    pub fn new(manager: Arc<DebugManager>, default_secs: u64) -> Self {
        Self {
            manager,
            default_secs: default_secs.clamp(TIMEOUT_MIN, TIMEOUT_MAX),
        }
    }
}

impl Tool for DebugTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "debug".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "launch", "attach", "set_breakpoint", "remove_breakpoint",
                            "set_instruction_breakpoint", "remove_instruction_breakpoint",
                            "data_breakpoint_info", "set_data_breakpoint",
                            "remove_data_breakpoint", "continue", "step_over", "step_in",
                            "step_out", "pause", "evaluate", "stack_trace", "threads",
                            "scopes", "variables", "disassemble", "read_memory",
                            "write_memory", "modules", "loaded_sources", "custom_request",
                            "output", "terminate", "sessions"
                        ],
                        "description": "Debug operation to execute."
                    },
                    "session": { "type": "string", "description": "Debug target id (`debug-1`, or `debug-1.1` for a child the adapter started). Default: the target that last stopped, else the running one." },
                    "program": { "type": "string", "description": "Launch target path (`launch`)." },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Program argv (`launch`)." },
                    "adapter": { "type": "string", "description": "Adapter id (`debugpy`, `dlv`, `codelldb`, `gdb`, `lldb-dap`, `js-debug-adapter`, ...). Otherwise chosen from the target." },
                    "cwd": { "type": "string", "description": "Working directory for the program and relative paths (default: session cwd)." },
                    "file": { "type": "string", "description": "Source file for source breakpoints." },
                    "line": { "type": "number", "description": "Source line for source breakpoints." },
                    "function": { "type": "string", "description": "Function breakpoint name (takes the function path instead of file/line)." },
                    "name": { "type": "string", "description": "Expression name to look up (`data_breakpoint_info`)." },
                    "condition": { "type": "string", "description": "Breakpoint condition expression." },
                    "hit_condition": { "type": "string", "description": "Stop only after this many hits." },
                    "expression": { "type": "string", "description": "Expression to evaluate (`evaluate`)." },
                    "context": { "type": "string", "description": "Evaluate context (default `repl`)." },
                    "frame_id": { "type": "number", "description": "Stack frame to work in (`evaluate`, `scopes`, `data_breakpoint_info`). Default: the current stopped frame." },
                    "scope_id": { "type": "number", "description": "Variables reference of a scope (`variables`)." },
                    "variable_ref": { "type": "number", "description": "Variables reference (`variables`); preferred over `scope_id`." },
                    "pid": { "type": "number", "description": "Process id to attach to." },
                    "port": { "type": "number", "description": "Debug server port to attach to." },
                    "host": { "type": "string", "description": "Debug server host for `port` (default 127.0.0.1)." },
                    "levels": { "type": "number", "description": "Max frames (`stack_trace`)." },
                    "memory_reference": { "type": "string", "description": "Memory reference or address (`disassemble`, `read_memory`, `write_memory`)." },
                    "instruction_reference": { "type": "string", "description": "Instruction breakpoint reference." },
                    "instruction_count": { "type": "number", "description": "Instructions to disassemble." },
                    "instruction_offset": { "type": "number", "description": "Instruction offset for `disassemble`." },
                    "count": { "type": "number", "description": "Bytes to read (`read_memory`)." },
                    "data": { "type": "string", "description": "Base64 payload (`write_memory`)." },
                    "data_id": { "type": "string", "description": "Data breakpoint id from `data_breakpoint_info`." },
                    "access_type": { "type": "string", "enum": ["read", "write", "readWrite"], "description": "Access type to watch (`set_data_breakpoint`)." },
                    "command": { "type": "string", "description": "DAP request name (`custom_request`)." },
                    "arguments": { "type": "object", "description": "Request body (`custom_request`)." },
                    "offset": { "type": "number", "description": "Offset for instruction breakpoints, disassembly and memory access." },
                    "resolve_symbols": { "type": "boolean", "description": "Ask the adapter to resolve symbol names (`disassemble`)." },
                    "allow_partial": { "type": "boolean", "description": "Accept a partial write (`write_memory`)." },
                    "start_module": { "type": "number", "description": "First module index (`modules`)." },
                    "module_count": { "type": "number", "description": "Module count (`modules`)." },
                    "json": { "type": "object", "description": "Extra DAP launch/attach arguments, merged over the adapter defaults." },
                    "timeout": { "type": "number", "description": "Per-request timeout in seconds (5-300)." }
                },
                "required": ["action"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, args: &Value) -> ToolApproval {
        match opt_str(args, "action") {
            Some(
                "sessions"
                | "stack_trace"
                | "threads"
                | "scopes"
                | "variables"
                | "disassemble"
                | "read_memory"
                | "modules"
                | "loaded_sources"
                | "output"
                | "data_breakpoint_info",
            ) => ToolApproval::read(),
            _ => ToolApproval::exec(),
        }
    }

    fn summarize_call(&self, args: &Value) -> String {
        let action = opt_str(args, "action").unwrap_or("?");
        if let (Some(file), Some(line)) = (opt_str(args, "file"), number(args, "line")) {
            return format!("{action} {file}:{line}");
        }
        let target = [
            "program",
            "function",
            "expression",
            "command",
            "memory_reference",
            "instruction_reference",
            "data_id",
            "name",
        ]
        .into_iter()
        .find_map(|key| opt_str(args, key))
        .map(str::to_owned)
        .or_else(|| number(args, "pid").map(|p| format!("pid {p}")))
        .or_else(|| number(args, "port").map(|p| format!("port {p}")));
        match target {
            Some(target) => format!("{action} {target}"),
            None => action.to_owned(),
        }
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let manager = Arc::clone(&self.manager);
        let default_secs = self.default_secs;
        let cwd = ctx.cwd.clone();
        let session_id = ctx.session_id.clone();
        let root = ctx.root_session_id.clone();
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            let action = required_str(&args, "action", "debug")?.to_owned();
            let secs = clamp_secs(number(&args, "timeout"), default_secs);
            let run = dispatch(&manager, &cwd, &session_id, &root, &args, &action, secs);
            tokio::select! {
                biased;
                () = cancel.cancelled() => Err(HrnError::Cancelled),
                result = run => result,
            }
        })
    }
}

fn clamp_secs(requested: Option<i64>, default_secs: u64) -> u64 {
    match requested {
        Some(secs) => u64::try_from(secs.max(0))
            .unwrap_or(default_secs)
            .clamp(TIMEOUT_MIN, TIMEOUT_MAX),
        None => default_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_approval_splits_read_and_exec_actions() {
        let catalog = hrn_edge_dap::Catalog::load(&std::collections::BTreeMap::new())
            .expect("builtin dap catalog");
        let tool = DebugTool::new(Arc::new(DebugManager::new(Arc::new(catalog))), 30);
        for action in ["sessions", "stack_trace", "threads", "variables", "output"] {
            assert_eq!(
                tool.approval(&json!({"action": action})).tier,
                hrn_kernel::ToolTier::Read,
                "{action}"
            );
        }
        for action in [
            "launch",
            "attach",
            "evaluate",
            "write_memory",
            "custom_request",
            "terminate",
            "continue",
        ] {
            assert_eq!(
                tool.approval(&json!({"action": action})).tier,
                hrn_kernel::ToolTier::Exec,
                "{action}"
            );
        }
    }
}
