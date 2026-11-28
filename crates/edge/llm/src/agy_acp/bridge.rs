//! Loopback Streamable HTTP MCP bridge (`127.0.0.1:0`) exposing `hrn` tools as `hrn__*`.
//!
//! Security and liveness guarantees:
//! - Binds strictly to IPv4 loopback (`127.0.0.1:0`).
//! - Authenticates every request with a 256-bit `x-hrn-bridge-token` checked in constant time.
//! - Rejects `GET` (`405`) and unauthenticated requests (`403`).
//! - Streams SSE `notifications/progress` every 15s (configurable) whenever `_meta.progressToken`
//!   is supplied, preventing long-running tools (e.g. `cargo test`) from hitting MCP timeouts.
//! - Fails closed (`isError: true`) on per-call timeout (`480s`) or cancellation.

use super::bridge_http::handle_http;
use super::schema::wire_tool_name;
use hrn_kernel::{HrnError, Result, ToolCall, ToolSpec};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

/// Prefix applied to all `hrn` tool names exposed over the loopback MCP bridge.
pub const BRIDGE_TOOL_PREFIX: &str = "hrn__";
/// HTTP header carrying the 256-bit per-runtime bridge token.
pub const BRIDGE_TOKEN_HEADER: &str = "x-hrn-bridge-token";
/// Default SSE `notifications/progress` keepalive interval (15 seconds).
pub const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_secs(15);
/// Default fail-closed per-tool-call timeout (480s < 600s `turn_timeout_ms`).
pub const DEFAULT_TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(480);

/// Tool result delivered back to a suspended MCP `tools/call` handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeToolResult {
    /// Text returned to the model.
    pub content: String,
    /// Whether tool execution failed or was cancelled.
    pub is_error: bool,
}

/// Configuration knobs for [`McpBridge`] (defaults match production spec; overridable in tests).
#[derive(Clone, Copy, Debug)]
pub struct McpBridgeOptions {
    /// SSE `notifications/progress` keepalive interval when `_meta.progressToken` is present.
    pub progress_interval: Duration,
    /// Maximum wait for `hrn` to resolve a pending `tools/call`.
    pub call_timeout: Duration,
}

impl Default for McpBridgeOptions {
    fn default() -> Self {
        Self {
            progress_interval: DEFAULT_PROGRESS_INTERVAL,
            call_timeout: DEFAULT_TOOL_CALL_TIMEOUT,
        }
    }
}

/// Process-wide `tools/call` sequence: bridged call ids (`hrn_call_<n>`) stay unique across every
/// runtime lane, so concurrent sessions never see the same id.
pub(crate) static NEXT_CALL_SEQ: AtomicU64 = AtomicU64::new(1);

pub(crate) struct BridgeState {
    pub(crate) token: String,
    pub(crate) tools: RwLock<Vec<ToolSpec>>,
    pub(crate) wire_to_real: RwLock<HashMap<String, String>>,
    pub(crate) pending_calls: Mutex<HashMap<String, oneshot::Sender<BridgeToolResult>>>,
    pub(crate) call_sink: Mutex<Option<mpsc::UnboundedSender<ToolCall>>>,
    pub(crate) options: McpBridgeOptions,
}

/// Running loopback HTTP MCP server shut down cleanly on [`Drop`].
pub struct McpBridge {
    addr: SocketAddr,
    state: Arc<BridgeState>,
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl Drop for McpBridge {
    fn drop(&mut self) {
        self.fail_all_pending("MCP bridge shut down");
        if let Some(tx) = self
            .shutdown_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = tx.send(());
        }
    }
}

impl McpBridge {
    /// Start a new bridge bound to `127.0.0.1:0` with a fresh 256-bit random token.
    pub async fn start(options: McpBridgeOptions) -> Result<Self> {
        let token = generate_token_hex_256()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| HrnError::Io(format!("bind loopback MCP bridge: {e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| HrnError::Io(format!("read bridge local_addr: {e}")))?;

        let state = Arc::new(BridgeState {
            token,
            tools: RwLock::new(Vec::new()),
            wire_to_real: RwLock::new(HashMap::new()),
            pending_calls: Mutex::new(HashMap::new()),
            call_sink: Mutex::new(None),
            options,
        });

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let serve_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else {
                            continue;
                        };
                        let conn_state = Arc::clone(&serve_state);
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let svc = service_fn(move |req| {
                                handle_http(req, Arc::clone(&conn_state))
                            });
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, svc)
                                .await;
                        });
                    }
                }
            }
        });

        Ok(Self {
            addr,
            state,
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
        })
    }

    /// Loopback URL (`http://127.0.0.1:<port>/mcp`).
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    /// 64-char hex authentication token required in `x-hrn-bridge-token`.
    pub fn token(&self) -> &str {
        &self.state.token
    }

    /// ACP v1 `mcpServers` entry JSON object suitable for `session/new` and `session/resume`.
    pub fn acp_mcp_server_entry(&self) -> Value {
        json!({
            "type": "http",
            "name": "hrn-bridge",
            "url": self.url(),
            "headers": [
                {
                    "name": BRIDGE_TOKEN_HEADER,
                    "value": self.token()
                }
            ]
        })
    }

    /// Replace the active [`ToolSpec`] catalog served by `tools/list`.
    pub fn set_tools(&self, tools: Vec<ToolSpec>) {
        let mut wire_map = HashMap::with_capacity(tools.len());
        for spec in &tools {
            let wire = wire_tool_name(&spec.name);
            let real = spec
                .name
                .strip_prefix(BRIDGE_TOOL_PREFIX)
                .unwrap_or(&spec.name)
                .to_owned();
            wire_map.insert(wire, real);
        }
        *self
            .state
            .wire_to_real
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = wire_map;
        *self
            .state
            .tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = tools;
    }

    /// Attach a receiver channel for incoming `tools/call` requests during an active turn.
    pub fn attach_call_sink(&self) -> mpsc::UnboundedReceiver<ToolCall> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self
            .state
            .call_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
        rx
    }

    /// Detach the current turn's tool-call receiver.
    pub fn detach_call_sink(&self) {
        *self
            .state
            .call_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Resolve one pending `tools/call` by `call_id`. Returns `true` if a waiting call existed.
    pub fn resolve_call(&self, call_id: &str, result: BridgeToolResult) -> bool {
        let sender = self
            .state
            .pending_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(call_id);
        sender.is_some_and(|tx| tx.send(result).is_ok())
    }

    /// Return the currently suspended `call_id`s awaiting `hrn` tool results.
    pub fn pending_call_ids(&self) -> Vec<String> {
        self.state
            .pending_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// Immediately fail-close every suspended `tools/call` (used on `CancelFlag` / abort / drop).
    pub fn fail_all_pending(&self, reason: &str) {
        let drained: Vec<_> = {
            let mut guard = self
                .state
                .pending_calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.drain().map(|(_, tx)| tx).collect()
        };
        for tx in drained {
            let _ = tx.send(BridgeToolResult {
                content: reason.to_owned(),
                is_error: true,
            });
        }
    }
}

/// Constant-time equality comparison for bearer/bridge tokens.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn generate_token_hex_256() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| HrnError::Internal(format!("getrandom failed: {e}")))?;
    let mut out = String::with_capacity(64);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    Ok(out)
}
