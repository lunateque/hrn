//! Wire types for JSON-RPC 2.0 and the Model Context Protocol (MCP).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;

/// Default MCP protocol version advertised during `initialize`.
pub const PROTOCOL_VERSION: &str = "2025-03-26";

/// JSON-RPC 2.0 request/response identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    /// Numeric identifier.
    Number(u64),
    /// String identifier.
    String(String),
}

impl fmt::Display for JsonRpcId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => f.write_str(s),
        }
    }
}

/// Outbound JSON-RPC 2.0 request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct JsonRpcRequest {
    pub(crate) jsonrpc: String,
    pub(crate) id: JsonRpcId,
    pub(crate) method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) params: Option<Value>,
}

impl JsonRpcRequest {
    pub(crate) fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id: JsonRpcId::Number(id),
            method: method.into(),
            params,
        }
    }
}

/// Outbound JSON-RPC 2.0 notification (no `id`, expects no response).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct JsonRpcNotification {
    pub(crate) jsonrpc: String,
    pub(crate) method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) params: Option<Value>,
}

impl JsonRpcNotification {
    pub(crate) fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 error object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Standard JSON-RPC error code (e.g. `-32601` Method not found).
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Optional structured error details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

/// JSON-RPC 2.0 response envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct JsonRpcResponse {
    pub(crate) jsonrpc: String,
    #[serde(default)]
    pub(crate) id: Option<JsonRpcId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub(crate) fn ok(id: JsonRpcId, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id: Some(id),
            result: Some(result),
            error: None,
        }
    }

    pub(crate) fn err(id: Option<JsonRpcId>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Client or server identity advertised during MCP `initialize`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Implementation {
    /// Program or server name.
    pub name: String,
    /// Version string.
    #[serde(default)]
    pub version: String,
}

/// Result payload of MCP `initialize`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeResult {
    /// Negotiated protocol version string.
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: String,
    /// Server implementation metadata.
    #[serde(rename = "serverInfo", default)]
    pub server_info: Implementation,
    /// Raw server capabilities object.
    #[serde(default)]
    pub capabilities: Value,
}

/// Parameters sent on MCP `initialize`.
pub(crate) fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "hrn",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// One tool advertised by an MCP server (`tools/list`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolSpec {
    /// Tool name on the remote MCP server.
    pub name: String,
    /// Optional model-facing tool description.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema for tool arguments.
    #[serde(rename = "inputSchema", default = "default_object_schema")]
    pub input_schema: Value,
}

pub(crate) fn default_object_schema() -> Value {
    json!({
        "type": "object",
        "properties": {}
    })
}

/// Result payload of MCP `tools/list`.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct ToolsListResult {
    #[serde(default)]
    pub(crate) tools: Vec<McpToolSpec>,
    #[serde(rename = "nextCursor", default)]
    pub(crate) next_cursor: Option<String>,
}

/// Maximum rendered text length returned by [`ResourcesReadResult::render_text_capped`] (256 KiB).
pub const MAX_RESOURCE_OUTPUT_BYTES: usize = 256 * 1024;
pub use crate::sanitize::sanitize_mcp_error;

/// One resource advertised by an MCP server (`resources/list`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResource {
    /// Resource URI (e.g. `file://server/doc`, `docs://guide`).
    pub uri: String,
    /// Human-readable resource name.
    pub name: String,
    /// Optional description of the resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional MIME type (e.g. `text/markdown`, `application/json`).
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Result payload of MCP `resources/list`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcesListResult {
    /// Resources returned in this page.
    #[serde(default)]
    pub resources: Vec<McpResource>,
    /// Optional pagination cursor for the next page.
    #[serde(
        rename = "nextCursor",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub next_cursor: Option<String>,
}

/// One content block returned by MCP `resources/read`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceContent {
    /// Resource URI.
    pub uri: String,
    /// Optional MIME type.
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// UTF-8 text payload, when textual.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64-encoded binary payload, when binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// Result payload of MCP `resources/read`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcesReadResult {
    /// Ordered resource content blocks.
    #[serde(default)]
    pub contents: Vec<ResourceContent>,
}

impl ResourcesReadResult {
    /// Render all content blocks as readable UTF-8 text, capping total size at `max_bytes`.
    #[must_use]
    pub fn render_text_capped(&self, max_bytes: usize) -> String {
        let mut parts = Vec::new();
        for item in &self.contents {
            if let Some(text) = &item.text {
                parts.push(text.clone());
            } else if let Some(blob) = &item.blob {
                let mime = item
                    .mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream");
                parts.push(format!(
                    "[binary resource: {} ({mime}, {} base64 chars)]",
                    item.uri,
                    blob.len()
                ));
            } else {
                parts.push(format!("[empty resource: {}]", item.uri));
            }
        }
        let joined = parts.join("\n");
        let limit = max_bytes.min(MAX_RESOURCE_OUTPUT_BYTES);
        if joined.len() <= limit {
            return joined;
        }
        let cut = joined.floor_char_boundary(limit);
        format!(
            "{}\n... [truncated MCP resource output at {limit} bytes ({} bytes total)]",
            joined.get(..cut).unwrap_or_default(),
            joined.len()
        )
    }
}

/// Result payload of MCP `tools/call`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallToolResult {
    /// Sequence of MCP content blocks (`text`, `image`, `resource`, etc.).
    #[serde(default)]
    pub content: Vec<Value>,
    /// Optional structured result payload (MCP 2025-03-26).
    #[serde(
        rename = "structuredContent",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
    /// Whether the remote tool reported an execution error.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

impl CallToolResult {
    /// Build a plain text success result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![json!({ "type": "text", "text": text.into() })],
            structured_content: None,
            is_error: false,
        }
    }

    /// Build a plain text error result (`isError = true`).
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![json!({ "type": "text", "text": message.into() })],
            structured_content: None,
            is_error: true,
        }
    }

    /// Flatten content blocks into readable text suitable for a harness `ToolOutput`.
    pub fn render_text(&self) -> String {
        let mut parts = Vec::new();
        for block in &self.content {
            let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
            match kind {
                "text" => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        parts.push(t.to_owned());
                    }
                }
                "image" | "audio" => {
                    let mime = block
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("application/octet-stream");
                    let bytes = block
                        .get("data")
                        .and_then(Value::as_str)
                        .map_or(0, str::len);
                    parts.push(format!("[{kind}: {mime}, {bytes} base64 chars]"));
                }
                "resource" => {
                    if let Some(res) = block.get("resource") {
                        let uri = res.get("uri").and_then(Value::as_str).unwrap_or("resource");
                        if let Some(t) = res.get("text").and_then(Value::as_str) {
                            parts.push(format!("[resource: {uri}]\n{t}"));
                        } else {
                            parts.push(format!("[resource: {uri}]"));
                        }
                    }
                }
                _ => {
                    parts.push(block.to_string());
                }
            }
        }
        if parts.is_empty()
            && let Some(structured) = &self.structured_content
        {
            return serde_json::to_string_pretty(structured)
                .unwrap_or_else(|_| structured.to_string());
        }
        parts.join("\n")
    }
}

/// Sanitize a server or tool name segment for use inside `mcp__<server>__<tool>`.
pub fn sanitize_tool_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unnamed".to_owned()
    } else {
        out
    }
}

/// The qualified-name prefix shared by every tool of `server`: `mcp__<server>__`.
pub(crate) fn tool_prefix(server: &str) -> String {
    format!("mcp__{}__", sanitize_tool_segment(server))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_text_resource_and_structured_content() {
        let res = CallToolResult {
            content: vec![
                json!({"type": "text", "text": "hello"}),
                json!({"type": "resource", "resource": {"uri": "file:///a.txt", "text": "body"}}),
                json!({"type": "image", "mimeType": "image/png", "data": "abcd"}),
            ],
            structured_content: None,
            is_error: false,
        };
        let text = res.render_text();
        assert!(text.contains("hello"));
        assert!(text.contains("[resource: file:///a.txt]\nbody"));
        assert!(text.contains("[image: image/png, 4 base64 chars]"));

        let structured = CallToolResult {
            content: Vec::new(),
            structured_content: Some(json!({"ok": true})),
            is_error: false,
        };
        assert!(structured.render_text().contains("\"ok\": true"));
    }

    #[test]
    fn sanitizes_tool_name_segments() {
        assert_eq!(sanitize_tool_segment("my.server/v1"), "my_server_v1");
        assert_eq!(sanitize_tool_segment("agy-bridge"), "agy-bridge");
        assert_eq!(sanitize_tool_segment(""), "unnamed");
    }
}
