// The one large `json!` tool schema in `tool.rs` nests deeper than the default limit.
#![recursion_limit = "512"]

//! `debug`: drive one DAP debug session at a time from the model.
//!
//! ```toml
//! [debug]
//! timeout_secs = 30                 # per-request seconds, clamped to 5..=300
//! [debug.adapters.custom-jvm]       # merged over the built-in descriptors
//! command = "kotlin-debug-adapter"
//! args = ["--stdio"]
//! languages = ["java", "kotlin"]
//! fileTypes = [".java", ".kt"]
//! ```
//!
//! The protocol work (framing, adapter descriptors, sessions) lives in `hrn-edge-dap`; this crate
//! is the tool surface, the process-wide session holder that survives between tool calls, and its
//! `proc://` rows.
//!
//! Removable with `cargo xtask module remove debug`. Nothing starts until the first `launch`.

mod actions;
mod format;
mod manager;
mod tool;

pub use manager::DebugManager;
pub use tool::{DebugConfig, DebugTool};

use hrn_kernel::{ProcSource, Registry, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Register the `debug` tool and its `proc://` source over one shared session holder.
///
/// # Errors
///
/// When the tool name is taken or the configured adapters do not parse.
pub fn register(registry: &mut Registry) -> Result {
    let config: DebugConfig = registry.config()?;
    let overrides: BTreeMap<String, hrn_kernel::serde_json::Value> =
        config.adapters.unwrap_or_default().into_iter().collect();
    let catalog = hrn_edge_dap::Catalog::load(&overrides)?;
    let manager = registry
        .shared()
        .get_or_init(|| Ok(DebugManager::new(Arc::new(catalog))))?;
    registry.proc_source(Arc::clone(&manager) as Arc<dyn ProcSource>);
    registry.help_page("debug", tool::HELP);
    let timeout = config.timeout_secs.unwrap_or(tool::TIMEOUT_DEFAULT);
    registry.deferrable_tool(Arc::new(DebugTool::new(Arc::clone(&manager), timeout)))?;
    Ok(())
}
