//! Loopback HTTP/1.1 + SSE client for communicating with a local or SSH-tunneled `hrn daemon`.

use crate::auth::{RunFile, ct_eq, read_run_file};
use crate::protocol::{
    ApproveRequest, CreateSessionRequest, DaemonEvent, ErrorResponse, HealthResponse, OkResponse,
    RestoreSessionRequest, RestoreSessionResponse, SessionSummaryDto, StartTurnRequest,
    SteerRequest,
};
use bytes::Bytes;
use hrn_edge_http::SseParser;
use hrn_kernel::{HrnError, Result};
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;
use tokio::net::TcpStream;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BODY: usize = 4 * 1024 * 1024;

/// Streaming SSE subscriber yielding [`DaemonEvent`]s from `GET /v1/sessions/{id}/events`.
pub struct DaemonEventStream {
    body: Incoming,
    parser: SseParser,
    pending: VecDeque<DaemonEvent>,
    eof: bool,
}

impl DaemonEventStream {
    /// Receive the next [`DaemonEvent`], or `Ok(None)` when the stream closes cleanly.
    pub async fn next_event(&mut self) -> Result<Option<DaemonEvent>> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Ok(Some(ev));
            }
            if self.eof {
                return Ok(None);
            }
            match self.body.frame().await {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        for sse in self.parser.push(data) {
                            if !sse.data.trim().is_empty()
                                && let Ok(ev) = serde_json::from_str::<DaemonEvent>(&sse.data)
                            {
                                self.pending.push_back(ev);
                            }
                        }
                    }
                }
                Some(Err(err)) => {
                    return Err(HrnError::Io(format!("daemon SSE stream error: {err}")));
                }
                None => {
                    self.eof = true;
                    for sse in self.parser.finish() {
                        if !sse.data.trim().is_empty()
                            && let Ok(ev) = serde_json::from_str::<DaemonEvent>(&sse.data)
                        {
                            self.pending.push_back(ev);
                        }
                    }
                }
            }
        }
    }
}

/// Authenticated loopback client for a local or SSH-tunneled `hrn daemon`.
#[derive(Clone, Debug)]
pub struct IpcClient {
    addr: SocketAddr,
    token: String,
    expected_host_key: Option<String>,
}

impl IpcClient {
    /// Create a client targeting `addr` (must be loopback) with `token` and optional pinned
    /// daemon `expected_host_key`.
    pub fn new(addr: SocketAddr, token: String, expected_host_key: Option<String>) -> Result<Self> {
        if !addr.ip().is_loopback() {
            return Err(HrnError::Auth(format!(
                "IPC client refuses non-loopback address `{addr}`"
            )));
        }
        Ok(Self {
            addr,
            token,
            expected_host_key,
        })
    }

    /// Construct a client from a local `run.json` file.
    pub fn from_run_file(path: &Path) -> Result<Self> {
        let rf = read_run_file(path)?;
        Self::from_run_file_struct(&rf)
    }

    /// Construct a client from an already-loaded [`RunFile`].
    pub fn from_run_file_struct(rf: &RunFile) -> Result<Self> {
        let addr = rf.loopback_addr()?;
        Self::new(addr, rf.token.clone(), Some(rf.host_key.clone()))
    }

    /// Query `GET /v1/health` and verify the pinned `host_key` if configured.
    pub async fn health(&self) -> Result<HealthResponse> {
        let resp: HealthResponse = self
            .request_json(Method::GET, "/v1/health", Option::<&()>::None)
            .await?;
        if let Some(expected) = &self.expected_host_key
            && !ct_eq(resp.host_key.as_bytes(), expected.as_bytes())
        {
            return Err(HrnError::Auth(format!(
                "daemon host-key mismatch: expected `{expected}`, remote presented `{}`",
                resp.host_key
            )));
        }
        Ok(resp)
    }

    /// List all sessions known to the daemon.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummaryDto>> {
        self.request_json(Method::GET, "/v1/sessions", Option::<&()>::None)
            .await
    }

    /// Create a new session on the daemon.
    pub async fn create_session(&self, req: &CreateSessionRequest) -> Result<SessionSummaryDto> {
        self.request_json(Method::POST, "/v1/sessions", Some(req))
            .await
    }

    /// Start a turn on `session_id`.
    pub async fn start_turn(&self, session_id: &str, req: &StartTurnRequest) -> Result<()> {
        let _: OkResponse = self
            .request_json(
                Method::POST,
                &format!("/v1/sessions/{session_id}/turns"),
                Some(req),
            )
            .await?;
        Ok(())
    }

    /// Queue steering guidance for `session_id`.
    pub async fn steer(&self, session_id: &str, text: String) -> Result<()> {
        let _: OkResponse = self
            .request_json(
                Method::POST,
                &format!("/v1/sessions/{session_id}/steer"),
                Some(&SteerRequest { text }),
            )
            .await?;
        Ok(())
    }

    /// Interrupt an active turn in `session_id`.
    pub async fn interrupt(&self, session_id: &str) -> Result<()> {
        let _: OkResponse = self
            .request_json(
                Method::POST,
                &format!("/v1/sessions/{session_id}/interrupt"),
                Option::<&()>::None,
            )
            .await?;
        Ok(())
    }

    /// Answer an approval request for `session_id`.
    pub async fn approve(&self, session_id: &str, call_id: String, approved: bool) -> Result<()> {
        let _: OkResponse = self
            .request_json(
                Method::POST,
                &format!("/v1/sessions/{session_id}/approve"),
                Some(&ApproveRequest { call_id, approved }),
            )
            .await?;
        Ok(())
    }

    /// Continue an interrupted session turn on the daemon (`POST /v1/sessions/{id}/restore`).
    pub async fn restore_session(&self, session_id: &str, auto_approve: bool) -> Result<bool> {
        let resp: RestoreSessionResponse = self
            .request_json(
                Method::POST,
                &format!("/v1/sessions/{session_id}/restore"),
                Some(&RestoreSessionRequest { auto_approve }),
            )
            .await?;
        Ok(resp.restored)
    }

    /// Subscribe to live SSE events for `session_id`.
    pub async fn subscribe_events(&self, session_id: &str) -> Result<DaemonEventStream> {
        let resp = self
            .send_raw(
                Method::GET,
                &format!("/v1/sessions/{session_id}/events"),
                Bytes::new(),
            )
            .await?;
        if !resp.status().is_success() {
            return Err(parse_error_response(resp).await);
        }
        Ok(DaemonEventStream {
            body: resp.into_body(),
            parser: SseParser::default(),
            pending: VecDeque::new(),
            eof: false,
        })
    }

    async fn request_json<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Req>,
    ) -> Result<Resp> {
        let bytes = match body {
            Some(payload) => Bytes::from(serde_json::to_vec(payload)?),
            None => Bytes::new(),
        };
        let resp = self.send_raw(method, path, bytes).await?;
        if !resp.status().is_success() {
            return Err(parse_error_response(resp).await);
        }
        let raw = collect_body(resp.into_body(), MAX_RESPONSE_BODY).await?;
        serde_json::from_slice(&raw)
            .map_err(|e| HrnError::Internal(format!("decode daemon JSON response: {e}")))
    }

    async fn send_raw(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
    ) -> Result<http::Response<Incoming>> {
        let stream = tokio::time::timeout(DEFAULT_TIMEOUT, TcpStream::connect(self.addr))
            .await
            .map_err(|_| HrnError::Io(format!("connect to daemon `{}` timed out", self.addr)))?
            .map_err(|e| HrnError::Io(format!("connect to daemon `{}` failed: {e}", self.addr)))?;
        let (mut sender, conn) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| HrnError::Io(format!("daemon HTTP/1 handshake failed: {e}")))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let host_header = self.addr.to_string();
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, host_header)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token));
        if !body.is_empty() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let req = builder
            .body(Full::new(body))
            .map_err(|e| HrnError::Internal(format!("build IPC request: {e}")))?;
        tokio::time::timeout(DEFAULT_TIMEOUT, sender.send_request(req))
            .await
            .map_err(|_| HrnError::Io("daemon HTTP request timed out".to_owned()))?
            .map_err(|e| HrnError::Io(format!("daemon HTTP request failed: {e}")))
    }
}

async fn collect_body(mut body: Incoming, max_bytes: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| HrnError::Io(format!("read daemon body: {e}")))?;
        if let Some(chunk) = frame.data_ref() {
            if buf.len().saturating_add(chunk.len()) > max_bytes {
                return Err(HrnError::Io("daemon response exceeded size limit".into()));
            }
            buf.extend_from_slice(chunk);
        }
    }
    Ok(buf)
}

async fn parse_error_response(resp: http::Response<Incoming>) -> HrnError {
    let status = resp.status();
    let raw = collect_body(resp.into_body(), 64 * 1024)
        .await
        .unwrap_or_default();
    let message = serde_json::from_slice::<ErrorResponse>(&raw).map_or_else(
        |_| {
            let text = String::from_utf8_lossy(&raw).trim().to_owned();
            if text.is_empty() {
                format!("daemon HTTP {}", status.as_u16())
            } else {
                format!("daemon HTTP {}: {text}", status.as_u16())
            }
        },
        |env| format!("daemon HTTP {}: {}", status.as_u16(), env.error),
    );
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => HrnError::Auth(message),
        StatusCode::NOT_FOUND => HrnError::Store(message),
        _ => HrnError::Io(message),
    }
}
