//! The port a tool uses to run *another* tool of the same session: the `bash` tool commands
//! (`read notes.txt`, `grep --pattern x` typed in the agent's shell) reach the session's tools
//! through it.
//!
//! The harness implements it over the same path that executes a model's tool call, so a nested
//! call is gated identically (permission mode, approval prompt, hooks). A `ToolContext` built
//! without one (a unit test, a detached tool) simply has no tool commands.

use crate::{BoxFuture, CancelFlag, Result, ToolOutput};
use serde_json::Value;
use std::path::PathBuf;

/// Runs the session's tools on behalf of a tool.
pub trait ToolCaller: Send + Sync {
    /// Resolve, gate and execute one call of `name` with `args`, as if typed in the session's
    /// shell at `cwd`. `cancel` is raised when the waiting command goes away.
    fn call(
        &self,
        name: &str,
        args: Value,
        cwd: PathBuf,
        cancel: CancelFlag,
    ) -> BoxFuture<Result<ToolOutput>>;

    /// The names of the tools available to call.
    fn tool_names(&self) -> Vec<String>;

    /// The schema of `name` (argument parsing and the `--help` page of a tool command), `None`
    /// when the session has no such tool.
    fn spec(&self, name: &str) -> Option<crate::ToolSpec>;
}
