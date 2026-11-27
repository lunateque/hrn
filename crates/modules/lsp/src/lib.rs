//! Native, fast, lazy-started Language Server Protocol module (`hrn-mod-lsp`).
//!
//! Provides:
//! - `[lsp]` configuration loaded from `data/defaults.json` plus user/project `lsp.toml` overrides.
//! - [`LspTool`] (`lsp`): semantic `diagnostics`, `definition`, `type_definition`,
//!   `implementation`, `references`, `hover`, `symbols`, `incoming_calls`, `outgoing_calls`,
//!   `code_actions`, `rename` (plan preview), `format`, `status`, and `reload` queries.
//! - [`LspDiagnosticsHook`]: post-write / post-edit hook supporting `diagnostics_on_write`,
//!   `diagnostics_on_edit`, and `format_on_write`.
//!
//! Removable with `cargo xtask module remove lsp`.

mod config;
mod format;
mod hook;
mod lifecycle;
mod pool;
mod queries;
mod rpc;
mod servers;
mod text_edit;
mod tool;
mod uri;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_extra;

pub use config::{LspConfig, LspServerSpec, LspTableConfig, builtin_server_specs};
pub use hook::LspDiagnosticsHook;
pub use pool::LspManager;
pub use rpc::DiagnosticItem;
pub use tool::LspTool;

use hrn_edge_tools::{EditModeHint, FileReadTracker};
use hrn_kernel::{Registry, Result};
use servers::load_merged_servers;
use std::sync::Arc;

/// Register the `lsp` tool and the post-write/edit hook over one shared [`LspManager`].
/// When `[lsp] enabled = false` (or `--no-lsp`), registers neither the tool nor the hook.
///
/// # Errors
///
/// When `[lsp]` is malformed or the tool name is already taken.
pub fn register(registry: &mut Registry) -> Result {
    let table: LspTableConfig = registry.config()?;
    table.validate()?;
    if !table.enabled {
        return Ok(());
    }
    let cwd = registry.host().cwd.clone();
    let data_dir = registry.host().data_dir.clone();
    let loaded = load_merged_servers(Some(&data_dir), &cwd);
    let mut cfg = LspConfig::from_table(&table, loaded.servers, loaded.warnings);
    if let Some(idle) = loaded.idle_timeout_override {
        cfg.idle_timeout = idle;
    }
    let tracker = registry
        .shared()
        .get_or_init(|| Ok(FileReadTracker::new()))?;
    let edit_hint = registry.shared().get_or_init(|| Ok(EditModeHint::new()))?;
    let manager = Arc::new(
        LspManager::with_context(cfg, Some(data_dir), Some(tracker)).with_edit_hint(edit_hint),
    );
    if !table.lazy {
        manager.warm_matching_in_background(cwd.clone());
    }
    registry.tool(Arc::new(LspTool::new(Arc::clone(&manager))))?;
    registry.hook(Arc::new(LspDiagnosticsHook::new(manager, cwd)));
    Ok(())
}
