//! Pure port traits for subagent worktree isolation ([`WorktreeIsolator`]) and eval-defined
//! dynamic tool resolution ([`DynamicToolResolver`]).
//!
//! Keeps `hrn-harness` free of process/git I/O and edge crates; the binary crate wires concrete
//! implementations (e.g. `GitWorktreeIsolator`) via [`crate::AgentRuntime::with_isolator`].

use crate::artifacts::ArtifactStore;
use crate::task_config::TaskIsolationConfig;
use hrn_kernel::{BoxFuture, Result, Tool};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Prepared git worktree backing one isolated subagent run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsolatedWorktree {
    /// Checked-out worktree directory (becomes the child session's `cwd`).
    pub path: PathBuf,
    /// Git branch created for this worktree (e.g. `hrn/task/<id>`).
    pub branch: String,
    /// Original parent repository working directory.
    pub repo_cwd: PathBuf,
}

/// Parameters supplied to [`WorktreeIsolator::finalize`] when an isolated subagent settles.
#[derive(Clone)]
pub struct IsolationFinalize {
    /// Root session identifier owning this agent tree.
    pub root_session_id: String,
    /// Agent identifier (`<id>` or `<parent>.<child>`).
    pub agent_id: String,
    /// Allowed relative path globs/prefixes for diff-scope verification.
    pub allowed_paths: Vec<String>,
    /// Denied relative path globs/prefixes for diff-scope verification.
    pub denied_paths: Vec<String>,
    /// Effective `[task.isolation]` settings (`merge`, `apply`).
    pub config: TaskIsolationConfig,
    /// Whether the child turn completed cleanly (`false` skips auto-apply and branch commit).
    pub child_succeeded: bool,
    /// Optional per-root artifact store where `<id>.patch` should be written.
    pub artifacts: Option<Arc<ArtifactStore>>,
}

/// Outcome returned by [`WorktreeIsolator::finalize`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IsolationOutcome {
    /// Path to `<id>.patch` in the artifact store, when non-empty changes were captured.
    pub patch_path: Option<PathBuf>,
    /// Committed branch name when `merge == "branch"` and changes were committed.
    pub branch: Option<String>,
    /// Whether the captured patch was successfully applied to the parent working tree.
    pub applied: bool,
    /// Human-readable notes (scope violations, apply failures, etc.).
    pub notes: Vec<String>,
}

/// Port managing isolated git worktrees for `task` items with `isolated: true`.
pub trait WorktreeIsolator: Send + Sync {
    /// Prepare a fresh worktree from `HEAD` under `<repo>/.harness/wt/task-<id>` on branch
    /// `hrn/task/<id>`.
    fn prepare(&self, repo_cwd: &Path, agent_id: &str) -> BoxFuture<Result<IsolatedWorktree>>;

    /// Verify diff scope, capture `<id>.patch` (and commit when `merge == "branch"`), and
    /// optionally apply the patch back into the parent working tree when `apply == true`.
    fn finalize(
        &self,
        worktree: &IsolatedWorktree,
        req: IsolationFinalize,
    ) -> BoxFuture<Result<IsolationOutcome>>;

    /// Remove a worktree when the agent is killed or released.
    fn cleanup(&self, worktree: &IsolatedWorktree) -> BoxFuture<Result<()>>;
}

/// Port resolving eval-defined kernel tools requested via `task` item `tools: [...]`.
///
/// Wired by Wave 10c (`eval` bridges) via [`crate::AgentRuntime::with_dynamic_tools`].
pub trait DynamicToolResolver: Send + Sync {
    /// Resolve dynamic tool names for `session_id` into callable [`Tool`] instances.
    fn resolve(&self, session_id: &str, names: &[String]) -> Result<Vec<Arc<dyn Tool>>>;
}
