//! Bounded HTTP/1.1 request parser and JSON response writer for the per-kernel loopback bridge.
//!
//! Enforces strict size and time bounds BEFORE allocating request bodies, and checks the 256-bit
//! bearer token in constant time via [`hrn_edge_ipc::ct_eq`] before reading any body bytes.

use hrn_edge_ipc::ct_eq;
use hrn_edge_rt::tokio;
use hrn_kernel::serde_json::{self, Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Maximum accepted HTTP request line length (bytes).
const MAX_REQUEST_LINE_BYTES: usize = 4 * 1024;
/// Maximum accepted HTTP headers per request.
const MAX_HEADERS: usize = 64;
/// Maximum cumulative HTTP header bytes per request.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Hard HTTP request body cap (4 MiB).
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Timeout for reading request line + headers.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout for reading the request body.
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Parsed and authenticated HTTP POST request.
pub(crate) struct ParsedRequest {
    pub(crate) path: String,
    pub(crate) body: Value,
}

/// JSON HTTP/1.1 response sent back over the loopback bridge socket.
pub(crate) struct HttpResponse {
    status: u16,
    reason: &'static str,
    body: String,
}

impl HttpResponse {
    pub(crate) fn json(status: u16, reason: &'static str, val: &Value) -> Self {
        Self {
            status,
            reason,
            body: val.to_string(),
        }
    }

    pub(crate) fn ok(val: &Value) -> Self {
        Self::json(200, "OK", val)
    }

    pub(crate) fn err(status: u16, reason: &'static str, message: &str) -> Self {
        Self::json(status, reason, &json!({ "ok": false, "error": message }))
    }
}

/// Write `response` with `Connection: close` and shut down `stream`.
pub(crate) async fn write_response(stream: &mut TcpStream, response: &HttpResponse) {
    let wire = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        response.reason,
        response.body.len(),
        response.body
    );
    let _ = stream.write_all(wire.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// Read, authenticate, and parse one bounded HTTP/1.1 JSON POST request.
pub(crate) async fn read_request(
    stream: &mut TcpStream,
    expected_token: &str,
) -> std::result::Result<ParsedRequest, HttpResponse> {
    let mut reader = BufReader::new(stream);
    let (method, path) = tokio::time::timeout(HEADER_TIMEOUT, read_request_line(&mut reader))
        .await
        .map_err(|_| HttpResponse::err(408, "Request Timeout", "header read timed out"))??;
    if method != "POST" {
        return Err(HttpResponse::err(
            405,
            "Method Not Allowed",
            "only POST is supported",
        ));
    }
    let headers = tokio::time::timeout(HEADER_TIMEOUT, read_headers(&mut reader))
        .await
        .map_err(|_| HttpResponse::err(408, "Request Timeout", "header read timed out"))??;

    // Constant-time bearer authentication on EVERY request before allocating/reading body.
    let provided = headers
        .authorization
        .as_deref()
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !ct_eq(provided.as_bytes(), expected_token.as_bytes()) {
        return Err(HttpResponse::err(401, "Unauthorized", "unauthorized"));
    }

    let content_len = headers.content_length.unwrap_or(0);
    if content_len > MAX_BODY_BYTES {
        return Err(HttpResponse::err(
            413,
            "Payload Too Large",
            "request body exceeds 4 MiB limit",
        ));
    }
    let mut buf = vec![0u8; content_len];
    if content_len > 0 {
        tokio::time::timeout(BODY_TIMEOUT, reader.read_exact(&mut buf))
            .await
            .map_err(|_| HttpResponse::err(408, "Request Timeout", "body read timed out"))?
            .map_err(|_| HttpResponse::err(400, "Bad Request", "incomplete request body"))?;
    }
    let body = if buf.is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_slice(&buf)
            .map_err(|_| HttpResponse::err(400, "Bad Request", "invalid JSON request body"))?
    };
    Ok(ParsedRequest { path, body })
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::result::Result<String, HttpResponse> {
    let mut raw = Vec::new();
    let n = reader
        .take((max_bytes + 1) as u64)
        .read_until(b'\n', &mut raw)
        .await
        .map_err(|_| HttpResponse::err(400, "Bad Request", "socket read error"))?;
    if n == 0 {
        return Err(HttpResponse::err(400, "Bad Request", "empty request"));
    }
    if raw.len() > max_bytes || !raw.ends_with(b"\n") {
        return Err(HttpResponse::err(
            431,
            "Request Header Fields Too Large",
            "HTTP header line exceeded size limit",
        ));
    }
    let text = String::from_utf8(raw)
        .map_err(|_| HttpResponse::err(400, "Bad Request", "non-UTF8 header line"))?;
    Ok(text.trim_end_matches(['\r', '\n']).to_owned())
}

async fn read_request_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::result::Result<(String, String), HttpResponse> {
    let line = read_bounded_line(reader, MAX_REQUEST_LINE_BYTES).await?;
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if method.is_empty()
        || path.is_empty()
        || !version.starts_with("HTTP/1.")
        || parts.next().is_some()
    {
        return Err(HttpResponse::err(
            400,
            "Bad Request",
            "malformed HTTP request line",
        ));
    }
    Ok((method.to_owned(), path.to_owned()))
}

struct ParsedHeaders {
    authorization: Option<String>,
    content_length: Option<usize>,
}

async fn read_headers<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::result::Result<ParsedHeaders, HttpResponse> {
    let mut authorization = None;
    let mut content_length = None;
    let mut total_bytes = 0usize;
    let mut count = 0usize;
    loop {
        let remaining = MAX_HEADER_BYTES.saturating_sub(total_bytes);
        if remaining == 0 {
            return Err(HttpResponse::err(
                431,
                "Request Header Fields Too Large",
                "total headers exceeded limit",
            ));
        }
        let line = read_bounded_line(reader, remaining.min(MAX_REQUEST_LINE_BYTES)).await?;
        total_bytes = total_bytes.saturating_add(line.len() + 2);
        if line.is_empty() {
            break;
        }
        count += 1;
        if count > MAX_HEADERS {
            return Err(HttpResponse::err(
                431,
                "Request Header Fields Too Large",
                "too many headers",
            ));
        }
        let Some((name, val)) = line.split_once(':') else {
            return Err(HttpResponse::err(
                400,
                "Bad Request",
                "malformed HTTP header",
            ));
        };
        let key = name.trim();
        let value = val.trim();
        if key.eq_ignore_ascii_case("transfer-encoding") {
            return Err(HttpResponse::err(
                400,
                "Bad Request",
                "chunked transfer encoding is not supported",
            ));
        }
        if key.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_owned());
        } else if key.eq_ignore_ascii_case("content-length") {
            let parsed = value.parse::<usize>().map_err(|_| {
                HttpResponse::err(400, "Bad Request", "invalid Content-Length header")
            })?;
            if content_length.replace(parsed).is_some() {
                return Err(HttpResponse::err(
                    400,
                    "Bad Request",
                    "duplicate Content-Length header",
                ));
            }
        }
    }
    Ok(ParsedHeaders {
        authorization,
        content_length,
    })
}
