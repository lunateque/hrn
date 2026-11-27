//! Antigravity CLI (`agy_acp`) subscription provider for `hrn`.
//!
//! 100% compliant with Google Antigravity Terms of Service: never extracts OAuth tokens or calls
//! private endpoints directly. All inference and authentication flow exclusively through the
//! official `agy` CLI over ACP v1 (`agy acp --stdio`) and a loopback Streamable HTTP MCP bridge.

pub mod agent_profile;
pub mod binary;
pub mod bridge;
mod bridge_http;
pub mod classify;
pub mod client;
pub mod config;
pub mod discover;
pub mod handoff;
pub mod login;
pub mod runtime;
mod runtime_turn;
pub mod schema;
pub mod session_map;
pub mod usage;
pub mod wire;

pub use agent_profile::{EphemeralAgentProfile, cleanup_stale_agent_dirs, validate_builtin_tools};
pub use binary::{
    MIN_AGY_VERSION, discover_agy_binary, is_supported_version, merged_no_proxy_env, parse_semver,
    probe_agy_version,
};
pub use bridge::{BridgeToolResult, McpBridge, McpBridgeOptions};
pub use client::{AcpClient, AcpSpawnSpec};
pub use config::{AgyProviderConfig, AgyToolMode};
pub use discover::{
    DEFAULT_DISCOVER_TIMEOUT, DiscoveredAgyModels, build_agy_catalog, default_agy_provider,
    discover_agy_models,
};
pub use handoff::{BootstrapStrategy, HandoffSummarizer, estimate_messages_tokens};
pub use login::{AgyAuthMethod, AgyDoctorReport, doctor_agy, login_agy};
pub use runtime::{AgyAcpRuntime, RouteKind, RuntimeOptions};
pub use session_map::{AgySessionEntry, RouteDecision, SessionMap, hash_messages_prefix};
pub use usage::{DEFAULT_USAGE_TIMEOUT, fetch_agy_usage, parse_agy_usage_json};
pub use wire::{
    AcpCapabilities, AcpModelOption, AcpPromptResult, AcpSessionNewResult, AcpUpdate,
    AuthMethodInfo, InlineQuota, MAX_NDJSON_LINE_BYTES,
};
