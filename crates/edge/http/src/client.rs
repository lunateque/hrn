//! Lightweight `hyper` + `rustls` (OS native cert store) HTTP/HTTPS client with SSE streaming.

use crate::guard;
use crate::sse::{SseEvent, SseParser};
use bytes::Bytes;
use hrn_kernel::{HrnError, Result};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use rustls_platform_verifier::BuilderVerifierExt;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// Longest single TCP connect attempt; the next resolved address is tried afterwards so a
/// black-holed IPv6 route does not stall the whole connect phase.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(8);
/// Largest error-body excerpt embedded in an error message.
const ERROR_EXCERPT_CHARS: usize = 2000;
/// Largest proxy `CONNECT` reply head accepted.
const MAX_PROXY_HEAD: usize = 8 * 1024;

fn transport_err(message: String) -> HrnError {
    HrnError::Provider {
        message,
        retryable: true,
        retry_after_secs: None,
    }
}

fn excerpt(text: &str) -> String {
    match text.char_indices().nth(ERROR_EXCERPT_CHARS) {
        Some((cut, _)) => format!("{}…", text.get(..cut).unwrap_or_default()),
        None => text.to_owned(),
    }
}

/// Buffered non-streaming HTTP response.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Raw body bytes (at most the caller's `max_bytes`).
    pub body: Bytes,
    /// `true` when the server sent more bytes than `max_bytes` and the rest was discarded.
    pub truncated: bool,
}

impl HttpResponse {
    /// Decode body as lossy UTF-8 string.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Parse `Retry-After` header in seconds when present.
    pub fn retry_after_secs(&self) -> Option<u64> {
        self.headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
    }

    /// The error a non-2xx reply maps to: 401/403 are [`HrnError::Auth`]; 408, 429, 5xx
    /// gateway errors and 529 are retryable with the `Retry-After` hint; the message carries a
    /// body excerpt.
    pub fn status_error(&self) -> HrnError {
        let code = self.status.as_u16();
        let message = format!("HTTP {code}: {}", excerpt(&self.text()));
        if matches!(code, 401 | 403) {
            return HrnError::Auth(message);
        }
        HrnError::Provider {
            message,
            retryable: matches!(code, 408 | 429 | 500 | 502 | 503 | 504 | 529),
            retry_after_secs: self.retry_after_secs(),
        }
    }
}

/// Outcome of [`HttpClient::post_stream`].
pub enum StreamReply {
    /// 2xx: the event stream.
    Stream(SseStream),
    /// Non-2xx: the buffered reply (at most 64 KiB) for callers that recover from specific
    /// failures before mapping it with [`HttpResponse::status_error`].
    Failed(HttpResponse),
}

/// Active Server-Sent Events response stream.
pub struct SseStream {
    body: Incoming,
    parser: SseParser,
    pending: VecDeque<SseEvent>,
    eof: bool,
    idle: Duration,
}

impl SseStream {
    /// Read the next decoded SSE event, or `Ok(None)` at clean EOF.
    ///
    /// Fails with a retryable error when no body bytes arrive for the idle timeout that was
    /// given to [`HttpClient::post_sse`] (a stalled connection must not hang a turn forever).
    pub async fn next_event(&mut self) -> Result<Option<SseEvent>> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Ok(Some(ev));
            }
            if self.eof {
                return Ok(None);
            }
            let frame = tokio::time::timeout(self.idle, self.body.frame())
                .await
                .map_err(|_| {
                    transport_err(format!(
                        "SSE stream idle for {}s without data",
                        self.idle.as_secs()
                    ))
                })?;
            match frame {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        self.pending.extend(self.parser.push(data));
                    }
                }
                Some(Err(err)) => {
                    return Err(transport_err(format!("SSE stream read error: {err}")));
                }
                None => {
                    self.eof = true;
                    self.pending.extend(self.parser.finish());
                }
            }
        }
    }
}

/// Shared outbound HTTP/HTTPS client backed by `hyper` + `rustls-platform-verifier`.
#[derive(Clone)]
pub struct HttpClient {
    tls: Arc<ClientConfig>,
    connect_timeout: Duration,
}

impl HttpClient {
    /// Create a new client using the OS native root certificate store.
    pub fn new() -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| HrnError::Internal(format!("tls protocol config: {e}")))?
            .with_platform_verifier()
            .map_err(|e| HrnError::Internal(format!("platform cert verifier: {e}")))?
            .with_no_client_auth();
        Ok(Self {
            tls: Arc::new(tls),
            connect_timeout: Duration::from_secs(25),
        })
    }

    /// Perform an HTTP `GET` and buffer the response up to `max_bytes` (see
    /// [`HttpResponse::truncated`]).
    pub async fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<HttpResponse> {
        let resp = self
            .send_raw(Method::GET, url, headers, Bytes::new(), timeout, false)
            .await?;
        collect_response(resp, max_bytes, timeout).await
    }

    /// Perform a public `web_fetch` `GET` enforcing SSRF checks on the target URL and resolved IP.
    /// Redirects are never followed here; callers re-validate each hop through this method.
    pub async fn get_public(
        &self,
        url: &str,
        headers: &[(String, String)],
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<HttpResponse> {
        guard::check_public_fetch_url(url)?;
        let resp = self
            .send_raw(Method::GET, url, headers, Bytes::new(), timeout, true)
            .await?;
        collect_response(resp, max_bytes, timeout).await
    }

    /// Perform an HTTP `POST` with raw body and buffer the response (at most 4 MiB; a larger
    /// response is an error rather than a silently cut body).
    pub async fn post_raw(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: Bytes,
        timeout: Duration,
    ) -> Result<HttpResponse> {
        const LIMIT: usize = 4 * 1024 * 1024;
        let resp = self
            .send_raw(Method::POST, url, headers, body, timeout, false)
            .await?;
        let buffered = collect_response(resp, LIMIT, timeout).await?;
        if buffered.truncated {
            return Err(HrnError::Provider {
                message: format!("response body exceeds {LIMIT} bytes"),
                retryable: false,
                retry_after_secs: None,
            });
        }
        Ok(buffered)
    }

    /// Perform an HTTP `POST` and return a streaming [`SseStream`]; a non-2xx reply becomes
    /// [`HttpResponse::status_error`].
    ///
    /// `timeout` bounds the time to response headers and, afterwards, every gap between body
    /// frames (an idle stream fails instead of hanging).
    pub async fn post_sse(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: Bytes,
        timeout: Duration,
    ) -> Result<SseStream> {
        match self.post_stream(url, headers, body, timeout).await? {
            StreamReply::Stream(stream) => Ok(stream),
            StreamReply::Failed(reply) => Err(reply.status_error()),
        }
    }

    /// Like [`Self::post_sse`], but a non-2xx reply comes back buffered instead of as an error,
    /// so the caller can recover (refresh a rejected token, adopt a required client version).
    pub async fn post_stream(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: Bytes,
        timeout: Duration,
    ) -> Result<StreamReply> {
        let resp = self
            .send_raw(Method::POST, url, headers, body, timeout, false)
            .await?;
        if !resp.status().is_success() {
            let buffered = collect_response(resp, 64 * 1024, Duration::from_secs(10)).await?;
            return Ok(StreamReply::Failed(buffered));
        }
        Ok(StreamReply::Stream(SseStream {
            body: resp.into_body(),
            parser: SseParser::default(),
            pending: VecDeque::new(),
            eof: false,
            idle: timeout,
        }))
    }

    async fn send_raw(
        &self,
        method: Method,
        url: &str,
        headers: &[(String, String)],
        body: Bytes,
        timeout: Duration,
        enforce_public_ip: bool,
    ) -> Result<http::Response<Incoming>> {
        guard::check_outbound_url(url)?;
        let uri: Uri = url
            .parse()
            .map_err(|e| HrnError::Config(format!("invalid URL `{url}`: {e}")))?;
        let scheme = match uri.scheme_str() {
            Some(s @ ("http" | "https")) => s,
            other => {
                return Err(HrnError::Config(format!(
                    "unsupported URL scheme {other:?} in `{url}`"
                )));
            }
        };
        let is_https = scheme == "https";
        let host = uri
            .host()
            .ok_or_else(|| HrnError::Config(format!("URL missing host: `{url}`")))?
            .to_owned();
        // `Uri::host` keeps the brackets of an IPv6 literal; DNS/TLS/connect want the bare form.
        let bare_host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        guard::check_outbound_host(&bare_host)?;
        let default_port = if is_https { 443 } else { 80 };
        let port = uri.port_u16().unwrap_or(default_port);
        let host_header = if port == default_port {
            host.clone()
        } else {
            format!("{host}:{port}")
        };
        let path_and_query = uri
            .path_and_query()
            .map_or("/", http::uri::PathAndQuery::as_str);

        let mut builder = Request::builder()
            .method(method)
            .uri(path_and_query)
            .header("host", host_header);
        let mut has_user_agent = false;
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| HrnError::Config(format!("invalid HTTP header name `{k}`")))?;
            // The value may be a credential: never echo it.
            let value = HeaderValue::from_str(v)
                .map_err(|_| HrnError::Config(format!("invalid value for HTTP header `{k}`")))?;
            has_user_agent |= name == http::header::USER_AGENT;
            builder = builder.header(name, value);
        }
        if !has_user_agent {
            builder = builder.header("user-agent", "hrn/0.1");
        }
        let req = builder
            .body(Full::new(body))
            .map_err(|e| HrnError::Internal(format!("build http request: {e}")))?;

        let stream = self
            .connect_tcp(&bare_host, port, is_https, enforce_public_ip)
            .await?;

        if is_https {
            let server_name = ServerName::try_from(bare_host.clone())
                .map_err(|_| HrnError::Config(format!("invalid TLS host `{bare_host}`")))?;
            let connector = TlsConnector::from(Arc::clone(&self.tls));
            let tls_stream =
                tokio::time::timeout(self.connect_timeout, connector.connect(server_name, stream))
                    .await
                    .map_err(|_| transport_err(format!("TLS handshake timeout for {bare_host}")))?
                    .map_err(|e| {
                        transport_err(format!("TLS connect failed for {bare_host}: {e}"))
                    })?;
            dispatch_http1(TokioIo::new(tls_stream), req, timeout).await
        } else {
            dispatch_http1(TokioIo::new(stream), req, timeout).await
        }
    }

    async fn connect_tcp(
        &self,
        host: &str,
        port: u16,
        is_https: bool,
        enforce_public_ip: bool,
    ) -> Result<TcpStream> {
        if is_https
            && !enforce_public_ip
            && let Some(proxy_url) = env_https_proxy()
        {
            return connect_via_proxy(&proxy_url, host, port, self.connect_timeout).await;
        }
        let addrs: Vec<SocketAddr> =
            tokio::time::timeout(self.connect_timeout, tokio::net::lookup_host((host, port)))
                .await
                .map_err(|_| transport_err(format!("DNS lookup timeout for {host}")))?
                .map_err(|e| transport_err(format!("DNS lookup failed for {host}: {e}")))?
                .filter(|a| !enforce_public_ip || !guard::is_private_ip(a.ip()))
                .collect();
        if addrs.is_empty() {
            return Err(if enforce_public_ip {
                HrnError::Tool(format!(
                    "DNS for `{host}` resolved only to private/blocked IPs"
                ))
            } else {
                transport_err(format!("DNS for `{host}` returned no addresses"))
            });
        }
        let mut last_err = String::new();
        for addr in addrs {
            match tokio::time::timeout(ATTEMPT_TIMEOUT, TcpStream::connect(addr)).await {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(e)) => last_err = format!("{addr}: {e}"),
                Err(_) => last_err = format!("{addr}: timed out"),
            }
        }
        Err(transport_err(format!(
            "TCP connect failed for {host}:{port} ({last_err})"
        )))
    }
}

async fn dispatch_http1<IO>(
    io: TokioIo<IO>,
    req: Request<Full<Bytes>>,
    timeout: Duration,
) -> Result<http::Response<Incoming>>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = http1::handshake(io)
        .await
        .map_err(|e| transport_err(format!("HTTP/1 handshake failed: {e}")))?;
    // The connection driver's own failure is reported through `send_request`/the body below, so
    // its result carries no extra information.
    tokio::spawn(async move {
        let _ = conn.await;
    });
    tokio::time::timeout(timeout, sender.send_request(req))
        .await
        .map_err(|_| transport_err("HTTP request timed out".to_owned()))?
        .map_err(|e| transport_err(format!("HTTP request failed: {e}")))
}

async fn collect_response(
    resp: http::Response<Incoming>,
    max_bytes: usize,
    timeout: Duration,
) -> Result<HttpResponse> {
    let status = resp.status();
    let headers = resp.headers().clone();
    let mut body = resp.into_body();
    let mut buf = Vec::new();
    let mut truncated = false;
    let fut = async {
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| transport_err(format!("read response body: {e}")))?;
            if let Some(chunk) = frame.data_ref() {
                let remaining = max_bytes.saturating_sub(buf.len());
                if chunk.len() > remaining {
                    buf.extend_from_slice(chunk.get(..remaining).unwrap_or_default());
                    truncated = true;
                    break;
                }
                buf.extend_from_slice(chunk);
            }
        }
        Ok::<(), HrnError>(())
    };
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| transport_err("response body read timed out".to_owned()))??;
    Ok(HttpResponse {
        status,
        headers,
        body: Bytes::from(buf),
        truncated,
    })
}

fn env_https_proxy() -> Option<String> {
    std::env::var("HTTPS_PROXY")
        .or_else(|_| std::env::var("https_proxy"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Open a tunnel through an HTTP proxy with `CONNECT`.
///
/// Limitations (documented, not silent): proxy credentials in the URL and `NO_PROXY` are not
/// supported; a proxy demanding authentication fails with its `407` status line.
async fn connect_via_proxy(
    proxy_url: &str,
    target_host: &str,
    target_port: u16,
    timeout: Duration,
) -> Result<TcpStream> {
    let uri: Uri = proxy_url
        .parse()
        .map_err(|e| HrnError::Config(format!("invalid HTTPS_PROXY (unparsable URL): {e}")))?;
    let proxy_host = uri
        .host()
        .ok_or_else(|| HrnError::Config("HTTPS_PROXY missing host".to_owned()))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let proxy_port = uri.port_u16().unwrap_or(8080);
    let authority = if target_host.contains(':') {
        format!("[{target_host}]:{target_port}")
    } else {
        format!("{target_host}:{target_port}")
    };
    let tunnel = async {
        let mut stream = TcpStream::connect((proxy_host.as_str(), proxy_port))
            .await
            .map_err(|e| transport_err(format!("proxy connect failed: {e}")))?;
        let req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| transport_err(format!("proxy CONNECT write failed: {e}")))?;
        // Read byte-wise up to the blank line so no tunnelled (TLS) byte is ever consumed.
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() >= MAX_PROXY_HEAD {
                return Err(transport_err(
                    "proxy CONNECT reply head too large".to_owned(),
                ));
            }
            let mut byte = [0u8; 1];
            let n = stream
                .read(&mut byte)
                .await
                .map_err(|e| transport_err(format!("proxy CONNECT read failed: {e}")))?;
            if n == 0 {
                return Err(transport_err(
                    "proxy closed the connection during CONNECT".to_owned(),
                ));
            }
            head.extend_from_slice(&byte);
        }
        let head = String::from_utf8_lossy(&head);
        let status_line = head.lines().next().unwrap_or_default();
        let code: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        if code != 200 {
            return Err(HrnError::Provider {
                message: format!("proxy CONNECT rejected: {status_line}"),
                retryable: matches!(code, 502..=504),
                retry_after_secs: None,
            });
        }
        Ok(stream)
    };
    tokio::time::timeout(timeout, tunnel)
        .await
        .map_err(|_| transport_err("proxy CONNECT timed out".to_owned()))?
}
