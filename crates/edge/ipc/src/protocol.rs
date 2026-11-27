//! Wire protocol DTOs for the `hrn` loopback HTTP + SSE daemon API (`/v1/*`).

use hrn_kernel::{AgentEvent, AgentStatus, PermissionMode, ThinkingLevel};
use serde::{Deserialize, Serialize};

/// Response payload for `GET /v1/health`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Static `"ok"` status marker.
    pub status: String,
    /// Daemon process ID.
    pub pid: u32,
    /// `hrn` crate version.
    pub version: String,
    /// Persistent daemon host-key fingerprint (`SHA256:<hex>`).
    pub host_key: String,
    /// Number of known sessions.
    pub sessions: usize,
}

/// Compact session summary returned by `/v1/sessions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummaryDto {
    /// Session identifier.
    pub id: String,
    /// Human-readable session title.
    pub title: String,
    /// Working directory of the session.
    pub cwd: String,
    /// Configured model alias or ID.
    pub model: Option<String>,
    /// Current agent status.
    pub status: AgentStatus,
    /// Whether this daemon currently owns a running turn for the session.
    #[serde(default)]
    pub active_turn: bool,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: u64,
}

/// Request body for `POST /v1/sessions`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    /// Session title.
    pub title: String,
    /// Working directory on the daemon host.
    pub cwd: String,
    /// Optional model alias or role.
    #[serde(default)]
    pub model: Option<String>,
    /// Thinking effort level.
    #[serde(default)]
    pub thinking: ThinkingLevel,
    /// Permission mode for tool calls.
    #[serde(default)]
    pub permission_mode: PermissionMode,
}

/// Request body for `POST /v1/sessions/{id}/turns`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartTurnRequest {
    /// User prompt for the turn.
    pub prompt: String,
    /// Optional auto-approve flag for mutating tools in headless daemon turns.
    #[serde(default)]
    pub auto_approve: bool,
}

/// Request body for `POST /v1/sessions/{id}/steer`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SteerRequest {
    /// Steering guidance injected into the active turn.
    pub text: String,
}

/// Request body for `POST /v1/sessions/{id}/approve`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApproveRequest {
    /// Tool call identifier awaiting approval.
    pub call_id: String,
    /// `true` to allow the tool call, `false` to deny it.
    pub approved: bool,
}

/// Request body for `POST /v1/sessions/{id}/restore`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RestoreSessionRequest {
    /// Optional auto-approve flag for mutating tools in headless daemon turns.
    #[serde(default)]
    pub auto_approve: bool,
}

/// Response body for `POST /v1/sessions/{id}/restore`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreSessionResponse {
    /// `true` when an interrupted turn was restored and started; `false` when nothing was pending.
    pub restored: bool,
}

/// Generic acknowledgement response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkResponse {
    /// Always `true` on success.
    pub ok: bool,
}

/// JSON error envelope returned for non-2xx responses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Human-readable error message.
    pub error: String,
}

/// One SSE event envelope emitted on `GET /v1/sessions/{id}/events`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonEvent {
    /// Monotonic event sequence number within the session.
    pub seq: u64,
    /// Session identifier.
    pub session_id: String,
    /// Kernel agent lifecycle event.
    pub event: AgentEvent,
}
