//! Configuration types and validation for the Antigravity ACP (`agy_acp`) provider.

use hrn_kernel::{HrnError, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Default token threshold above which rewind/fork uses Smart Handoff instead of full transcript bootstrap.
pub const DEFAULT_HANDOFF_THRESHOLD_TOKENS: u64 = 12_000;
/// Default role/model selector invoked to generate a Smart Handoff checkpoint.
pub const DEFAULT_HANDOFF_MODEL: &str = "smol";
/// Default overall turn wall-clock timeout in milliseconds (10 minutes).
pub const DEFAULT_TURN_TIMEOUT_MS: u64 = 600_000;
/// Default stream stall timeout while waiting on model output (2 minutes).
pub const DEFAULT_STALL_TIMEOUT_MS: u64 = 120_000;
/// Default stall timeout while a bridged `hrn` tool is executing (5 minutes).
pub const DEFAULT_TOOL_STALL_TIMEOUT_MS: u64 = 300_000;

/// How tool calls are executed when talking to `agy acp`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgyToolMode {
    /// Route 100% of tool calls through `hrn`'s loopback HTTP MCP bridge, permissions, hooks, and UI.
    #[default]
    Hrn,
    /// Let `agy` use its own built-in tools without spawning the ephemeral custom agent bridge.
    Native,
}

/// Provider-specific configuration for `ProviderKind::AgyAcp`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgyProviderConfig {
    /// Explicit path or executable name for the `agy` CLI binary (`None` = auto-discover).
    pub binary: Option<String>,
    /// Tool execution mode (`hrn` loopback MCP bridge by default, or `native`).
    pub tool_mode: AgyToolMode,
    /// Estimated history token threshold above which rewind/fork triggers Smart Handoff.
    pub handoff_threshold_tokens: u64,
    /// Role or preset selector used to build a Smart Handoff summary (default `"smol"`).
    pub handoff_model: String,
    /// Overall turn wall-clock timeout in milliseconds (default `600_000`).
    pub turn_timeout_ms: u64,
    /// Stream idle timeout in milliseconds when no tool is running (default `120_000`).
    pub stall_timeout_ms: u64,
    /// Idle timeout in milliseconds while waiting on an `hrn` tool execution (default `300_000`).
    pub tool_stall_timeout_ms: u64,
    /// Optional additional workspace directories passed via `--add-dir`.
    pub add_dirs: Vec<String>,
}

impl Default for AgyProviderConfig {
    fn default() -> Self {
        Self {
            binary: None,
            tool_mode: AgyToolMode::Hrn,
            handoff_threshold_tokens: DEFAULT_HANDOFF_THRESHOLD_TOKENS,
            handoff_model: DEFAULT_HANDOFF_MODEL.to_owned(),
            turn_timeout_ms: DEFAULT_TURN_TIMEOUT_MS,
            stall_timeout_ms: DEFAULT_STALL_TIMEOUT_MS,
            tool_stall_timeout_ms: DEFAULT_TOOL_STALL_TIMEOUT_MS,
            add_dirs: Vec::new(),
        }
    }
}

impl AgyProviderConfig {
    /// Overall turn wall-clock timeout as a [`Duration`].
    pub const fn turn_timeout(&self) -> Duration {
        Duration::from_millis(self.turn_timeout_ms)
    }

    /// Stream stall timeout as a [`Duration`].
    pub const fn stall_timeout(&self) -> Duration {
        Duration::from_millis(self.stall_timeout_ms)
    }

    /// Active-tool stall timeout as a [`Duration`].
    pub const fn tool_stall_timeout(&self) -> Duration {
        Duration::from_millis(self.tool_stall_timeout_ms)
    }

    /// Validate configuration invariants.
    pub fn validate(&self, provider_name: &str) -> Result {
        if let Some(bin) = self.binary.as_deref()
            && bin.trim().is_empty()
        {
            return Err(HrnError::Config(format!(
                "provider `{provider_name}`: `binary` cannot be empty when specified"
            )));
        }
        if self.turn_timeout_ms < 5_000 {
            return Err(HrnError::Config(format!(
                "provider `{provider_name}`: `turn_timeout_ms` must be at least 5000 ms"
            )));
        }
        if self.stall_timeout_ms < 1_000 {
            return Err(HrnError::Config(format!(
                "provider `{provider_name}`: `stall_timeout_ms` must be at least 1000 ms"
            )));
        }
        if self.tool_stall_timeout_ms < 1_000 {
            return Err(HrnError::Config(format!(
                "provider `{provider_name}`: `tool_stall_timeout_ms` must be at least 1000 ms"
            )));
        }
        if self.handoff_model.trim().is_empty() {
            return Err(HrnError::Config(format!(
                "provider `{provider_name}`: `handoff_model` cannot be empty"
            )));
        }
        Ok(())
    }
}
