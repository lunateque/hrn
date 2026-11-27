//! Pure-Rust RFC 6455 client WebSocket stream framing over plain TCP or TLS.

use hrn_kernel::{HrnError, Result};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

/// Default maximum single WebSocket message accepted (32 MiB — accommodates CDP PNG screenshots).
pub(crate) const DEFAULT_MAX_WS_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
/// RFC 6455 §5.5 maximum payload size for control frames (Close, Ping, Pong).
const MAX_CONTROL_FRAME_BYTES: usize = 125;

/// Underlying client WebSocket transport (plain TCP for `ws://`, rustls TLS for `wss://`).
pub(crate) enum WsIo {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for WsIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for WsIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Connected RFC 6455 client WebSocket stream over `ws://` or `wss://`.
pub struct WsStream {
    reader: WsReader,
    writer: WsWriter,
}

impl WsStream {
    pub(crate) fn from_io(io: WsIo, leftover: Vec<u8>) -> Self {
        let (read_half, write_half) = tokio::io::split(io);
        Self {
            reader: WsReader {
                inner: read_half,
                buffered: leftover,
                pos: 0,
                max_message_bytes: DEFAULT_MAX_WS_MESSAGE_BYTES,
                assembled: Vec::new(),
                frag_kind: None,
            },
            writer: WsWriter { inner: write_half },
        }
    }

    /// Tighten the maximum inbound message size enforced by the reader.
    pub fn set_max_message_bytes(&mut self, max_bytes: usize) {
        self.reader.set_max_message_bytes(max_bytes);
    }

    /// Split into independent read and write halves for concurrent request/event handling.
    pub fn into_split(self) -> (WsReader, WsWriter) {
        (self.reader, self.writer)
    }

    /// Send a UTF-8 text frame.
    pub async fn send_text(&mut self, text: &str) -> Result<()> {
        self.writer.send_text(text).await
    }

    /// Send an opaque binary frame.
    pub async fn send_binary(&mut self, data: &[u8]) -> Result<()> {
        self.writer.send_binary(data).await
    }

    /// Send a clean RFC 6455 Close frame.
    pub async fn send_close(&mut self) -> Result<()> {
        self.writer.send_close().await
    }

    /// Receive the next complete UTF-8 text message (automatically replying to Ping frames).
    pub async fn recv_text(&mut self) -> Result<Option<String>> {
        self.reader.recv_text(Some(&mut self.writer)).await
    }

    /// Receive the next complete binary message (automatically replying to Ping frames).
    pub async fn recv_binary(&mut self) -> Result<Option<Vec<u8>>> {
        self.reader.recv_binary(Some(&mut self.writer)).await
    }
}

/// Write half of a connected [`WsStream`].
pub struct WsWriter {
    inner: WriteHalf<WsIo>,
}

impl WsWriter {
    /// Send a masked RFC 6455 text frame.
    pub async fn send_text(&mut self, text: &str) -> Result<()> {
        self.send_frame(0x1, text.as_bytes()).await
    }

    /// Send a masked RFC 6455 binary frame.
    pub async fn send_binary(&mut self, data: &[u8]) -> Result<()> {
        self.send_frame(0x2, data).await
    }

    /// Send a masked RFC 6455 pong frame.
    pub async fn send_pong(&mut self, payload: &[u8]) -> Result<()> {
        self.send_frame(0xA, payload).await
    }

    /// Send a masked RFC 6455 close frame.
    pub async fn send_close(&mut self) -> Result<()> {
        self.send_frame(0x8, &[]).await
    }

    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<()> {
        let mut mask = [0u8; 4];
        getrandom::getrandom(&mut mask)
            .map_err(|e| HrnError::Tool(format!("ws mask rng failed: {e}")))?;

        let mut frame = Vec::with_capacity(14 + payload.len());
        frame.push(0x80 | (opcode & 0x0F));
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | u8::try_from(len).unwrap_or(0));
        } else if let Ok(len16) = u16::try_from(len) {
            frame.push(0x80 | 0x7E);
            frame.extend_from_slice(&len16.to_be_bytes());
        } else {
            frame.push(0x80 | 0x7F);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (i, &b) in payload.iter().enumerate() {
            let m = mask.get(i & 3).copied().unwrap_or(0);
            frame.push(b ^ m);
        }
        self.inner
            .write_all(&frame)
            .await
            .map_err(|e| HrnError::Tool(format!("ws write failed: {e}")))?;
        self.inner
            .flush()
            .await
            .map_err(|e| HrnError::Tool(format!("ws flush failed: {e}")))?;
        Ok(())
    }
}

/// Read half of a connected [`WsStream`].
pub struct WsReader {
    inner: ReadHalf<WsIo>,
    buffered: Vec<u8>,
    pos: usize,
    max_message_bytes: usize,
    assembled: Vec<u8>,
    frag_kind: Option<FragKind>,
}

/// One decoded control or data event returned by [`WsReader::next_event`].
pub enum WsEvent {
    /// Complete UTF-8 text message (reassembled across continuation frames if fragmented).
    Text(String),
    /// Complete binary message (reassembled across continuation frames if fragmented).
    Binary(Vec<u8>),
    /// Server Ping payload (caller should reply via [`WsWriter::send_pong`]).
    Ping(Vec<u8>),
    /// Clean close or EOF.
    Closed,
}

#[derive(Clone, Copy)]
enum FragKind {
    Text,
    Binary,
}

impl WsReader {
    /// Tighten the maximum inbound message size enforced by this reader.
    pub fn set_max_message_bytes(&mut self, max_bytes: usize) {
        self.max_message_bytes = max_bytes;
    }

    /// Read the next data message or Ping/Close control frame.
    ///
    /// Fragment assembly state lives on `self` so RFC 6455–legal control frames (Ping/Pong)
    /// interleaved between data fragments preserve the in-progress message.
    pub async fn next_event(&mut self) -> Result<WsEvent> {
        loop {
            let Some((fin, opcode, payload)) = self.read_raw_frame().await? else {
                return Ok(WsEvent::Closed);
            };
            match opcode {
                0x8 => return Ok(WsEvent::Closed),
                0x9 => return Ok(WsEvent::Ping(payload)),
                0xA => {}
                0x1 => {
                    if let Some(ev) = finish_or_buffer(
                        FragKind::Text,
                        fin,
                        payload,
                        &mut self.assembled,
                        &mut self.frag_kind,
                    )? {
                        return Ok(ev);
                    }
                }
                0x2 => {
                    if let Some(ev) = finish_or_buffer(
                        FragKind::Binary,
                        fin,
                        payload,
                        &mut self.assembled,
                        &mut self.frag_kind,
                    )? {
                        return Ok(ev);
                    }
                }
                0x0 => {
                    let Some(kind) = self.frag_kind else {
                        return Err(HrnError::Tool(
                            "unexpected ws continuation frame".to_owned(),
                        ));
                    };
                    self.assembled.extend_from_slice(&payload);
                    if fin {
                        self.frag_kind = None;
                        return decode_complete(kind, std::mem::take(&mut self.assembled));
                    }
                }
                _ => {
                    return Err(HrnError::Tool(format!("unsupported ws opcode {opcode:#x}")));
                }
            }
        }
    }

    async fn read_raw_frame(&mut self) -> Result<Option<(bool, u8, Vec<u8>)>> {
        let mut hdr = [0u8; 2];
        if !self.read_exact_or_eof(&mut hdr).await? {
            return Ok(None);
        }
        let [b0, b1] = hdr;
        let fin = (b0 & 0x80) != 0;
        let opcode = b0 & 0x0F;
        let masked = (b1 & 0x80) != 0;
        let len7 = b1 & 0x7F;

        let payload_len: usize = match len7 {
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
                    .map_err(|_| HrnError::Tool("ws frame length overflow".to_owned()))?
            }
            _ => 0,
        };
        if opcode >= 0x8 {
            if !fin || payload_len > MAX_CONTROL_FRAME_BYTES {
                return Err(HrnError::Tool(
                    "invalid RFC 6455 control frame (fragmented or >125 bytes)".to_owned(),
                ));
            }
        } else if payload_len > self.max_message_bytes
            || self.assembled.len().saturating_add(payload_len) > self.max_message_bytes
        {
            return Err(HrnError::Tool(format!(
                "ws message exceeded {} byte limit",
                self.max_message_bytes
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
        Ok(Some((fin, opcode, payload)))
    }

    pub(crate) async fn recv_text(
        &mut self,
        mut writer: Option<&mut WsWriter>,
    ) -> Result<Option<String>> {
        loop {
            match self.next_event().await? {
                WsEvent::Text(t) => return Ok(Some(t)),
                WsEvent::Binary(_) => {}
                WsEvent::Ping(p) => {
                    if let Some(w) = writer.as_deref_mut() {
                        w.send_pong(&p).await?;
                    }
                }
                WsEvent::Closed => return Ok(None),
            }
        }
    }

    pub(crate) async fn recv_binary(
        &mut self,
        mut writer: Option<&mut WsWriter>,
    ) -> Result<Option<Vec<u8>>> {
        loop {
            match self.next_event().await? {
                WsEvent::Binary(b) => return Ok(Some(b)),
                WsEvent::Text(_) => {}
                WsEvent::Ping(p) => {
                    if let Some(w) = writer.as_deref_mut() {
                        w.send_pong(&p).await?;
                    }
                }
                WsEvent::Closed => return Ok(None),
            }
        }
    }

    async fn read_exact_or_eof(&mut self, dst: &mut [u8]) -> Result<bool> {
        let mut filled = 0usize;
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
                if self.pos >= self.buffered.len() {
                    self.buffered.clear();
                    self.pos = 0;
                }
                continue;
            }
            let Some(slice) = dst.get_mut(filled..) else {
                break;
            };
            let n = self
                .inner
                .read(slice)
                .await
                .map_err(|e| HrnError::Tool(format!("ws read failed: {e}")))?;
            if n == 0 {
                return if filled == 0 {
                    Ok(false)
                } else {
                    Err(HrnError::Tool("ws unexpected EOF mid-frame".to_owned()))
                };
            }
            filled += n;
        }
        Ok(true)
    }

    async fn read_exact_required(&mut self, dst: &mut [u8]) -> Result<()> {
        if self.read_exact_or_eof(dst).await? {
            Ok(())
        } else {
            Err(HrnError::Tool("ws unexpected EOF".to_owned()))
        }
    }
}

fn finish_or_buffer(
    kind: FragKind,
    fin: bool,
    payload: Vec<u8>,
    assembled: &mut Vec<u8>,
    frag_kind: &mut Option<FragKind>,
) -> Result<Option<WsEvent>> {
    if frag_kind.is_some() {
        return Err(HrnError::Tool(
            "unexpected new ws data frame before continuation finished".to_owned(),
        ));
    }
    if fin {
        return decode_complete(kind, payload).map(Some);
    }
    *assembled = payload;
    *frag_kind = Some(kind);
    Ok(None)
}

fn decode_complete(kind: FragKind, data: Vec<u8>) -> Result<WsEvent> {
    match kind {
        FragKind::Text => {
            let text = String::from_utf8(data)
                .map_err(|e| HrnError::Tool(format!("ws invalid UTF-8: {e}")))?;
            Ok(WsEvent::Text(text))
        }
        FragKind::Binary => Ok(WsEvent::Binary(data)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn loopback_ws_pair() -> (WsStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (client, server) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let client = client.expect("connect");
        let (server, _) = server.expect("accept");
        (WsStream::from_io(WsIo::Plain(client), Vec::new()), server)
    }

    #[tokio::test]
    async fn ws_reader_rejects_invalid_control_frames_and_preserves_interleaved_fragments() {
        // 1. Interleaved Ping between data fragment 1 and continuation frame preserves full message.
        let (ws, mut srv) = loopback_ws_pair().await;
        let (mut reader, _writer) = ws.into_split();
        let writer_task = tokio::spawn(async move {
            // Binary fragment 1 (fin=0, opcode=2): "hello "
            srv.write_all(&[0x02, 6, b'h', b'e', b'l', b'l', b'o', b' '])
                .await
                .expect("f1");
            // Interleaved Ping (fin=1, opcode=9): "ok"
            srv.write_all(&[0x89, 2, b'o', b'k']).await.expect("ping");
            // Continuation final fragment (fin=1, opcode=0): "world"
            srv.write_all(&[0x80, 5, b'w', b'o', b'r', b'l', b'd'])
                .await
                .expect("f2");
            srv.flush().await.expect("flush");
        });
        assert!(matches!(reader.next_event().await.expect("ev1"), WsEvent::Ping(p) if p == b"ok"));
        assert!(
            matches!(reader.next_event().await.expect("ev2"), WsEvent::Binary(b) if b == b"hello world")
        );
        let _ = writer_task.await;

        // 2. Oversized Ping (>125 bytes) is rejected per RFC 6455 §5.5.
        let (ws, mut srv) = loopback_ws_pair().await;
        let (mut reader, _writer) = ws.into_split();
        let mut big_ping = vec![0x89, 126, 0, 126];
        big_ping.extend_from_slice(&[b'x'; 126]);
        srv.write_all(&big_ping).await.expect("big ping");
        assert!(reader.next_event().await.is_err());

        // 3. Fragmented control frame (fin=0, opcode=9) is rejected per RFC 6455 §5.5.
        let (ws, mut srv) = loopback_ws_pair().await;
        let (mut reader, _writer) = ws.into_split();
        srv.write_all(&[0x09, 2, b'h', b'i'])
            .await
            .expect("frag ping");
        assert!(reader.next_event().await.is_err());
    }
}
