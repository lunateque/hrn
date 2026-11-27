//! DAP base-protocol framing: `Content-Length: <n>\r\n\r\n<json>` over any byte stream.

use hrn_kernel::{HrnError, Result};
use serde_json::Value;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};

/// Largest message accepted from an adapter (a `variables` or `modules` body can be big).
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Read one message; `None` at a clean end of stream.
///
/// # Errors
///
/// A stream error, a header block without a valid `Content-Length`, an oversized frame or a
/// body that is not JSON.
pub async fn read_message<R>(reader: &mut R) -> Result<Option<Value>>
where
    R: AsyncBufRead + AsyncRead + Unpin,
{
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| HrnError::Io(format!("DAP read: {e}")))?;
        if n == 0 {
            return Ok(None);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().map_err(|e| {
                HrnError::Tool(format!("DAP: bad Content-Length `{}`: {e}", value.trim()))
            })?);
        }
    }
    let length =
        length.ok_or_else(|| HrnError::Tool("DAP: frame without Content-Length".into()))?;
    if length > MAX_FRAME_BYTES {
        return Err(HrnError::Tool(format!(
            "DAP: frame of {length} bytes exceeds the {MAX_FRAME_BYTES} byte limit"
        )));
    }
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| HrnError::Io(format!("DAP read body: {e}")))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| HrnError::Tool(format!("DAP: frame is not JSON: {e}")))
}

/// Write one message and flush.
///
/// # Errors
///
/// A stream error.
pub async fn write_message<W>(writer: &mut W, message: &Value) -> Result
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(message)?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    writer
        .write_all(&frame)
        .await
        .map_err(|e| HrnError::Io(format!("DAP write: {e}")))?;
    writer
        .flush()
        .await
        .map_err(|e| HrnError::Io(format!("DAP flush: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{BufReader, duplex};

    #[tokio::test]
    async fn frames_survive_arbitrary_chunking_and_utf8() {
        let (mut tx, rx) = duplex(8);
        let sent = vec![
            json!({"seq": 1, "type": "event", "event": "output", "body": {"output": "héllo → мир\n"}}),
            json!({"seq": 2, "type": "response", "success": true}),
        ];
        let to_send = sent.clone();
        let writer = tokio::spawn(async move {
            for message in &to_send {
                write_message(&mut tx, message).await.expect("write");
            }
        });
        let mut reader = BufReader::new(rx);
        for expected in &sent {
            assert_eq!(
                read_message(&mut reader).await.expect("read").as_ref(),
                Some(expected)
            );
        }
        writer.await.expect("writer");
        assert_eq!(read_message(&mut reader).await.expect("eof"), None);
    }

    #[tokio::test]
    async fn a_frame_without_content_length_is_an_error() {
        let mut reader = BufReader::new(&b"X-Other: 1\r\n\r\n{}"[..]);
        assert!(read_message(&mut reader).await.is_err());
    }
}
