//! Native lean Rust MCP (Model Context Protocol) client and stdio server.
//!
//! - Client supports both **stdio** (backed by `hrn-edge-proc` Win32 Job Objects / Unix
//!   `ProcessGroup` + `.cmd` resolution) and **Streamable HTTP** (`hrn-edge-http`, supporting both
//!   `application/json` and `text/event-stream` responses + `Mcp-Session-Id`).
//! - Tool adapters expose remote tools to the harness as `mcp__<server>__<tool>` with per-server
//!   `allow_tools` / `deny_tools`, `timeout_secs`, `auto_approve` (default `true`), `read_only`,
//!   auto-reconnect on broken stdio pipe, and graceful degradation when a server is offline.
//! - Tool lists are cached on disk (`McpCache`), so a session advertises the tools of a known
//!   server without starting it; the process starts on the first tool call.
//! - Tools beyond a schema-token budget are deferred (`McpToolSet`): only their names are
//!   advertised until the model activates them with `tool_search`.
//! - Server implements a strict stdio JSON-RPC 2.0 server (`serve_stdio` / `serve_io`) whose
//!   `stdout` carries exclusively JSON-RPC frames.

mod cache;
mod client;
mod config;
mod http;
mod manager;
mod protocol;
mod resources;
mod sanitize;
mod server;
mod stdio;
mod toolset;

pub use cache::{Lookup, McpCache};
pub use client::{McpClient, McpHandle, McpToolAdapter};
pub use config::{
    DEFAULT_TIMEOUT_SECS, McpServerConfig, McpTransport, discover_mcp_configs, parse_mcp_json,
};
pub use manager::{McpManager, McpServerStatus};
pub use protocol::{
    CallToolResult, Implementation, InitializeResult, JsonRpcError, JsonRpcId,
    MAX_RESOURCE_OUTPUT_BYTES, McpResource, McpToolSpec, PROTOCOL_VERSION, ResourceContent,
    ResourcesListResult, ResourcesReadResult, sanitize_mcp_error, sanitize_tool_segment,
};
pub use resources::{AdvertisedResource, McpResources, McpResourcesBinder};
pub use server::{McpServerHandler, serve_io, serve_stdio};
pub use toolset::McpToolSet;
