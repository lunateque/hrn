//! Chrome Native Messaging stdio framing (`u32` little-endian length prefix + UTF-8 JSON).
//!
//! Used by `hrn browser-relay serve --stdio` to communicate with the Chrome MV3 relay extension.
//! Chrome enforces asymmetric limits: **1 MiB** from native host → extension and **64 MiB** from
//! extension → native host. Oversized host → extension messages are split into `cmd: "chunk"`
//! frames via [`write_native_message_chunked`].

use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{HrnError, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Chrome Native Messaging ceiling for a single host → extension message (1 MiB).
pub const MAX_NATIVE_WRITE_BYTES: usize = 1024 * 1024;
/// Chrome Native Messaging ceiling for a single extension → host message (64 MiB).
pub const MAX_NATIVE_READ_BYTES: usize = 64 * 1024 * 1024;
/// Backwards-compatible alias for [`MAX_NATIVE_WRITE_BYTES`].
pub const MAX_NATIVE_MESSAGE_BYTES: usize = MAX_NATIVE_WRITE_BYTES;
/// Chunk slice target (bytes) when splitting host → extension messages above 1 MiB.
const NATIVE_CHUNK_BYTES: usize = 900_000;

static CHUNK_SEQ: AtomicU64 = AtomicU64::new(1);

/// Read one Chrome Native Messaging frame (`u32` LE byte length followed by UTF-8 JSON).
///
/// Accepts up to [`MAX_NATIVE_READ_BYTES`] (64 MiB, Chrome's extension → host limit) so full-page
/// Retina PNG screenshots and large accessibility trees transfer intact.
/// Returns `Ok(None)` when `reader` reaches clean EOF before the next frame header.
///
/// # Errors
///
/// Returns an error on truncated frames, invalid UTF-8/JSON, or lengths exceeding
/// [`MAX_NATIVE_READ_BYTES`].
pub async fn read_native_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut header = [0u8; 4];
    let mut read_so_far = 0usize;
    while read_so_far < 4 {
        let Some(slice) = header.get_mut(read_so_far..) else {
            break;
        };
        let n = reader
            .read(slice)
            .await
            .map_err(|e| HrnError::Io(format!("native messaging header read failed: {e}")))?;
        if n == 0 {
            if read_so_far == 0 {
                return Ok(None);
            }
            return Err(HrnError::Io(
                "truncated Chrome Native Messaging length header".to_owned(),
            ));
        }
        read_so_far += n;
    }

    let len = usize::try_from(u32::from_le_bytes(header))
        .map_err(|_| HrnError::Tool("native messaging length overflow".to_owned()))?;
    if len == 0 {
        return Err(HrnError::Tool(
            "empty Chrome Native Messaging frame".to_owned(),
        ));
    }
    if len > MAX_NATIVE_READ_BYTES {
        return Err(HrnError::Tool(format!(
            "Chrome Native Messaging frame too large ({len} > {MAX_NATIVE_READ_BYTES} bytes)"
        )));
    }

    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|e| HrnError::Io(format!("native messaging payload read failed: {e}")))?;

    let val: Value = serde_json::from_slice(&payload)
        .map_err(|e| HrnError::Tool(format!("invalid Chrome Native Messaging JSON: {e}")))?;
    Ok(Some(val))
}

/// Write one Chrome Native Messaging frame (`u32` LE byte length followed by UTF-8 JSON) and flush.
///
/// # Errors
///
/// Returns an error when serialization fails, the encoded JSON exceeds
/// [`MAX_NATIVE_WRITE_BYTES`], or writing/flushing fails.
pub async fn write_native_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
) -> Result<()> {
    let payload = serde_json::to_vec(value)
        .map_err(|e| HrnError::Tool(format!("native messaging JSON encode failed: {e}")))?;
    write_raw_frame(writer, &payload).await
}

/// Write a host → extension message, automatically chunking into `cmd: "chunk"` envelopes when
/// the serialized JSON exceeds [`MAX_NATIVE_WRITE_BYTES`] (1 MiB).
///
/// # Errors
///
/// Returns an error if serialization or any frame write fails.
pub async fn write_native_message_chunked<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
) -> Result<()> {
    let text = serde_json::to_string(value)
        .map_err(|e| HrnError::Tool(format!("native messaging JSON encode failed: {e}")))?;
    if text.len() <= MAX_NATIVE_WRITE_BYTES {
        return write_raw_frame(writer, text.as_bytes()).await;
    }

    let chunk_id = CHUNK_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut start = 0usize;
    let mut index = 0usize;
    while start < text.len() {
        let mut end = (start + NATIVE_CHUNK_BYTES).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        let slice = text.get(start..end).unwrap_or_default();
        let last = end >= text.len();
        let envelope = json!({
            "cmd": "chunk",
            "chunkId": chunk_id,
            "index": index,
            "last": last,
            "data": slice,
        });
        write_native_message(writer, &envelope).await?;
        start = end;
        index += 1;
    }
    Ok(())
}

async fn write_raw_frame<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_NATIVE_WRITE_BYTES {
        return Err(HrnError::Tool(format!(
            "Chrome Native Messaging response too large ({} > {MAX_NATIVE_WRITE_BYTES} bytes)",
            payload.len()
        )));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| HrnError::Tool("native messaging payload length overflow".to_owned()))?;
    writer
        .write_all(&len.to_le_bytes())
        .await
        .map_err(|e| HrnError::Io(format!("native messaging header write failed: {e}")))?;
    writer
        .write_all(payload)
        .await
        .map_err(|e| HrnError::Io(format!("native messaging payload write failed: {e}")))?;
    writer
        .flush()
        .await
        .map_err(|e| HrnError::Io(format!("native messaging flush failed: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::serde_json::json;

    #[tokio::test]
    async fn native_messaging_roundtrips_json_and_eof() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let msg1 = json!({"id": 1, "cmd": "list_tabs"});
        let msg2 = json!({"id": 2, "cmd": "open_tab", "url": "https://example.test"});

        write_native_message(&mut client, &msg1)
            .await
            .expect("write 1");
        write_native_message(&mut client, &msg2)
            .await
            .expect("write 2");
        drop(client);

        let r1 = read_native_message(&mut server)
            .await
            .expect("read 1")
            .expect("some 1");
        let r2 = read_native_message(&mut server)
            .await
            .expect("read 2")
            .expect("some 2");
        let eof = read_native_message(&mut server).await.expect("eof");
        assert_eq!(r1, msg1);
        assert_eq!(r2, msg2);
        assert!(eof.is_none());
    }

    #[tokio::test]
    async fn native_messaging_reads_large_extension_reply_over_1mib() {
        let (mut ext, mut host) = tokio::io::duplex(2 * 1024 * 1024);
        let big_b64 = "A".repeat(1_250_000);
        let msg = json!({"id": 7, "ok": true, "result": {"data": big_b64}});
        let bytes = serde_json::to_vec(&msg).expect("encode");
        assert!(bytes.len() > MAX_NATIVE_WRITE_BYTES);

        let writer = tokio::spawn(async move {
            let len = u32::try_from(bytes.len()).expect("u32");
            ext.write_all(&len.to_le_bytes()).await.expect("hdr");
            ext.write_all(&bytes).await.expect("body");
            ext.flush().await.expect("flush");
        });

        let decoded = read_native_message(&mut host)
            .await
            .expect("read >1MiB")
            .expect("some");
        writer.await.expect("writer");
        assert_eq!(
            decoded
                .pointer("/result/data")
                .and_then(Value::as_str)
                .map(str::len),
            Some(1_250_000)
        );
    }

    #[tokio::test]
    async fn native_messaging_chunks_oversized_host_write() {
        let (mut host, mut ext) = tokio::io::duplex(2 * 1024 * 1024);
        let huge_expr = "x".repeat(1_150_000);
        let msg = json!({"id": 9, "cmd": "cdp", "method": "Runtime.evaluate", "params": {"expression": huge_expr}});

        let writer = tokio::spawn(async move {
            write_native_message_chunked(&mut host, &msg)
                .await
                .expect("chunked write");
        });

        let mut chunks = Vec::new();
        while let Some(frame) = read_native_message(&mut ext).await.expect("read chunk") {
            assert_eq!(frame.get("cmd").and_then(Value::as_str), Some("chunk"));
            let last = frame.get("last").and_then(Value::as_bool).unwrap_or(false);
            chunks.push(
                frame
                    .get("data")
                    .and_then(Value::as_str)
                    .expect("data")
                    .to_owned(),
            );
            if last {
                break;
            }
        }
        writer.await.expect("writer");
        assert!(
            chunks.len() >= 2,
            "expected >=2 chunks, got {}",
            chunks.len()
        );
        let reassembled: Value = serde_json::from_str(&chunks.concat()).expect("reassemble json");
        assert_eq!(reassembled.get("id").and_then(Value::as_i64), Some(9));
    }

    #[tokio::test]
    async fn native_messaging_rejects_oversized_frame() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let huge_len = u32::try_from(MAX_NATIVE_READ_BYTES + 1).expect("u32");
        client
            .write_all(&huge_len.to_le_bytes())
            .await
            .expect("write header");
        drop(client);

        let err = read_native_message(&mut server)
            .await
            .expect_err("must reject oversized frame");
        assert!(err.to_string().contains("too large"), "{err}");
    }
}
