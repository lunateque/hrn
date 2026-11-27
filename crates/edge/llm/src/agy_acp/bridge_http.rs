//! HTTP/JSON-RPC/SSE request handlers backing [`super::bridge::McpBridge`].

use super::bridge::{
    BRIDGE_TOKEN_HEADER, BRIDGE_TOOL_PREFIX, BridgeState, BridgeToolResult, constant_time_eq,
};
use super::schema::{sanitize_tool_schema, wire_tool_name};
use bytes::Bytes;
use hrn_kernel::ToolCall;
use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper::body::{Frame, Incoming};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::{mpsc, oneshot};

/// Maximum HTTP request body accepted by `/mcp` (8 MiB).
const MAX_HTTP_BODY_BYTES: usize = 8 * 1024 * 1024;

pub(crate) type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

pub(crate) async fn handle_http(
    req: Request<Incoming>,
    state: Arc<BridgeState>,
) -> std::result::Result<Response<BoxBody>, Infallible> {
    if req.uri().path() != "/mcp" {
        return Ok(empty_response(StatusCode::NOT_FOUND));
    }

    let provided = req
        .headers()
        .get(BRIDGE_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), state.token.as_bytes()) {
        return Ok(empty_response(StatusCode::FORBIDDEN));
    }

    if *req.method() != Method::POST {
        return Ok(empty_response(StatusCode::METHOD_NOT_ALLOWED));
    }

    let body_bytes = match read_bounded_body(req.into_body(), MAX_HTTP_BODY_BYTES).await {
        Ok(b) => b,
        Err(status) => return Ok(empty_response(status)),
    };
    let payload: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(err) => {
            return Ok(json_response(
                StatusCode::BAD_REQUEST,
                &jsonrpc_error(&Value::Null, -32700, &format!("parse error: {err}")),
            ));
        }
    };

    let method = payload.get("method").and_then(Value::as_str).unwrap_or("");
    let rpc_id = payload.get("id").cloned();

    match method {
        "notifications/initialized" | "notifications/cancelled" => {
            Ok(empty_response(StatusCode::ACCEPTED))
        }
        "initialize" => {
            let id = rpc_id.unwrap_or(Value::Null);
            Ok(json_response(
                StatusCode::OK,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {
                            "tools": { "listChanged": true }
                        },
                        "serverInfo": {
                            "name": "hrn-bridge",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }
                }),
            ))
        }
        "tools/list" => {
            let id = rpc_id.unwrap_or(Value::Null);
            let tools = build_mcp_tools_list(&state);
            Ok(json_response(
                StatusCode::OK,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "tools": tools }
                }),
            ))
        }
        "tools/call" => {
            let id = rpc_id.unwrap_or(Value::Null);
            let params = payload.get("params").unwrap_or(&Value::Null);
            Ok(handle_tool_call(id, params, state).await)
        }
        _ => {
            if let Some(id) = rpc_id {
                Ok(json_response(
                    StatusCode::OK,
                    &jsonrpc_error(&id, -32601, &format!("method not found: `{method}`")),
                ))
            } else {
                Ok(empty_response(StatusCode::ACCEPTED))
            }
        }
    }
}

fn build_mcp_tools_list(state: &BridgeState) -> Vec<Value> {
    let guard = state
        .tools
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .iter()
        .map(|spec| {
            let name = wire_tool_name(&spec.name);
            let input_schema = sanitize_tool_schema(&spec.parameters);
            json!({
                "name": name,
                "description": spec.description,
                "inputSchema": input_schema
            })
        })
        .collect()
}

async fn handle_tool_call(id: Value, params: &Value, state: Arc<BridgeState>) -> Response<BoxBody> {
    let raw_name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let tool_name = state
        .wire_to_real
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(raw_name)
        .cloned()
        .unwrap_or_else(|| {
            raw_name
                .strip_prefix(BRIDGE_TOOL_PREFIX)
                .unwrap_or(raw_name)
                .to_owned()
        });
    if tool_name.is_empty() {
        return json_response(
            StatusCode::OK,
            &jsonrpc_error(&id, -32602, "missing tool `name`"),
        );
    }

    let args_val = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let arguments = serde_json::to_string(&args_val).unwrap_or_else(|_| "{}".to_owned());
    let progress_token = params
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .cloned();

    let seq = state.next_call_seq.fetch_add(1, Ordering::Relaxed);
    let call_id = format!("hrn_call_{seq}");
    let (tx, rx) = oneshot::channel();
    state
        .pending_calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(call_id.clone(), tx);

    let dispatched = {
        let sink = state
            .call_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        sink.is_some_and(|s| {
            s.send(ToolCall {
                id: call_id.clone(),
                name: tool_name.clone(),
                arguments,
                signature: None,
            })
            .is_ok()
        })
    };

    if !dispatched {
        state
            .pending_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&call_id);
        let err_res = BridgeToolResult {
            content: "No active hrn turn is listening for bridged tool calls".to_owned(),
            is_error: true,
        };
        return json_response(StatusCode::OK, &tool_call_rpc_response(&id, &err_res));
    }

    if let Some(token) = progress_token {
        return stream_tool_call_with_progress(id, call_id, token, rx, state);
    }

    let result = await_pending_result(&call_id, rx, &state).await;
    json_response(StatusCode::OK, &tool_call_rpc_response(&id, &result))
}

fn stream_tool_call_with_progress(
    id: Value,
    call_id: String,
    progress_token: Value,
    mut rx: oneshot::Receiver<BridgeToolResult>,
    state: Arc<BridgeState>,
) -> Response<BoxBody> {
    let (tx, stream_rx) = mpsc::channel::<Bytes>(16);
    let progress_interval = state.options.progress_interval;
    let call_timeout = state.options.call_timeout;

    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + call_timeout;
        let mut ticker = tokio::time::interval(progress_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let _ = ticker.tick().await;
        let mut step: u64 = 0;

        let final_res = loop {
            tokio::select! {
                res = &mut rx => {
                    break res.unwrap_or_else(|_| BridgeToolResult {
                        content: "Cancelled by user".to_owned(),
                        is_error: true,
                    });
                }
                () = tokio::time::sleep_until(deadline) => {
                    state
                        .pending_calls
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&call_id);
                    break BridgeToolResult {
                        content: format!(
                            "Bridged tool call `{call_id}` timed out after {}s",
                            call_timeout.as_secs()
                        ),
                        is_error: true,
                    };
                }
                _ = ticker.tick() => {
                    step = step.saturating_add(1);
                    let notif = json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/progress",
                        "params": {
                            "progressToken": progress_token,
                            "progress": step,
                            "message": "Executing hrn tool..."
                        }
                    });
                    if tx.send(format_sse_event(&notif)).await.is_err() {
                        break BridgeToolResult {
                            content: "SSE client disconnected".to_owned(),
                            is_error: true,
                        };
                    }
                }
            }
        };

        let final_msg = tool_call_rpc_response(&id, &final_res);
        let _ = tx.send(format_sse_event(&final_msg)).await;
    });

    let body = stream_body::StreamBodyAdapter(ChannelStream { rx: stream_rx });
    let mut resp = Response::new(body.boxed());
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

async fn await_pending_result(
    call_id: &str,
    rx: oneshot::Receiver<BridgeToolResult>,
    state: &BridgeState,
) -> BridgeToolResult {
    let timeout = state.options.call_timeout;
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(res)) => res,
        Ok(Err(_)) => BridgeToolResult {
            content: "Cancelled by user".to_owned(),
            is_error: true,
        },
        Err(_) => {
            state
                .pending_calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(call_id);
            BridgeToolResult {
                content: format!(
                    "Bridged tool call `{call_id}` timed out after {}s",
                    timeout.as_secs()
                ),
                is_error: true,
            }
        }
    }
}

struct ChannelStream {
    rx: mpsc::Receiver<Bytes>,
}

impl stream_body::Stream for ChannelStream {
    type Item = std::result::Result<Frame<Bytes>, Infallible>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx
            .poll_recv(cx)
            .map(|opt| opt.map(|b| Ok(Frame::data(b))))
    }
}

mod stream_body {
    use http_body::{Body, Frame};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub(super) trait Stream {
        type Item;
        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>>;
    }

    pub(super) struct StreamBodyAdapter<S>(pub(super) S);

    impl<S, D, E> Body for StreamBodyAdapter<S>
    where
        S: Stream<Item = Result<Frame<D>, E>> + Unpin,
        D: bytes::Buf,
    {
        type Data = D;
        type Error = E;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            Pin::new(&mut self.0).poll_next(cx)
        }
    }
}

fn format_sse_event(value: &Value) -> Bytes {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned());
    Bytes::from(format!("event: message\ndata: {json}\n\n"))
}

fn tool_call_rpc_response(id: &Value, res: &BridgeToolResult) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [
                {
                    "type": "text",
                    "text": res.content
                }
            ],
            "isError": res.is_error
        }
    })
}

fn jsonrpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn json_response(status: StatusCode, payload: &Value) -> Response<BoxBody> {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    let mut resp = Response::new(Full::new(Bytes::from(body)).boxed());
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

fn empty_response(status: StatusCode) -> Response<BoxBody> {
    let mut resp = Response::new(Full::new(Bytes::new()).boxed());
    *resp.status_mut() = status;
    resp
}

async fn read_bounded_body(
    mut body: Incoming,
    limit: usize,
) -> std::result::Result<Vec<u8>, StatusCode> {
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
        if let Ok(data) = frame.into_data() {
            if out.len().saturating_add(data.len()) > limit {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(out)
}
