//! Pure-Rust loopback RFC 6455 WebSocket server framing with bearer-token authentication.
//!
//! Used by `hrn browser-relay serve --stdio` to expose a local CDP WebSocket bridge to `hrn`
//! without adding external WebSocket dependencies.

use crate::auth::ct_eq;
use hrn_kernel::{HrnError, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// RFC 6455 WebSocket handshake GUID.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Maximum HTTP upgrade header size accepted (64 KiB).
const MAX_HTTP_HEAD_BYTES: usize = 64 * 1024;
/// Maximum WebSocket payload accepted (32 MiB — accommodates full-page PNG screenshots).
const MAX_WS_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

/// An authenticated server-side RFC 6455 WebSocket connection.
pub struct ServerWs {
    path: String,
    reader: ServerWsReader,
    writer: ServerWsWriter,
}

impl ServerWs {
    /// Request path (without query string), e.g. `"/control"` or `"/tab/42"`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Split into independent read and write halves.
    pub fn into_split(self) -> (ServerWsReader, ServerWsWriter) {
        (self.reader, self.writer)
    }
}

/// Decoded inbound frame event on a [`ServerWs`].
pub enum ServerWsEvent {
    /// Complete UTF-8 text message.
    Text(String),
    /// Client Ping payload.
    Ping(Vec<u8>),
    /// Clean Close or EOF.
    Closed,
}

/// Read half of [`ServerWs`].
pub struct ServerWsReader {
    inner: OwnedReadHalf,
    buffered: Vec<u8>,
    pos: usize,
}

/// Write half of [`ServerWs`].
pub struct ServerWsWriter {
    inner: OwnedWriteHalf,
}

impl ServerWsWriter {
    /// Send an unmasked RFC 6455 UTF-8 text frame.
    pub async fn send_text(&mut self, text: &str) -> Result<()> {
        self.send_frame(0x1, text.as_bytes()).await
    }

    /// Send an unmasked RFC 6455 pong frame.
    pub async fn send_pong(&mut self, payload: &[u8]) -> Result<()> {
        self.send_frame(0xA, payload).await
    }

    /// Send an unmasked RFC 6455 close frame.
    pub async fn send_close(&mut self) -> Result<()> {
        self.send_frame(0x8, &[]).await
    }

    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::with_capacity(10 + payload.len());
        frame.push(0x80 | (opcode & 0x0F));
        let len = payload.len();
        if len < 126 {
            frame.push(u8::try_from(len).unwrap_or(0));
        } else if let Ok(len16) = u16::try_from(len) {
            frame.push(126);
            frame.extend_from_slice(&len16.to_be_bytes());
        } else {
            frame.push(127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        frame.extend_from_slice(payload);
        self.inner
            .write_all(&frame)
            .await
            .map_err(|e| HrnError::Io(format!("relay ws write failed: {e}")))?;
        self.inner
            .flush()
            .await
            .map_err(|e| HrnError::Io(format!("relay ws flush failed: {e}")))?;
        Ok(())
    }
}

impl ServerWsReader {
    /// Read the next message, Ping, or Close frame.
    pub async fn next_event(&mut self) -> Result<ServerWsEvent> {
        let mut assembled = Vec::new();
        let mut assembling = false;

        loop {
            let mut hdr = [0u8; 2];
            if !self.read_exact_or_eof(&mut hdr).await? {
                return Ok(ServerWsEvent::Closed);
            }
            let [b0, b1] = hdr;
            let fin = (b0 & 0x80) != 0;
            let opcode = b0 & 0x0F;
            let masked = (b1 & 0x80) != 0;
            let len7 = b1 & 0x7F;

            let payload_len = match len7 {
                0..=125 => usize::from(len7),
                126 => {
                    let mut b = [0u8; 2];
                    self.read_exact_required(&mut b).await?;
                    usize::from(u16::from_be_bytes(b))
                }
                127 => {
                    let mut b = [0u8; 8];
                    self.read_exact_required(&mut b).await?;
                    usize::try_from(u64::from_be_bytes(b))
                        .map_err(|_| HrnError::Tool("relay ws frame length overflow".to_owned()))?
                }
                _ => 0,
            };
            if payload_len > MAX_WS_MESSAGE_BYTES
                || assembled.len().saturating_add(payload_len) > MAX_WS_MESSAGE_BYTES
            {
                return Err(HrnError::Tool(format!(
                    "relay ws message exceeded {MAX_WS_MESSAGE_BYTES} byte limit"
                )));
            }

            let mask_key = if masked {
                let mut m = [0u8; 4];
                self.read_exact_required(&mut m).await?;
                Some(m)
            } else {
                None
            };

            let mut payload = vec![0u8; payload_len];
            if payload_len > 0 {
                self.read_exact_required(&mut payload).await?;
            }
            if let Some(m) = mask_key {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= m.get(i & 3).copied().unwrap_or(0);
                }
            }

            match opcode {
                0x8 => return Ok(ServerWsEvent::Closed),
                0x9 => return Ok(ServerWsEvent::Ping(payload)),
                0x1 => {
                    if fin && !assembling {
                        let text = String::from_utf8(payload)
                            .map_err(|e| HrnError::Tool(format!("relay ws invalid UTF-8: {e}")))?;
                        return Ok(ServerWsEvent::Text(text));
                    }
                    assembled.clear();
                    assembled.extend_from_slice(&payload);
                    assembling = true;
                    if fin {
                        let text = String::from_utf8(assembled)
                            .map_err(|e| HrnError::Tool(format!("relay ws invalid UTF-8: {e}")))?;
                        return Ok(ServerWsEvent::Text(text));
                    }
                }
                0x0 if assembling => {
                    assembled.extend_from_slice(&payload);
                    if fin {
                        let text = String::from_utf8(assembled)
                            .map_err(|e| HrnError::Tool(format!("relay ws invalid UTF-8: {e}")))?;
                        return Ok(ServerWsEvent::Text(text));
                    }
                }
                _ => {}
            }
        }
    }

    async fn read_exact_or_eof(&mut self, dst: &mut [u8]) -> Result<bool> {
        let mut filled = 0;
        while filled < dst.len() {
            if self.pos < self.buffered.len() {
                let avail = self.buffered.len() - self.pos;
                let take = avail.min(dst.len() - filled);
                if let (Some(src), Some(out)) = (
                    self.buffered.get(self.pos..self.pos + take),
                    dst.get_mut(filled..filled + take),
                ) {
                    out.copy_from_slice(src);
                }
                self.pos += take;
                filled += take;
                continue;
            }
            let Some(out) = dst.get_mut(filled..) else {
                break;
            };
            let n = self
                .inner
                .read(out)
                .await
                .map_err(|e| HrnError::Io(format!("relay ws read failed: {e}")))?;
            if n == 0 {
                return Ok(false);
            }
            filled += n;
        }
        Ok(true)
    }

    async fn read_exact_required(&mut self, dst: &mut [u8]) -> Result<()> {
        if self.read_exact_or_eof(dst).await? {
            Ok(())
        } else {
            Err(HrnError::Io("relay ws unexpected EOF".to_owned()))
        }
    }
}

/// Complete an RFC 6455 HTTP/1.1 upgrade on `stream`, verifying `expected_token` via either
/// `?token=<tok>` query parameter or `Authorization: Bearer <tok>` header.
///
/// # Errors
///
/// Returns an error if the HTTP request is malformed, authentication fails, or the client
/// did not supply `Sec-WebSocket-Key`.
pub async fn accept_loopback_ws(mut stream: TcpStream, expected_token: &str) -> Result<ServerWs> {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 2048];
    let split = loop {
        if let Some(pos) = find_header_end(&raw) {
            break pos;
        }
        if raw.len() > MAX_HTTP_HEAD_BYTES {
            let _ = stream
                .write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\n\r\n")
                .await;
            return Err(HrnError::Tool("relay ws headers too large".to_owned()));
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| HrnError::Io(format!("relay ws handshake read failed: {e}")))?;
        if n == 0 {
            return Err(HrnError::Io("relay ws closed before handshake".to_owned()));
        }
        if let Some(slice) = chunk.get(..n) {
            raw.extend_from_slice(slice);
        }
    };

    let head = String::from_utf8_lossy(raw.get(..split).unwrap_or_default()).into_owned();
    let mut lines = head.lines();
    let req_line = lines.next().unwrap_or_default();
    let target = req_line.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_owned(), Some(q)),
        None => (target.to_owned(), None),
    };

    let mut ws_key: Option<String> = None;
    let mut auth_token: Option<String> = query.and_then(extract_query_token);
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let name = k.trim();
            let val = v.trim();
            if name.eq_ignore_ascii_case("sec-websocket-key") {
                ws_key = Some(val.to_owned());
            } else if name.eq_ignore_ascii_case("authorization")
                && let Some(tok) = val.strip_prefix("Bearer ")
            {
                auth_token = Some(tok.trim().to_owned());
            }
        }
    }

    let authorized = auth_token
        .as_deref()
        .is_some_and(|tok| ct_eq(tok.as_bytes(), expected_token.as_bytes()));
    if !authorized {
        let _ = stream
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
            .await;
        return Err(HrnError::Auth("relay ws unauthorized".to_owned()));
    }

    let Some(key) = ws_key else {
        let _ = stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await;
        return Err(HrnError::Tool(
            "relay ws missing Sec-WebSocket-Key".to_owned(),
        ));
    };

    let accept = ws_accept_key(&key);
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream
        .write_all(resp.as_bytes())
        .await
        .map_err(|e| HrnError::Io(format!("relay ws upgrade write failed: {e}")))?;

    let leftover = raw.get(split + 4..).unwrap_or_default().to_vec();
    let (read_half, write_half) = stream.into_split();
    Ok(ServerWs {
        path,
        reader: ServerWsReader {
            inner: read_half,
            buffered: leftover,
            pos: 0,
        },
        writer: ServerWsWriter { inner: write_half },
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn extract_query_token(query: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "token" && !v.is_empty()).then(|| v.to_owned())
    })
}

/// Compute the RFC 6455 `Sec-WebSocket-Accept` response header value for `client_key`.
pub fn ws_accept_key(client_key: &str) -> String {
    let mut input = Vec::with_capacity(client_key.len() + WS_GUID.len());
    input.extend_from_slice(client_key.trim().as_bytes());
    input.extend_from_slice(WS_GUID.as_bytes());
    base64_encode(&sha1(&input))
}

/// Pure safe Rust RFC 3174 SHA-1 (used strictly for the RFC 6455 WebSocket handshake accept key).
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
        let deltas = [ra, rb, rc, rd, re];
        for (slot, delta) in state.iter_mut().zip(deltas) {
            *slot = slot.wrapping_add(delta);
        }
    }

    let mut out = [0u8; 20];
    for (idx, word) in state.iter().enumerate() {
        let bytes = word.to_be_bytes();
        if let Some(dst) = out.get_mut(idx * 4..idx * 4 + 4) {
            dst.copy_from_slice(&bytes);
        }
    }
    out
}

const B64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        let idx = |shift: u32| usize::try_from((triple >> shift) & 0x3f).unwrap_or(0);
        out.push(char::from(B64_TABLE.get(idx(18)).copied().unwrap_or(b'A')));
        out.push(char::from(B64_TABLE.get(idx(12)).copied().unwrap_or(b'A')));
        out.push(if chunk.len() > 1 {
            char::from(B64_TABLE.get(idx(6)).copied().unwrap_or(b'A'))
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(B64_TABLE.get(idx(0)).copied().unwrap_or(b'A'))
        } else {
            '='
        });
    }
    out
}

/// Decode standard Base64 (ignoring ASCII whitespace). Used to derive Chrome extension IDs
/// from pinned SPKI public keys.
pub fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let mut vals: Vec<u8> = Vec::with_capacity(input.len());
    let mut pad = 0usize;
    for b in input.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            pad += 1;
            continue;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => {
                return Err(HrnError::Config(format!("invalid base64 byte `0x{b:02x}`")));
            }
        };
        vals.push(v);
    }
    if !(vals.len() + pad).is_multiple_of(4) || pad > 2 {
        return Err(HrnError::Config("malformed base64 padding".to_owned()));
    }
    let mut out = Vec::with_capacity((vals.len() * 3) / 4);
    for chunk in vals.chunks(4) {
        let c0 = u32::from(chunk.first().copied().unwrap_or(0));
        let c1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let c2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let c3 = u32::from(chunk.get(3).copied().unwrap_or(0));
        let triple = (c0 << 18) | (c1 << 12) | (c2 << 6) | c3;
        out.push(u8::try_from((triple >> 16) & 0xff).unwrap_or(0));
        if chunk.len() > 2 {
            out.push(u8::try_from((triple >> 8) & 0xff).unwrap_or(0));
        }
        if chunk.len() > 3 {
            out.push(u8::try_from(triple & 0xff).unwrap_or(0));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6455_accept_vector_matches() {
        assert_eq!(
            ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn base64_roundtrips() {
        let raw = b"hello hrn browser relay";
        let enc = base64_encode(raw);
        assert_eq!(base64_decode(&enc).expect("decode"), raw);
    }
}
