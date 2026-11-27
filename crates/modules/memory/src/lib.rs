//! `memory` (`cargo xtask module remove memory`): cross-session long-term memory,
//! after omp's memory suite.
//!
//! - Tools `retain`, `recall`, `reflect`, `memory_edit` and `learn` (deferred: found through
//!   `tool_search`), and the `memory://` URL scheme (`memory://<id>` reads one memory in full).
//! - Storage is one `SQLite` file with an FTS5 index, in two scopes: `project` (keyed by the
//!   repository's primary checkout root, so linked worktrees share it) and `global`. Recall
//!   searches both.
//! - Memories matching a session's first prompt are put into its system prompt (at most eight,
//!   clipped, under 4 KB), frozen for the session.
//! - Nothing touches the disk until it is needed: a session that never retained a memory only
//!   `stat`s the database path once.

pub mod admin;
mod fmt;
mod inject;
mod project;
mod query;
mod redact;
mod scheme;
mod specs;
mod store;
mod tools;

#[cfg(test)]
mod tests;

use hrn_kernel::{DeferredTool, HarnessHook, Registry, Result, UrlScheme};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Name of the deferred-tool group in the system prompt listing.
const GROUP: &str = "memory";

/// Everything the memory module plugs into the runtime.
pub(crate) struct MemoryParts {
    /// The memory tools, to be registered as deferred.
    pub(crate) tools: Vec<DeferredTool>,
    /// The `memory://` scheme.
    pub(crate) scheme: Arc<dyn UrlScheme>,
    /// Injects relevant memories into the system prompt.
    pub(crate) hook: Arc<dyn HarnessHook>,
}

/// The memory module over the database file `db_path` (created on the first `retain`). `cwd`
/// is the project the prompt injection looks up; tools and the scheme use the calling agent's
/// own working directory.
pub(crate) fn memory_parts(db_path: PathBuf, cwd: &Path) -> MemoryParts {
    let memory = Arc::new(store::Memory::new(db_path));
    let tools = tools::all(&memory)
        .into_iter()
        .map(|tool| DeferredTool {
            group: GROUP.to_owned(),
            short_name: tool.spec().name,
            tool,
        })
        .collect();
    MemoryParts {
        tools,
        scheme: Arc::new(scheme::MemoryScheme::new(Arc::clone(&memory))),
        hook: Arc::new(inject::InjectHook::new(memory, cwd)),
    }
}

/// Register the deferred memory tools, the `memory://` scheme and the prompt-injection hook over
/// `<data dir>/memory.db`.
///
/// # Errors
///
/// When a tool or scheme name is already taken.
pub fn register(registry: &mut Registry) -> Result {
    let host = registry.host();
    let parts = memory_parts(host.data_dir.join("memory.db"), &host.cwd);
    registry.deferred(parts.tools)?;
    registry.scheme(parts.scheme)?;
    registry.hook(parts.hook);
    Ok(())
}
