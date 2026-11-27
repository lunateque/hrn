//! Strict stdio MCP JSON-RPC 2.0 server (`hrn mcp serve`).
//!
//! `stdout` carries exclusively newline-delimited JSON-RPC 2.0 responses — never logs or prose.

use crate::protocol::{
    CallToolResult, JsonRpcId, JsonRpcResponse, McpResource, McpToolSpec, PROTOCOL_VERSION,
    ResourcesReadResult,
};
use hrn_kernel::{BoxFuture, HrnError, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const ERR_PARSE: i64 = -32700;
const ERR_INVALID_PARAMS: i64 = -32602;
const ERR_METHOD_NOT_FOUND: i64 = -32601;

/// Handler implemented by a host (e.g. `hrn mcp serve`) to expose tools over MCP stdio.
pub trait McpServerHandler: Send + Sync {
    /// Server name returned in `initialize.serverInfo.name`.
    fn server_name(&self) -> &'static str {
        "hrn"
    }

    /// Server version returned in `initialize.serverInfo.version`.
    fn server_version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// List tools exposed by this server.
    fn list_tools(&self) -> Vec<McpToolSpec>;

    /// Execute a tool requested via `tools/call`.
    fn call_tool(&self, name: &str, arguments: Value) -> BoxFuture<Result<CallToolResult>>;

    /// List resources advertised via `resources/list` (defaults to empty).
    fn list_resources(&self) -> Vec<McpResource> {
        Vec::new()
    }

    /// Read one resource requested via `resources/read`.
    fn read_resource(&self, uri: &str) -> BoxFuture<Result<ResourcesReadResult>> {
        let msg = format!("resource not found: `{uri}`");
        Box::pin(async move { Err(HrnError::Tool(msg)) })
    }
}

#[derive(Debug, Deserialize)]
struct ReadResourceParams {
    uri: String,
}

#[derive(Debug, Deserialize)]
struct IncomingMessage {
    #[serde(default)]
    id: Option<JsonRpcId>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct CallToolParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

/// Serve MCP over process `stdin`/`stdout` until EOF.
pub async fn serve_stdio<H: McpServerHandler>(handler: &H) -> Result {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_io(stdin, stdout, handler).await
}

/// Serve MCP over arbitrary async `reader` and `writer` (used by `serve_stdio` and unit tests).
pub async fn serve_io<R, W, H>(mut reader: R, mut writer: W, handler: &H) -> Result
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
    H: McpServerHandler,
{
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| HrnError::Io(format!("mcp server read stdin: {e}")))?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = handle_line(trimmed, handler).await;
        if let Some(resp) = response {
            let mut out = serde_json::to_string(&resp)
                .map_err(|e| HrnError::Internal(format!("mcp server encode response: {e}")))?;
            out.push('\n');
            writer
                .write_all(out.as_bytes())
                .await
                .map_err(|e| HrnError::Io(format!("mcp server write stdout: {e}")))?;
            writer
                .flush()
                .await
                .map_err(|e| HrnError::Io(format!("mcp server flush stdout: {e}")))?;
        }
    }
    Ok(())
}

async fn handle_line<H: McpServerHandler>(line: &str, handler: &H) -> Option<JsonRpcResponse> {
    let msg: IncomingMessage = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(err) => {
            return Some(JsonRpcResponse::err(
                None,
                ERR_PARSE,
                format!("parse error: {err}"),
            ));
        }
    };
    let method = msg.method.as_deref().unwrap_or("");
    let Some(id) = msg.id else {
        // Notifications (e.g. `notifications/initialized`, `notifications/cancelled`) get no reply.
        return None;
    };

    let response = match method {
        "initialize" => {
            let mut caps = json!({ "tools": { "listChanged": false } });
            if !handler.list_resources().is_empty()
                && let Some(obj) = caps.as_object_mut()
            {
                obj.insert("resources".to_owned(), json!({ "listChanged": false }));
            }
            JsonRpcResponse::ok(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": caps,
                    "serverInfo": {
                        "name": handler.server_name(),
                        "version": handler.server_version(),
                    }
                }),
            )
        }
        "ping" => JsonRpcResponse::ok(id, json!({})),
        "tools/list" => {
            let tools = handler.list_tools();
            JsonRpcResponse::ok(id, json!({ "tools": tools }))
        }
        "tools/call" => dispatch_tool_call(id, msg.params, handler).await,
        "resources/list" => {
            let resources = handler.list_resources();
            JsonRpcResponse::ok(id, json!({ "resources": resources }))
        }
        "resources/read" => dispatch_read_resource(id, msg.params, handler).await,
        other => JsonRpcResponse::err(
            Some(id),
            ERR_METHOD_NOT_FOUND,
            format!("method not found: `{other}`"),
        ),
    };
    Some(response)
}

async fn dispatch_read_resource<H: McpServerHandler>(
    id: JsonRpcId,
    params: Option<Value>,
    handler: &H,
) -> JsonRpcResponse {
    let Some(raw_params) = params else {
        return JsonRpcResponse::err(Some(id), ERR_INVALID_PARAMS, "missing params");
    };
    let parsed: ReadResourceParams = match serde_json::from_value(raw_params) {
        Ok(p) => p,
        Err(err) => {
            return JsonRpcResponse::err(
                Some(id),
                ERR_INVALID_PARAMS,
                format!("invalid resources/read params: {err}"),
            );
        }
    };
    match handler.read_resource(&parsed.uri).await {
        Ok(res) => match serde_json::to_value(res) {
            Ok(val) => JsonRpcResponse::ok(id, val),
            Err(err) => JsonRpcResponse::err(
                Some(id),
                -32603,
                format!("serialize resource result: {err}"),
            ),
        },
        Err(err) => JsonRpcResponse::err(Some(id), -32002, err.to_string()),
    }
}

async fn dispatch_tool_call<H: McpServerHandler>(
    id: JsonRpcId,
    params: Option<Value>,
    handler: &H,
) -> JsonRpcResponse {
    let Some(raw_params) = params else {
        return JsonRpcResponse::err(Some(id), ERR_INVALID_PARAMS, "missing params");
    };
    let parsed: CallToolParams = match serde_json::from_value(raw_params) {
        Ok(p) => p,
        Err(err) => {
            return JsonRpcResponse::err(
                Some(id),
                ERR_INVALID_PARAMS,
                format!("invalid tools/call params: {err}"),
            );
        }
    };
    let result = match handler.call_tool(&parsed.name, parsed.arguments).await {
        Ok(res) => res,
        Err(err) => CallToolResult::error(err.to_string()),
    };
    match serde_json::to_value(result) {
        Ok(val) => JsonRpcResponse::ok(id, val),
        Err(err) => JsonRpcResponse::err(Some(id), -32603, format!("serialize tool result: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::default_object_schema;

    struct EchoHandler;

    impl McpServerHandler for EchoHandler {
        fn list_tools(&self) -> Vec<McpToolSpec> {
            vec![McpToolSpec {
                name: "echo".to_owned(),
                description: Some("Echo message".to_owned()),
                input_schema: default_object_schema(),
            }]
        }

        fn call_tool(&self, name: &str, arguments: Value) -> BoxFuture<Result<CallToolResult>> {
            let name = name.to_owned();
            Box::pin(async move {
                if name == "echo" {
                    let msg = arguments
                        .get("msg")
                        .and_then(Value::as_str)
                        .unwrap_or("empty");
                    Ok(CallToolResult::text(format!("echo:{msg}")))
                } else {
                    Err(HrnError::Tool(format!("unknown tool `{name}`")))
                }
            })
        }
    }

    #[tokio::test]
    async fn stdio_server_handles_initialize_list_call_ping_and_errors() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"echo\",\"arguments\":{\"msg\":\"hi\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"unknown/op\"}\n",
        );
        let mut output = Vec::new();
        serve_io(BufReader::new(input.as_bytes()), &mut output, &EchoHandler)
            .await
            .expect("serve");
        let text = String::from_utf8(output).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        // 5 requests -> 5 responses (notification produces no line).
        assert_eq!(lines.len(), 5, "{text}");

        let r1: Value = serde_json::from_str(lines.first().copied().unwrap_or("")).expect("r1");
        assert_eq!(r1["result"]["serverInfo"]["name"], "hrn");

        let r3: Value = serde_json::from_str(lines.get(2).copied().unwrap_or("")).expect("r3");
        assert_eq!(r3["result"]["tools"][0]["name"], "echo");

        let r4: Value = serde_json::from_str(lines.get(3).copied().unwrap_or("")).expect("r4");
        assert_eq!(r4["result"]["content"][0]["text"], "echo:hi");
        assert_eq!(r4["result"]["isError"], false);

        let r5: Value = serde_json::from_str(lines.get(4).copied().unwrap_or("")).expect("r5");
        assert_eq!(r5["error"]["code"], ERR_METHOD_NOT_FOUND);
    }
}
