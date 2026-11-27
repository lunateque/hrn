//! Pure-Rust server-side RFC 6455 binary WebSocket frame reader/writer over upgraded `hyper` IO.

use hrn_kernel::{HrnError, Result};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};

/// RFC 6455 §5.5 maximum payload size for control frames (Close, Ping, Pong).
const MAX_CONTROL_FRAME_BYTES: usize = 125;

/// Decoded inbound server WebSocket frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RelayWsEvent {
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Closed,
}

/// Read half of an upgraded relay WebSocket connection.
pub(crate) struct RelayWsReader<R = ReadHalf<TokioIo<Upgraded>>> {
    inner: R,
    max_frame_bytes: usize,
    assembled: Vec<u8>,
    assembling: bool,
}

/// Write half of an upgraded relay WebSocket connection.
pub(crate) struct RelayWsWriter {
    inner: WriteHalf<TokioIo<Upgraded>>,
}

/// Split an upgraded `hyper` stream into RFC 6455 reader/writer halves.
pub(crate) fn split_upgraded(
    upgraded: Upgraded,
    max_frame_bytes: usize,
) -> (RelayWsReader, RelayWsWriter) {
    let (r, w) = tokio::io::split(TokioIo::new(upgraded));
    (
        RelayWsReader::new(r, max_frame_bytes),
        RelayWsWriter { inner: w },
    )
}

impl RelayWsWriter {
    /// Send an unmasked RFC 6455 binary frame.
    pub(crate) async fn send_binary(&mut self, payload: &[u8]) -> Result<()> {
        self.send_frame(0x2, payload).await
    }

    /// Send an unmasked RFC 6455 pong frame (truncated to RFC 6455 125-byte control cap).
    pub(crate) async fn send_pong(&mut self, payload: &[u8]) -> Result<()> {
        let capped = payload
            .get(..payload.len().min(MAX_CONTROL_FRAME_BYTES))
            .unwrap_or_default();
        self.send_frame(0xA, capped).await
    }

    /// Send an unmasked RFC 6455 close frame.
    pub(crate) async fn send_close(&mut self) -> Result<()> {
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
            .map_err(|e| HrnError::Io(format!("relay ws write: {e}")))?;
        self.inner
            .flush()
            .await
            .map_err(|e| HrnError::Io(format!("relay ws flush: {e}")))
    }
}

impl<R: AsyncRead + Unpin> RelayWsReader<R> {
    pub(crate) fn new(inner: R, max_frame_bytes: usize) -> Self {
        Self {
            inner,
            max_frame_bytes,
            assembled: Vec::new(),
            assembling: false,
        }
    }

    /// Read the next complete binary message or control frame, enforcing `max_frame_bytes`
    /// and RFC 6455 §5.5 control-frame rules BEFORE allocating payload buffers.
    ///
    /// Fragment assembly state lives on `self` so RFC 6455–legal Pings/Pongs interleaved between
    /// data fragments preserve the in-progress message.
    pub(crate) async fn next_event(&mut self) -> Result<RelayWsEvent> {
        loop {
            let mut hdr = [0u8; 2];
            if !self.read_exact_or_eof(&mut hdr).await? {
                return Ok(RelayWsEvent::Closed);
            }
            let [b0, b1] = hdr;
            let fin = (b0 & 0x80) != 0;
            let opcode = b0 & 0x0F;
            let masked = (b1 & 0x80) != 0;
            if !masked {
                return Err(HrnError::Auth("unmasked client WebSocket frame".to_owned()));
            }
            let payload_len = self.read_payload_len(b1 & 0x7F).await?;
            if opcode >= 0x8 {
                if !fin || payload_len > MAX_CONTROL_FRAME_BYTES {
                    return Err(HrnError::Tool(
                        "invalid RFC 6455 control frame (fragmented or >125 bytes)".to_owned(),
                    ));
                }
            } else if payload_len > self.max_frame_bytes
                || self.assembled.len().saturating_add(payload_len) > self.max_frame_bytes
            {
                return Err(HrnError::Tool(format!(
                    "ws frame exceeds {} byte cap",
                    self.max_frame_bytes
                )));
            }
            let payload = self.read_masked_payload(payload_len).await?;

            match opcode {
                0x8 => return Ok(RelayWsEvent::Closed),
                0x9 => return Ok(RelayWsEvent::Ping(payload)),
                0xA => {} // Pong
                0x2 => {
                    if self.assembling {
                        return Err(HrnError::Tool(
                            "unexpected binary frame before continuation finished".to_owned(),
                        ));
                    }
                    if fin {
                        return Ok(RelayWsEvent::Binary(payload));
                    }
                    self.assembled = payload;
                    self.assembling = true;
                }
                0x0 if self.assembling => {
                    self.assembled.extend_from_slice(&payload);
                    if fin {
                        self.assembling = false;
                        return Ok(RelayWsEvent::Binary(std::mem::take(&mut self.assembled)));
                    }
                }
                _ => {
                    // Reject plaintext text frames or unknown opcodes on the sealed binary channel.
                    return Err(HrnError::Tool("unsupported ws opcode on relay".to_owned()));
                }
            }
        }
    }

    async fn read_payload_len(&mut self, len7: u8) -> Result<usize> {
        match len7 {
            0..=125 => Ok(usize::from(len7)),
            126 => {
                let mut b = [0u8; 2];
                self.read_exact_required(&mut b).await?;
                Ok(usize::from(u16::from_be_bytes(b)))
            }
            127 => {
                let mut b = [0u8; 8];
                self.read_exact_required(&mut b).await?;
                usize::try_from(u64::from_be_bytes(b))
                    .map_err(|_| HrnError::Tool("ws frame length overflow".to_owned()))
            }
            _ => Ok(0),
        }
    }

    async fn read_masked_payload(&mut self, payload_len: usize) -> Result<Vec<u8>> {
        let mut mask = [0u8; 4];
        self.read_exact_required(&mut mask).await?;
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            self.read_exact_required(&mut payload).await?;
        }
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask.get(i & 3).copied().unwrap_or(0);
        }
        Ok(payload)
    }

    async fn read_exact_or_eof(&mut self, dst: &mut [u8]) -> Result<bool> {
        let mut filled = 0usize;
        while filled < dst.len() {
            let Some(slice) = dst.get_mut(filled..) else {
                break;
            };
            let n = self
                .inner
                .read(slice)
                .await
                .map_err(|e| HrnError::Io(format!("relay ws read: {e}")))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn masked_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x12u8, 0x34, 0x56, 0x78];
        let mut out = vec![if fin { 0x80 } else { 0x00 } | (opcode & 0x0F)];
        if payload.len() < 126 {
            out.push(0x80 | u8::try_from(payload.len()).unwrap_or(0));
        } else {
            out.push(0x80 | 0x7E);
            out.extend_from_slice(&u16::try_from(payload.len()).unwrap_or(0).to_be_bytes());
        }
        out.extend_from_slice(&mask);
        for (i, &b) in payload.iter().enumerate() {
            out.push(b ^ mask[i & 3]);
        }
        out
    }

    #[tokio::test]
    async fn relay_ws_rejects_invalid_control_frames_and_preserves_interleaved_ping() {
        // 1. Ping interleaved between binary fragment 1 and continuation preserves complete message.
        let (mut tx, rx) = tokio::io::duplex(4096);
        let mut reader = RelayWsReader::new(rx, 4096);
        tx.write_all(&masked_frame(false, 0x2, b"part1-"))
            .await
            .expect("f1");
        tx.write_all(&masked_frame(true, 0x9, b"hb"))
            .await
            .expect("ping");
        tx.write_all(&masked_frame(true, 0x0, b"part2"))
            .await
            .expect("f2");
        drop(tx);
        assert_eq!(
            reader.next_event().await.expect("ev1"),
            RelayWsEvent::Ping(b"hb".to_vec())
        );
        assert_eq!(
            reader.next_event().await.expect("ev2"),
            RelayWsEvent::Binary(b"part1-part2".to_vec())
        );

        // 2. Oversized Ping (>125 bytes) rejected before allocation.
        let (mut tx, rx) = tokio::io::duplex(4096);
        let mut reader = RelayWsReader::new(rx, 4096);
        tx.write_all(&masked_frame(true, 0x9, &[b'p'; 126]))
            .await
            .expect("oversize ping");
        assert!(reader.next_event().await.is_err());

        // 3. Fragmented control frame (fin=false) rejected.
        let (mut tx, rx) = tokio::io::duplex(4096);
        let mut reader = RelayWsReader::new(rx, 4096);
        tx.write_all(&masked_frame(false, 0x9, b"hi"))
            .await
            .expect("frag ping");
        assert!(reader.next_event().await.is_err());
    }
}
