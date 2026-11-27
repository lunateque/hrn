//! Streamable HTTP MCP transport (MCP 2025-03-26 specification) over `hrn-edge-http`.

use crate::protocol::{
    InitializeResult, JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    PROTOCOL_VERSION, initialize_params, sanitize_mcp_error,
};
use bytes::Bytes;
use hrn_edge_http::{HttpClient, SseParser};
use hrn_kernel::{CancelFlag, HrnError, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;

/// MCP Streamable HTTP client handling both JSON and SSE responses plus `Mcp-Session-Id`.
pub(crate) struct HttpMcpTransport {
    server_name: String,
    url: String,
    headers: BTreeMap<String, String>,
    header_envs: BTreeMap<String, String>,
    timeout: Duration,
    http: HttpClient,
    next_id: AtomicU64,
    session_id: RwLock<Option<String>>,
}

impl HttpMcpTransport {
    pub(crate) async fn connect(
        server_name: String,
        url: String,
        headers: BTreeMap<String, String>,
        header_envs: BTreeMap<String, String>,
        timeout: Duration,
        http: HttpClient,
    ) -> Result<(Self, InitializeResult)> {
        let transport = Self {
            server_name,
            url,
            headers,
            header_envs,
            timeout,
            http,
            next_id: AtomicU64::new(1),
            session_id: RwLock::new(None),
        };
        let init = transport.initialize().await?;
        Ok((transport, init))
    }

    async fn initialize(&self) -> Result<InitializeResult> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(id, "initialize", Some(initialize_params()));
        let (val, new_session) = self.send_rpc(&req, &CancelFlag::new()).await?;
        if let Some(sid) = new_session {
            *self.session_id.write().await = Some(sid);
        }
        let init: InitializeResult = serde_json::from_value(val).map_err(|e| {
            HrnError::Tool(format!(
                "MCP `{}`: invalid initialize response: {e}",
                self.server_name
            ))
        })?;
        // Send notifications/initialized; ignore 202/204 empty responses cleanly.
        let notif = JsonRpcNotification::new("notifications/initialized", None);
        let _ = self.post_notification(&notif).await;
        Ok(init)
    }

    pub(crate) async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        cancel: &CancelFlag,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(id, method, params.clone());
        match self.send_rpc(&req, cancel).await {
            Ok((val, sid)) => {
                if let Some(sid) = sid {
                    *self.session_id.write().await = Some(sid);
                }
                Ok(val)
            }
            Err(HrnError::Tool(msg)) if msg.contains("HTTP 404") && !cancel.is_cancelled() => {
                // Session expired on remote server: re-initialize once and retry.
                *self.session_id.write().await = None;
                let _ = self.initialize().await?;
                let retry_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                let retry_req = JsonRpcRequest::new(retry_id, method, params);
                let (val, sid) = self.send_rpc(&retry_req, cancel).await?;
                if let Some(sid) = sid {
                    *self.session_id.write().await = Some(sid);
                }
                Ok(val)
            }
            Err(err) => Err(err),
        }
    }

    async fn build_headers(&self) -> Result<Vec<(String, String)>> {
        let mut out = vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
            (
                "mcp-protocol-version".to_owned(),
                PROTOCOL_VERSION.to_owned(),
            ),
        ];
        if let Some(sid) = self.session_id.read().await.as_ref() {
            out.push(("mcp-session-id".to_owned(), sid.clone()));
        }
        for (k, v) in &self.headers {
            out.push((k.clone(), v.clone()));
        }
        for (k, env_var) in &self.header_envs {
            let val = std::env::var(env_var).map_err(|_| {
                HrnError::Config(format!(
                    "MCP `{}`: header `{k}` requires env var `{env_var}`",
                    self.server_name
                ))
            })?;
            out.push((k.clone(), val));
        }
        Ok(out)
    }

    async fn post_notification(&self, notif: &JsonRpcNotification) -> Result {
        let body = serde_json::to_vec(notif).map_err(|e| {
            HrnError::Internal(format!(
                "MCP `{}`: encode notification: {e}",
                self.server_name
            ))
        })?;
        let headers = self.build_headers().await?;
        let _ = self
            .http
            .post_raw(&self.url, &headers, Bytes::from(body), self.timeout)
            .await?;
        Ok(())
    }

    async fn send_rpc(
        &self,
        req: &JsonRpcRequest,
        cancel: &CancelFlag,
    ) -> Result<(Value, Option<String>)> {
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        let body = serde_json::to_vec(req).map_err(|e| {
            HrnError::Internal(format!("MCP `{}`: encode JSON-RPC: {e}", self.server_name))
        })?;
        let headers = self.build_headers().await?;
        let resp = cancel
            .race(
                self.http
                    .post_raw(&self.url, &headers, Bytes::from(body), self.timeout),
            )
            .await?;
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        if !resp.status.is_success() {
            let secrets: Vec<String> = headers.iter().map(|(_, v)| v.clone()).collect();
            let body_msg = sanitize_mcp_error(resp.text().trim(), &secrets);
            return Err(HrnError::Tool(format!(
                "MCP `{}`: HTTP {}: {body_msg}",
                self.server_name,
                resp.status.as_u16()
            )));
        }
        let sid = resp
            .headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let content_type = resp
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        let rpc_resp = if content_type.contains("text/event-stream") {
            extract_sse_response(&resp.body, &req.id, &self.server_name)?
        } else {
            serde_json::from_slice::<JsonRpcResponse>(&resp.body).map_err(|e| {
                HrnError::Tool(format!(
                    "MCP `{}`: invalid JSON-RPC HTTP response: {e}",
                    self.server_name
                ))
            })?
        };
        if let Some(err) = rpc_resp.error {
            let secrets: Vec<String> = headers.iter().map(|(_, v)| v.clone()).collect();
            let safe_err = sanitize_mcp_error(&err.to_string(), &secrets);
            return Err(HrnError::Tool(format!(
                "MCP `{}`: {safe_err}",
                self.server_name
            )));
        }
        Ok((rpc_resp.result.unwrap_or(Value::Null), sid))
    }
}

fn extract_sse_response(
    body: &[u8],
    expected_id: &JsonRpcId,
    server_name: &str,
) -> Result<JsonRpcResponse> {
    let mut parser = SseParser::default();
    let mut events = parser.push(body);
    events.extend(parser.finish());
    let mut last_valid: Option<JsonRpcResponse> = None;
    for ev in events {
        let trimmed = ev.data.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
            if parsed.id.as_ref() == Some(expected_id) {
                return Ok(parsed);
            }
            last_valid = Some(parsed);
        }
    }
    last_valid.ok_or_else(|| {
        HrnError::Tool(format!(
            "MCP `{server_name}`: SSE stream contained no JSON-RPC response"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_matching_jsonrpc_response_from_sse() {
        let body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"tools\":[]}}\n\n"
        );
        let resp =
            extract_sse_response(body.as_bytes(), &JsonRpcId::Number(7), "srv").expect("extract");
        assert_eq!(resp.id, Some(JsonRpcId::Number(7)));
        assert!(resp.result.is_some());
    }
}
