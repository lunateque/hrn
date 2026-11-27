//! Pure-Rust RFC 6455 WebSocket client (`ws://` and `wss://` with OS native cert store) and
//! loopback CDP HTTP/WebSocket helpers.

use crate::guard;
use crate::seal::ct_eq;
use crate::ws_frame::WsIo;
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use hrn_kernel::{HrnError, Result};
use http::Uri;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use rustls_platform_verifier::BuilderVerifierExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

pub use crate::ws_frame::{WsEvent, WsReader, WsStream, WsWriter};

/// Maximum HTTP upgrade / JSON discovery response head accepted.
const MAX_HTTP_HEAD_BYTES: usize = 64 * 1024;
/// Default connection and handshake timeout for [`connect_ws`].
const DEFAULT_WS_TIMEOUT: Duration = Duration::from_secs(15);
/// RFC 6455 WebSocket handshake GUID.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Parsed WebSocket / HTTP target components.
struct WsTarget {
    is_tls: bool,
    bare_host: String,
    port: u16,
    host_header: String,
    path_and_query: String,
    display_url: String,
}

/// Strip `#fragment` and any `userinfo@` credentials from `url` before placing it in errors/logs.
pub(crate) fn sanitize_url_for_display(url: &str) -> String {
    let no_frag = url.split('#').next().unwrap_or(url).trim();
    let Some((scheme, rest)) = no_frag.split_once("://") else {
        return no_frag.to_owned();
    };
    let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = rest.get(..auth_end).unwrap_or(rest);
    let suffix = rest.get(auth_end..).unwrap_or("");
    let clean_auth = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{scheme}://{clean_auth}{suffix}")
}

fn parse_loopback_url(url: &str, expected_scheme: &str) -> Result<WsTarget> {
    if url.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return Err(HrnError::Tool(
            "invalid control character in URL".to_owned(),
        ));
    }
    let display_url = sanitize_url_for_display(url);
    let prefix = format!("{expected_scheme}://");
    let rest = display_url.strip_prefix(&prefix).ok_or_else(|| {
        HrnError::Tool(format!(
            "expected `{prefix}` loopback URL, got `{display_url}`"
        ))
    })?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (
            rest.get(..idx).unwrap_or(rest),
            rest.get(idx..).unwrap_or("/"),
        ),
        None => (rest, "/"),
    };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (h, tail) = bracketed
            .split_once(']')
            .ok_or_else(|| HrnError::Tool(format!("malformed IPv6 host in `{display_url}`")))?;
        let p = tail
            .strip_prefix(':')
            .ok_or_else(|| HrnError::Tool(format!("missing port in `{display_url}`")))?;
        (h, p)
    } else {
        authority
            .split_once(':')
            .ok_or_else(|| HrnError::Tool(format!("missing port in `{display_url}`")))?
    };
    let port_num: u16 = port
        .parse()
        .map_err(|_| HrnError::Tool(format!("invalid port `{port}` in `{display_url}`")))?;
    let is_loopback =
        host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1";
    if !is_loopback {
        return Err(HrnError::Tool(format!(
            "refusing non-loopback local DevTools host `{host}`"
        )));
    }
    Ok(WsTarget {
        is_tls: false,
        bare_host: host.to_owned(),
        port: port_num,
        host_header: authority.to_owned(),
        path_and_query: path.to_owned(),
        display_url,
    })
}

fn parse_general_ws_url(url: &str) -> Result<WsTarget> {
    if url.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return Err(HrnError::Config(
            "invalid control character in WebSocket URL".to_owned(),
        ));
    }
    // Strip fragment and userinfo defensively so secrets never leak to the wire or error logs.
    let display_url = sanitize_url_for_display(url);
    let (is_tls, http_equiv) = if let Some(rest) = strip_scheme_ci(&display_url, "wss://") {
        (true, format!("https://{rest}"))
    } else if let Some(rest) = strip_scheme_ci(&display_url, "ws://") {
        (false, format!("http://{rest}"))
    } else {
        return Err(HrnError::Config(format!(
            "unsupported WebSocket scheme in `{display_url}` (expected ws:// or wss://)"
        )));
    };
    guard::check_outbound_url(&http_equiv)?;
    let uri: Uri = http_equiv
        .parse()
        .map_err(|e| HrnError::Config(format!("invalid WebSocket URL `{display_url}`: {e}")))?;
    let host = uri
        .host()
        .ok_or_else(|| HrnError::Config(format!("WebSocket URL missing host: `{display_url}`")))?
        .to_owned();
    let bare_host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    guard::check_outbound_host(&bare_host)?;
    let default_port = if is_tls { 443 } else { 80 };
    let port = uri.port_u16().unwrap_or(default_port);
    let host_header = if port == default_port {
        host
    } else {
        format!("{host}:{port}")
    };
    let path_and_query = uri
        .path_and_query()
        .map_or("/", http::uri::PathAndQuery::as_str)
        .to_owned();
    Ok(WsTarget {
        is_tls,
        bare_host,
        port,
        host_header,
        path_and_query,
        display_url,
    })
}

fn strip_scheme_ci<'a>(input: &'a str, scheme: &str) -> Option<&'a str> {
    let head = input.get(..scheme.len())?;
    head.eq_ignore_ascii_case(scheme)
        .then(|| input.get(scheme.len()..))
        .flatten()
}

/// Perform a simple HTTP/1.1 `GET` against a strictly loopback `http://127.0.0.1:<port>/...` URL.
pub async fn loopback_http_get(url: &str, timeout: Duration) -> Result<String> {
    let target = parse_loopback_url(url, "http")?;
    let disp = &target.display_url;
    let mut stream = tokio::time::timeout(
        timeout,
        TcpStream::connect((target.bare_host.as_str(), target.port)),
    )
    .await
    .map_err(|_| HrnError::Tool(format!("loopback GET timed out connecting to {disp}")))?
    .map_err(|e| HrnError::Tool(format!("loopback GET connect failed for {disp}: {e}")))?;

    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        target.path_and_query, target.host_header
    );
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| HrnError::Tool(format!("loopback GET write failed: {e}")))?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let split = read_http_head(&mut stream, &mut raw, &mut chunk, timeout, disp).await?;

    let head = String::from_utf8_lossy(raw.get(..split).unwrap_or_default());
    let first = head.lines().next().unwrap_or_default();
    if !first.contains(" 200 ") {
        return Err(HrnError::Tool(format!(
            "loopback GET {disp} returned `{first}`"
        )));
    }
    let content_length = head.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("Content-Length")
            .then(|| v.trim().parse::<usize>().ok())
            .flatten()
    });
    let body_start = split + 4;
    if let Some(expected) = content_length {
        while raw.len().saturating_sub(body_start) < expected {
            let n = tokio::time::timeout(timeout, stream.read(&mut chunk))
                .await
                .map_err(|_| HrnError::Tool(format!("loopback GET timed out reading {disp}")))?
                .map_err(|e| HrnError::Tool(format!("loopback GET read failed: {e}")))?;
            if n == 0 {
                break;
            }
            if let Some(slice) = chunk.get(..n) {
                raw.extend_from_slice(slice);
            }
        }
        let end = raw.len().min(body_start + expected);
        let body = raw.get(body_start..end).unwrap_or_default();
        return Ok(String::from_utf8_lossy(body).into_owned());
    }

    let _ = tokio::time::timeout(timeout, stream.read_to_end(&mut raw)).await;
    let body = raw.get(body_start..).unwrap_or_default();
    Ok(String::from_utf8_lossy(body).into_owned())
}

/// Connect to a `ws://` or `wss://` WebSocket endpoint, enforcing [`crate::guard::check_outbound_url`]
/// and verifying TLS certificates via the OS native store plus RFC 6455 `Sec-WebSocket-Accept`.
pub async fn connect_ws(url: &str) -> Result<WsStream> {
    connect_ws_timeout(url, DEFAULT_WS_TIMEOUT).await
}

/// Like [`connect_ws`] with an explicit connect and handshake timeout.
pub async fn connect_ws_timeout(url: &str, timeout: Duration) -> Result<WsStream> {
    let target = parse_general_ws_url(url)?;
    let disp = &target.display_url;
    let tcp = tokio::time::timeout(
        timeout,
        TcpStream::connect((target.bare_host.as_str(), target.port)),
    )
    .await
    .map_err(|_| HrnError::Tool(format!("ws connect timed out for {disp}")))?
    .map_err(|e| HrnError::Tool(format!("ws connect failed for {disp}: {e}")))?;

    let io = if target.is_tls {
        let tls_cfg = build_tls_config()?;
        let server_name = ServerName::try_from(target.bare_host.clone())
            .map_err(|_| HrnError::Config(format!("invalid TLS host `{}`", target.bare_host)))?;
        let connector = TlsConnector::from(Arc::new(tls_cfg));
        let tls_stream = tokio::time::timeout(timeout, connector.connect(server_name, tcp))
            .await
            .map_err(|_| HrnError::Tool(format!("ws TLS handshake timeout for {disp}")))?
            .map_err(|e| HrnError::Tool(format!("ws TLS connect failed for {disp}: {e}")))?;
        WsIo::Tls(Box::new(tls_stream))
    } else {
        WsIo::Plain(tcp)
    };
    perform_ws_handshake(io, &target, timeout, true).await
}

/// Connect to a local `ws://127.0.0.1:<port>/...` WebSocket endpoint (RFC 6455).
pub async fn connect_loopback_ws(url: &str, timeout: Duration) -> Result<WsStream> {
    let target = parse_loopback_url(url, "ws")?;
    let disp = &target.display_url;
    let stream = tokio::time::timeout(
        timeout,
        TcpStream::connect((target.bare_host.as_str(), target.port)),
    )
    .await
    .map_err(|_| HrnError::Tool(format!("ws connect timed out for {disp}")))?
    .map_err(|e| HrnError::Tool(format!("ws connect failed for {disp}: {e}")))?;
    perform_ws_handshake(WsIo::Plain(stream), &target, timeout, false).await
}

async fn perform_ws_handshake(
    mut io: WsIo,
    target: &WsTarget,
    timeout: Duration,
    verify_accept: bool,
) -> Result<WsStream> {
    let disp = &target.display_url;
    let mut key_bytes = [0u8; 16];
    getrandom::getrandom(&mut key_bytes)
        .map_err(|e| HrnError::Tool(format!("ws key rng failed: {e}")))?;
    let key = B64.encode(key_bytes);

    let req = format!(
        "GET {} HTTP/1.1\r\n\
         Host: {}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {}\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n",
        target.path_and_query, target.host_header, key
    );
    io.write_all(req.as_bytes())
        .await
        .map_err(|e| HrnError::Tool(format!("ws handshake write failed: {e}")))?;
    io.flush()
        .await
        .map_err(|e| HrnError::Tool(format!("ws handshake flush failed: {e}")))?;

    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let head_end = read_http_head(&mut io, &mut buf, &mut chunk, timeout, disp).await?;

    let head = String::from_utf8_lossy(buf.get(..head_end).unwrap_or_default());
    let status_line = head.lines().next().unwrap_or_default();
    if !status_line.contains(" 101 ") {
        return Err(HrnError::Tool(format!(
            "ws handshake rejected by {disp}: `{status_line}`"
        )));
    }
    if verify_accept {
        let expected = compute_ws_accept(&key);
        let got = head.lines().skip(1).find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("sec-websocket-accept")
                .then(|| v.trim())
        });
        let valid = got.is_some_and(|val| ct_eq(val.as_bytes(), expected.as_bytes()));
        if !valid {
            return Err(HrnError::Auth(format!(
                "ws server `{disp}` returned invalid Sec-WebSocket-Accept"
            )));
        }
    }

    let leftover = buf.get(head_end + 4..).unwrap_or_default().to_vec();
    Ok(WsStream::from_io(io, leftover))
}

async fn read_http_head<IO: tokio::io::AsyncRead + Unpin>(
    io: &mut IO,
    buf: &mut Vec<u8>,
    chunk: &mut [u8],
    timeout: Duration,
    url: &str,
) -> Result<usize> {
    loop {
        if let Some(pos) = find_header_end(buf) {
            return Ok(pos);
        }
        if buf.len() > MAX_HTTP_HEAD_BYTES {
            return Err(HrnError::Tool(format!(
                "HTTP response headers too large from {url}"
            )));
        }
        let n = tokio::time::timeout(timeout, io.read(chunk))
            .await
            .map_err(|_| HrnError::Tool(format!("HTTP header read timed out for {url}")))?
            .map_err(|e| HrnError::Tool(format!("HTTP header read failed for {url}: {e}")))?;
        if n == 0 {
            return Err(HrnError::Tool(format!(
                "connection closed before HTTP headers from {url}"
            )));
        }
        if let Some(slice) = chunk.get(..n) {
            buf.extend_from_slice(slice);
        }
    }
}

fn build_tls_config() -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| HrnError::Internal(format!("tls protocol config: {e}")))?
        .with_platform_verifier()
        .map_err(|e| HrnError::Internal(format!("platform cert verifier: {e}")))?
        .with_no_client_auth();
    Ok(tls)
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Compute the RFC 6455 `Sec-WebSocket-Accept` value for `client_key`.
pub(crate) fn compute_ws_accept(client_key: &str) -> String {
    let mut input = Vec::with_capacity(client_key.len() + WS_GUID.len());
    input.extend_from_slice(client_key.trim().as_bytes());
    input.extend_from_slice(WS_GUID.as_bytes());
    B64.encode(sha1(&input))
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((data.len() + 9).div_ceil(64) * 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut sched = [0u32; 80];
        for (idx, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            if let Some(slot) = sched.get_mut(idx) {
                *slot = u32::from_be_bytes(*word);
            }
        }
        for idx in 16..80 {
            let mixed = sched.get(idx - 3).copied().unwrap_or(0)
                ^ sched.get(idx - 8).copied().unwrap_or(0)
                ^ sched.get(idx - 14).copied().unwrap_or(0)
                ^ sched.get(idx - 16).copied().unwrap_or(0);
            if let Some(slot) = sched.get_mut(idx) {
                *slot = mixed.rotate_left(1);
            }
        }
        let [mut ra, mut rb, mut rc, mut rd, mut re] = state;
        for (idx, &wi) in sched.iter().enumerate() {
            let (round_f, round_k) = match idx {
                0..=19 => ((rb & rc) | ((!rb) & rd), 0x5a82_7999_u32),
                20..=39 => (rb ^ rc ^ rd, 0x6ed9_eba1_u32),
                40..=59 => ((rb & rc) | (rb & rd) | (rc & rd), 0x8f1b_bcdc_u32),
                _ => (rb ^ rc ^ rd, 0xca62_c1d6_u32),
            };
            let temp = ra
                .rotate_left(5)
                .wrapping_add(round_f)
                .wrapping_add(re)
                .wrapping_add(round_k)
                .wrapping_add(wi);
            re = rd;
            rd = rc;
            rc = rb.rotate_left(30);
            rb = ra;
            ra = temp;
        }
        for (slot, delta) in state.iter_mut().zip([ra, rb, rc, rd, re]) {
            *slot = slot.wrapping_add(delta);
        }
    }
    let mut out = [0u8; 20];
    for (idx, word) in state.iter().enumerate() {
        if let Some(dst) = out.get_mut(idx * 4..idx * 4 + 4) {
            dst.copy_from_slice(&word.to_be_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn rejects_non_loopback_ws_and_http_targets() {
        assert!(parse_loopback_url("ws://example.com:9222/a", "ws").is_err());
        assert!(parse_loopback_url("http://192.0.2.1:9222/json", "http").is_err());
        assert!(parse_loopback_url("ws://127.0.0.1:9222/devtools/page/1", "ws").is_ok());
    }

    #[test]
    fn general_ws_url_enforces_guard_and_strips_fragments() {
        assert!(parse_general_ws_url("wss://cloudcode-pa.googleapis.com/r/abc12345").is_err());
        assert!(parse_general_ws_url("http://127.0.0.1:8787/r/abc12345").is_err());
        let parsed = parse_general_ws_url("ws://127.0.0.1:8787/r/room12345?role=guest#secretkey")
            .expect("ok");
        assert!(!parsed.is_tls);
        assert_eq!(parsed.path_and_query, "/r/room12345?role=guest");
        assert!(!parsed.path_and_query.contains("secretkey"));
        let tls =
            parse_general_ws_url("wss://relay.example.com/r/room12345?role=host").expect("tls");
        assert!(tls.is_tls);
        assert_eq!(tls.port, 443);
        assert_eq!(
            compute_ws_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[tokio::test]
    async fn loopback_ws_roundtrips_small_and_large_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let srv = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut req = vec![0u8; 2048];
            let n = sock.read(&mut req).await.expect("read req");
            let req_str = String::from_utf8_lossy(&req[..n]);
            let key = req_str
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| v.trim().to_owned())
                })
                .unwrap_or_default();
            let accept = compute_ws_accept(&key);
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            );
            sock.write_all(resp.as_bytes()).await.expect("upgrade");

            let mut hdr = [0u8; 2];
            sock.read_exact(&mut hdr).await.expect("hdr");
            let opcode = hdr[0] & 0x0F;
            let len7 = hdr[1] & 0x7F;
            let len = match len7 {
                126 => {
                    let mut b = [0u8; 2];
                    sock.read_exact(&mut b).await.expect("len16");
                    usize::from(u16::from_be_bytes(b))
                }
                127 => {
                    let mut b = [0u8; 8];
                    sock.read_exact(&mut b).await.expect("len64");
                    usize::try_from(u64::from_be_bytes(b)).expect("len")
                }
                n => usize::from(n),
            };
            let mut mask = [0u8; 4];
            sock.read_exact(&mut mask).await.expect("mask");
            let mut data = vec![0u8; len];
            sock.read_exact(&mut data).await.expect("payload");
            for (i, b) in data.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }

            sock.write_all(&[0x89, 2, b'h', b'i']).await.expect("ping");
            let mut reply = vec![0x80 | opcode];
            if data.len() < 126 {
                reply.push(u8::try_from(data.len()).unwrap_or(0));
            } else if let Ok(len16) = u16::try_from(data.len()) {
                reply.push(126);
                reply.extend_from_slice(&len16.to_be_bytes());
            } else {
                reply.push(127);
                reply.extend_from_slice(&(data.len() as u64).to_be_bytes());
            }
            reply.extend_from_slice(&data);
            sock.write_all(&reply).await.expect("echo");
            sock.flush().await.expect("flush");
            let mut drain = [0u8; 64];
            while matches!(sock.read(&mut drain).await, Ok(n) if n > 0) {}
        });

        let url = format!("ws://127.0.0.1:{port}/devtools/page/1");
        let mut ws = connect_ws(&url).await.expect("connect");
        let big = "x".repeat(70_000);
        ws.send_text(&big).await.expect("send");
        let echoed = ws.recv_text().await.expect("recv").expect("some");
        assert_eq!(echoed.len(), 70_000);
        assert_eq!(echoed, big);
        drop(ws);
        let _ = srv.await;
    }

    #[tokio::test]
    async fn ws_errors_omit_url_fragments_and_userinfo() {
        let bad_scheme =
            parse_general_ws_url("ftp://alice:s3cr3t@relay.local/r/room12345#key=SUPERKEY")
                .err()
                .expect("bad scheme")
                .to_string();
        assert!(!bad_scheme.contains("s3cr3t"), "{bad_scheme}");
        assert!(!bad_scheme.contains("SUPERKEY"), "{bad_scheme}");
        assert!(!bad_scheme.contains('#'), "{bad_scheme}");

        // Bind then drop to guarantee an unused loopback port for connection refusal.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let url = format!("ws://bob:p4ssw0rd@127.0.0.1:{port}/r/room12345#key=TOPSECRETFRAG");
        let err = connect_ws_timeout(&url, Duration::from_secs(2))
            .await
            .err()
            .expect("must fail")
            .to_string();
        assert!(!err.contains("p4ssw0rd"), "{err}");
        assert!(!err.contains("TOPSECRETFRAG"), "{err}");
        assert!(!err.contains('#'), "{err}");
    }
}
