//! ACP v1 JSON-RPC 2.0 wire types, bounded NDJSON line reader, and quota/auth error classification.

pub use super::classify::{
    classify_jsonrpc_error, detect_quota_exhaustion, now_epoch_secs, retry_after_from_quota,
    thinking_level_to_acp,
};
use hrn_kernel::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Hard cap on a single NDJSON stdout line from `agy acp` (8 MiB).
pub const MAX_NDJSON_LINE_BYTES: usize = 8 * 1024 * 1024;
/// JSON-RPC error code emitted by `agy acp` when the CLI is not authenticated.
pub const ACP_AUTH_REQUIRED_CODE: i64 = -32000;

/// One authentication method advertised by `agy acp` `initialize`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthMethodInfo {
    /// Wire method id (`oauth-personal`, `oauth-business`, `gemini-api-key`, `agent-platform`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: String,
}

/// Capabilities reported by `agy acp` in response to `initialize`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "ACP v1 initialize capability flags mirror the wire protocol directly"
)]
pub struct AcpCapabilities {
    /// ACP protocol version negotiated (`1`).
    pub protocol_version: u64,
    /// Server name (e.g. `"Antigravity ACP Server"`).
    pub agent_name: String,
    /// Server version (e.g. `"1.5.18"`).
    pub agent_version: String,
    /// Whether `session/load` is advertised.
    pub load_session: bool,
    /// Whether `session/resume` is advertised.
    pub supports_resume: bool,
    /// Whether `session/close` is advertised.
    pub supports_close: bool,
    /// Whether native `session/fork` is advertised (auto-enabled on future `agy` releases).
    pub supports_fork: bool,
    /// Whether inline image blocks are accepted in `session/prompt`.
    pub supports_image: bool,
    /// Supported auth methods returned by `initialize`.
    pub auth_methods: Vec<AuthMethodInfo>,
}

/// One model option returned by `session/new` (`configOptions` category `model`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpModelOption {
    /// Canonical model value accepted by `session/set_config_option(configId="model")`.
    pub value: String,
    /// Human-readable display name.
    pub name: String,
}

/// Parsed response to ACP `session/new`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpSessionNewResult {
    /// Newly created ACP session UUID.
    pub session_id: String,
    /// Base models advertised in `configOptions` (`category == "model"`).
    pub available_models: Vec<AcpModelOption>,
    /// Current model value selected by default.
    pub current_model: Option<String>,
    /// Supported `thought_level` values (`low`, `medium`, `high`, `xhigh`, ...).
    pub thought_levels: Vec<String>,
    /// Antigravity subscription tier label if reported (`_meta.antigravity.tier.name`).
    pub tier_name: Option<String>,
}

/// Streamed update decoded from an ACP `session/update` notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcpUpdate {
    /// Visible assistant text delta.
    ContentDelta(String),
    /// Reasoning / thinking summary delta.
    ReasoningDelta(String),
    /// Context/token usage update.
    UsageUpdate {
        /// Tokens currently used.
        used: u64,
        /// Context window size.
        size: u64,
    },
    /// Other informational notification (`available_commands_update`, `session_info_update`, etc.).
    Other,
}

/// Inline quota metadata optionally attached to `PromptResponse._meta.quota`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InlineQuota {
    /// RFC 3339 reset timestamp if provided.
    pub reset_time: Option<String>,
    /// Remaining quota fraction in `0.0..=1.0`, if provided.
    pub remaining_fraction: Option<f64>,
    /// Explicit retry-after seconds if provided.
    pub retry_after_secs: Option<u64>,
}

/// Completed result of ACP `session/prompt`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AcpPromptResult {
    /// Raw ACP stop reason (`end_turn`, `max_tokens`, `refusal`, `cancelled`, ...).
    pub stop_reason: String,
    /// Parsed token usage telemetry if present.
    pub usage: Option<Usage>,
    /// Parsed inline quota metadata from `_meta.quota` if present.
    pub quota: Option<InlineQuota>,
}

/// Server-initiated `session/request_permission` payload.
#[derive(Clone, Debug)]
pub struct AcpPermissionRequest {
    /// JSON-RPC request id to echo in the reply.
    pub rpc_id: Value,
    /// Owning ACP session id.
    pub session_id: String,
    /// Tool name or title extracted from `toolCall`.
    pub tool_name: String,
    /// MCP server name if identifiable.
    pub server_name: Option<String>,
    /// First allow option id advertised in `options`, if any.
    pub allow_option_id: Option<String>,
    /// First reject/deny option id advertised in `options`, if any.
    pub reject_option_id: Option<String>,
}

/// Read one `\n`-terminated UTF-8 line from `reader`, failing cleanly if the line exceeds `limit` bytes.
pub async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if buf.len().saturating_add(pos) > limit {
                reader.consume(pos + 1);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("ACP NDJSON line exceeded {limit} byte limit"),
                ));
            }
            buf.extend_from_slice(available.get(..pos).unwrap_or_default());
            reader.consume(pos + 1);
            break;
        }
        let chunk_len = available.len();
        if buf.len().saturating_add(chunk_len) > limit {
            reader.consume(chunk_len);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ACP NDJSON line exceeded {limit} byte limit"),
            ));
        }
        buf.extend_from_slice(available);
        reader.consume(chunk_len);
    }
    if buf.last() == Some(&b'\r') {
        let _ = buf.pop();
    }
    String::from_utf8(buf).map(Some).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ACP stdout contained invalid UTF-8: {e}"),
        )
    })
}

/// Parse `initialize` response `result` object into [`AcpCapabilities`].
pub fn parse_capabilities(result: &Value) -> AcpCapabilities {
    let agent_info = result.get("agentInfo");
    let caps = result.get("agentCapabilities");
    let sess_caps = caps
        .and_then(|c| c.get("sessionCapabilities"))
        .or_else(|| caps.and_then(|c| c.get("session_capabilities")));
    let prompt_caps = caps
        .and_then(|c| c.get("promptCapabilities"))
        .or_else(|| caps.and_then(|c| c.get("prompt_capabilities")));
    let auth_methods = result
        .get("authMethods")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let id = item.get("id")?.as_str()?.to_owned();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_owned();
                    let description = item
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    Some(AuthMethodInfo {
                        id,
                        name,
                        description,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    AcpCapabilities {
        protocol_version: result
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .unwrap_or(1),
        agent_name: agent_info
            .and_then(|a| a.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("Antigravity ACP Server")
            .to_owned(),
        agent_version: agent_info
            .and_then(|a| a.get("version"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        load_session: caps
            .and_then(|c| c.get("loadSession").or_else(|| c.get("load_session")))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        supports_resume: sess_caps.and_then(|s| s.get("resume")).is_some(),
        supports_close: sess_caps.and_then(|s| s.get("close")).is_some(),
        supports_fork: sess_caps.and_then(|s| s.get("fork")).is_some(),
        supports_image: prompt_caps
            .and_then(|p| p.get("image"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        auth_methods,
    }
}

/// Parse `session/new` response `result` object into [`AcpSessionNewResult`].
pub fn parse_session_new(result: &Value) -> Option<AcpSessionNewResult> {
    let session_id = result.get("sessionId")?.as_str()?.to_owned();
    let mut available_models = Vec::new();
    let mut current_model = None;
    let mut thought_levels = Vec::new();

    if let Some(opts) = result.get("configOptions").and_then(Value::as_array) {
        for opt in opts {
            let category = opt
                .get("category")
                .or_else(|| opt.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if category == "model" {
                current_model = opt
                    .get("currentValue")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(items) = opt.get("options").and_then(Value::as_array) {
                    for item in items {
                        if let Some(val) = item.get("value").and_then(Value::as_str) {
                            let name = item
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or(val)
                                .to_owned();
                            available_models.push(AcpModelOption {
                                value: val.to_owned(),
                                name,
                            });
                        }
                    }
                }
            } else if category == "thought_level"
                && let Some(items) = opt.get("options").and_then(Value::as_array)
            {
                for item in items {
                    if let Some(val) = item.get("value").and_then(Value::as_str) {
                        thought_levels.push(val.to_owned());
                    }
                }
            }
        }
    }

    if available_models.is_empty()
        && let Some(items) = result
            .get("models")
            .and_then(|m| m.get("availableModels"))
            .and_then(Value::as_array)
    {
        for item in items {
            if let Some(id) = item.get("modelId").and_then(Value::as_str) {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_owned();
                available_models.push(AcpModelOption {
                    value: id.to_owned(),
                    name,
                });
            }
        }
    }

    let tier_name = result
        .get("_meta")
        .and_then(|m| m.get("antigravity"))
        .and_then(|a| a.get("tier"))
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    Some(AcpSessionNewResult {
        session_id,
        available_models,
        current_model,
        thought_levels,
        tier_name,
    })
}

/// Parse one `session/update` notification `params` object into `(session_id, AcpUpdate)`.
pub fn parse_session_update(params: &Value) -> Option<(String, AcpUpdate)> {
    let session_id = params.get("sessionId")?.as_str()?.to_owned();
    let update = params.get("update")?;
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let parsed = match kind {
        "agent_message_chunk" | "text_delta" => extract_chunk_text(update)
            .filter(|s| !s.is_empty())
            .map_or(AcpUpdate::Other, AcpUpdate::ContentDelta),
        "agent_thought_chunk" | "thought" | "thinking_summary" => extract_chunk_text(update)
            .filter(|s| !s.is_empty())
            .map_or(AcpUpdate::Other, AcpUpdate::ReasoningDelta),
        "usage_update" => AcpUpdate::UsageUpdate {
            used: update.get("used").and_then(Value::as_u64).unwrap_or(0),
            size: update.get("size").and_then(Value::as_u64).unwrap_or(0),
        },
        _ => AcpUpdate::Other,
    };
    Some((session_id, parsed))
}

fn extract_chunk_text(update: &Value) -> Option<String> {
    if let Some(text) = update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
    {
        return Some(text.to_owned());
    }
    update
        .get("delta")
        .or_else(|| update.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Parse `session/prompt` JSON-RPC `result` object into [`AcpPromptResult`].
pub fn parse_prompt_result(result: &Value) -> AcpPromptResult {
    let stop_reason = result
        .get("stopReason")
        .or_else(|| result.get("stop_reason"))
        .and_then(Value::as_str)
        .unwrap_or("end_turn")
        .to_owned();
    let usage = result.get("usage").map(|u| {
        let prompt = u
            .get("inputTokens")
            .or_else(|| u.get("input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let completion = u
            .get("outputTokens")
            .or_else(|| u.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let total = u
            .get("totalTokens")
            .or_else(|| u.get("total_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or_else(|| prompt.saturating_add(completion));
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
            cache_read_tokens: u
                .get("cachedReadTokens")
                .or_else(|| u.get("cache_read_tokens"))
                .and_then(Value::as_u64),
            cache_write_tokens: u
                .get("cachedWriteTokens")
                .or_else(|| u.get("cache_write_tokens"))
                .and_then(Value::as_u64),
            reasoning_tokens: u
                .get("thoughtTokens")
                .or_else(|| u.get("thinking_tokens"))
                .and_then(Value::as_u64),
            cost_micros: None,
        }
    });
    let quota = result
        .get("_meta")
        .and_then(|m| m.get("quota"))
        .map(|q| InlineQuota {
            reset_time: q
                .get("reset_time")
                .or_else(|| q.get("resetTime"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            remaining_fraction: q
                .get("remaining_fraction")
                .or_else(|| q.get("remainingFraction"))
                .and_then(Value::as_f64),
            retry_after_secs: q
                .get("retry_after_secs")
                .or_else(|| q.get("retryAfterSecs"))
                .and_then(Value::as_u64),
        });
    AcpPromptResult {
        stop_reason,
        usage,
        quota,
    }
}

/// Parse a server-initiated `session/request_permission` request.
pub fn parse_permission_request(rpc_id: Value, params: &Value) -> AcpPermissionRequest {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let tool_call = params.get("toolCall").or_else(|| params.get("tool_call"));
    let tool_name = tool_call
        .and_then(|tc| {
            tc.get("name")
                .or_else(|| tc.get("toolName"))
                .or_else(|| tc.get("title"))
                .or_else(|| tc.get("kind"))
        })
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let server_name = tool_call
        .and_then(|tc| {
            tc.get("serverName")
                .or_else(|| tc.get("ServerName"))
                .or_else(|| tc.get("server_name"))
                .or_else(|| tc.get("_meta").and_then(|m| m.get("mcpServer")))
                .or_else(|| {
                    tc.get("rawInput")
                        .and_then(|r| r.get("serverName").or_else(|| r.get("ServerName")))
                })
        })
        .and_then(Value::as_str)
        .map(str::to_owned);

    let mut allow_option_id = None;
    let mut reject_option_id = None;
    if let Some(options) = params.get("options").and_then(Value::as_array) {
        for opt in options {
            let kind = opt
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let id = opt
                .get("optionId")
                .or_else(|| opt.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if allow_option_id.is_none() && kind.starts_with("allow") {
                allow_option_id.clone_from(&id);
            }
            if reject_option_id.is_none() && (kind.starts_with("reject") || kind.contains("deny")) {
                reject_option_id = id;
            }
        }
    }

    AcpPermissionRequest {
        rpc_id,
        session_id,
        tool_name,
        server_name,
        allow_option_id,
        reject_option_id,
    }
}

/// Fail-closed check: return `true` ONLY when a permission request targets `hrn-bridge` MCP tools.
pub fn is_hrn_bridge_permission(req: &AcpPermissionRequest) -> bool {
    let lower = req.tool_name.trim().to_ascii_lowercase();
    let stripped = lower.strip_prefix("run ").unwrap_or(&lower);
    let is_bridge_tool_prefix = stripped.starts_with("hrn__")
        || stripped.starts_with("mcp_hrn-bridge_")
        || stripped.starts_with("mcp__hrn-bridge__")
        || stripped.starts_with("hrn-bridge:")
        || stripped.starts_with("hrn-bridge/");
    if is_bridge_tool_prefix {
        return true;
    }
    req.server_name
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("hrn-bridge"))
        && (stripped.is_empty() || stripped == "call_mcp_tool")
}
