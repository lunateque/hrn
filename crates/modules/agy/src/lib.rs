//! `agy`: a clean subprocess bridge to the official `agy` CLI binary (`cargo xtask module remove agy`).
//!
//! By workspace architecture gate (`xtask/src/graph`), this crate is hard-fenced away from all
//! HTTP, OAuth, and LLM provider crates: it only spawns the installed `agy` binary via
//! `hrn-edge-proc` (`process-wrap` Win32 Job Objects / Unix `ProcessGroup`). Without the binary
//! nothing is registered.

mod cli;
mod runner;
mod tools;
mod worktree;

pub use runner::{
    AgyConfig, AgyRequest, AgyResponse, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_SECS,
    discover_agy_binary, format_slash_response, parse_slash_command, run_agy, status_report,
};
pub use tools::{AgyJobPool, agy_tools};

use hrn_kernel::{Registry, Result};

/// Register the `hrn agy` command, and the `agy_*` tools when the `agy` binary is installed.
///
/// # Errors
///
/// When the command or a tool name is already taken.
pub fn register(registry: &mut Registry) -> Result {
    registry.command(cli::command())?;
    let cfg = AgyConfig::from_env();
    if discover_agy_binary(cfg.binary.as_deref()).is_none() {
        return Ok(());
    }
    for tool in agy_tools(cfg) {
        registry.tool(tool)?;
    }
    Ok(())
}
