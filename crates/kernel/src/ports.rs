//! Secondary port traits wired by the binary so `hrn-harness` and modules stay strictly
//! dependency-free (zero disk I/O, zero image crates, zero cross-module links).

use crate::error::Result;
use crate::ir::{ImageAttachment, ToolOutput};
use crate::traits::{BoxFuture, ToolContext};
use serde_json::Value;

/// Disk-backed artifact store for subagent outputs (`<data>/sessions/<root>/artifacts/`).
///
/// Implemented by an edge crate so `hrn-harness` remains free of `std::fs`.
pub trait ArtifactStore: Send + Sync {
    /// Persist `content` under `<root_session_id>/artifacts/<name>` and return its locator.
    fn write_artifact(&self, root_session_id: &str, name: &str, content: &str) -> Result<String>;

    /// Read `<root_session_id>/artifacts/<name>`, returning `Ok(None)` when absent.
    fn read_artifact(&self, root_session_id: &str, name: &str) -> Result<Option<String>>;

    /// List artifact filenames stored for `root_session_id`, sorted.
    fn list_artifacts(&self, root_session_id: &str) -> Result<Vec<String>>;

    /// Remove artifact directories whose root session id is not in `live_session_ids`.
    /// Returns the number of removed directories.
    fn prune_orphans(&self, live_session_ids: &[String]) -> Result<usize>;
}

/// Host bridge exposed to `modules/eval` so Python/JS cells can invoke registered tools, spawn
/// subagents, and run side completions through the owning runtime without cross-layer coupling.
pub trait EvalHost: Send + Sync {
    /// Invoke a registered tool by name through the active approval ladder and hooks.
    fn call_tool(
        &self,
        ctx: &ToolContext,
        tool_name: &str,
        args: Value,
    ) -> BoxFuture<Result<ToolOutput>>;

    /// Spawn a background subagent from an eval cell and return its handle metadata JSON.
    fn spawn_agent(&self, ctx: &ToolContext, request: Value) -> BoxFuture<Result<Value>>;

    /// Wait on one or more agent handle ids with an optional timeout in milliseconds.
    fn wait_agents(
        &self,
        ctx: &ToolContext,
        ids: Vec<String>,
        timeout_ms: Option<u64>,
    ) -> BoxFuture<Result<Value>>;

    /// Run a lightweight text completion / judge call on an optional model role.
    fn complete(
        &self,
        ctx: &ToolContext,
        prompt: String,
        role: Option<String>,
    ) -> BoxFuture<Result<String>>;
}

/// Renders transcript pages into deterministic PNG images for `snapcompact` (11i) without pulling
/// image-encoding crates into `hrn-harness`.
pub trait SnapRenderer: Send + Sync {
    /// Render each formatted transcript page into a PNG [`ImageAttachment`].
    fn render_pages(&self, pages: &[String]) -> Result<Vec<ImageAttachment>>;
}
