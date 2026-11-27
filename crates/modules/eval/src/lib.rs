//! Persistent Python / JavaScript kernels behind one `eval` tool (`hrn-mod-eval`).
//!
//! Each (session, language) pair owns a long-lived interpreter child paired with an authenticated
//! loopback HTTP bridge (`127.0.0.1:0`), started on the first call and killed with the process
//! tree (Job Object / process group) when dropped. Embedded preludes (`runner/prelude.py`,
//! `runner/prelude.js`) expose `tool.<name>`, `@tool`, `agent()`, `wait()`, `workpool()`,
//! `judge()`, `completion()`, magics (`%pip`, `%load`, `!cmd`, …), and the approval-gated
//! `computer.*` facade when `[desktop] eval_prelude = true`.
//!
//! Removable with `cargo xtask module remove eval`. Nothing runs until the first call.

mod bridge;
mod bridge_http;
mod config;
mod dyn_tools;
mod kernel;
mod lang;
mod magics;
mod pool;
mod spill;
mod tool;

#[cfg(test)]
mod bridge_tests;
#[cfg(test)]
mod magic_tests;
#[cfg(test)]
mod tests;

pub use config::{DEFAULT_TIMEOUT_SECS, EvalConfig, MAX_TIMEOUT_SECS, MIN_TIMEOUT_SECS};
pub use dyn_tools::KernelToolStore;
pub use lang::Lang;
pub use tool::EvalTool;

use hrn_edge_tools::{DynamicToolLookup, EvalDynamicToolsSlot, EvalRuntimeHints};
use hrn_kernel::{Registry, Result};
use std::sync::Arc;

/// Register `[eval]` configuration, `help://eval`, kernel-defined `@tool` slot, and the `eval`
/// tool; runner and prelude scripts are written under `<data dir>/eval` on first use.
///
/// # Errors
///
/// When `[eval]` config is malformed or the tool name is already taken.
pub fn register(registry: &mut Registry) -> Result {
    let config: EvalConfig = registry.config()?;
    let data_dir = registry.host().data_dir.clone();
    let eval_host = registry.eval_host();
    let hints = registry
        .shared()
        .get_or_init(|| Ok(EvalRuntimeHints::default()))?;
    let dyn_tools = Arc::new(KernelToolStore::new());
    let slot = registry
        .shared()
        .get_or_init(|| Ok(EvalDynamicToolsSlot::default()))?;
    if config.tools_enabled {
        let lookup: Arc<dyn DynamicToolLookup> = Arc::clone(&dyn_tools) as _;
        slot.set(lookup);
    } else {
        slot.clear();
    }
    registry.help_page("eval", tool::HELP);
    registry.tool(Arc::new(EvalTool::with_parts(
        data_dir, config, eval_host, hints, dyn_tools,
    )))
}
