//! JSON-RPC 2.0 `Content-Length` framed LSP transport over stdio or duplex streams.

use crate::uri::canonical_uri_key;
use hrn_edge_proc::{ChildWrapper, build_stdio_command};
use hrn_edge_rt::tokio;
use hrn_kernel::serde::{Deserialize, Serialize};
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{HrnError, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Notify, oneshot};

/// Maximum single LSP frame accepted (16 MiB).
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Maximum header lines permitted before the blank header terminator.
const MAX_HEADER_LINES: usize = 64;
/// Maximum bytes permitted in a single LSP header line.
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// One LSP diagnostic cached from `textDocument/publishDiagnostics`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(crate = "hrn_kernel::serde")]
pub struct DiagnosticItem {
    /// 0-based start line.
    pub line: u32,
    /// 0-based start character.
    pub col: u32,
    /// LSP severity: 1=Error, 2=Warning, 3=Info, 4=Hint.
    pub severity: u8,
    /// Optional source (e.g. `rustc`, `ts`, `pyright`).
    pub source: Option<String>,
    /// Optional diagnostic code.
    pub code: Option<String>,
    /// Diagnostic text.
    pub message: String,
}

#[derive(Default)]
struct DiagEntry {
    seq: u64,
    items: Vec<DiagnosticItem>,
}

type PendingMap = BTreeMap<i64, oneshot::Sender<Result<Value>>>;

/// Active connection to one LSP server.
pub(crate) struct LspClient {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    pending: Arc<Mutex<PendingMap>>,
    diagnostics: Arc<Mutex<BTreeMap<String, DiagEntry>>>,
    diag_notify: Arc<Notify>,
    next_id: AtomicI64,
    closed: Arc<AtomicBool>,
    reader_task: tokio::task::JoinHandle<()>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    _child: Option<Mutex<Box<dyn ChildWrapper>>>,
}

impl LspClient {
    /// Spawn an external LSP process inside an OS process-tree guard (Win32 Job Object / Unix PG).
    pub(crate) fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        env: &BTreeMap<String, String>,
        settings: Option<Value>,
    ) -> Result<Self> {
        let mut cmd = build_stdio_command(program, args, Some(cwd), env);
        let mut child = cmd
            .spawn()
            .map_err(|e| HrnError::Tool(format!("failed to spawn LSP `{program}`: {e}")))?;
        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| HrnError::Tool(format!("LSP `{program}` stdin unavailable")))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| HrnError::Tool(format!("LSP `{program}` stdout unavailable")))?;
        let stderr_task = child.stderr().take().map(|mut err| {
            tokio::spawn(async move {
                let mut sink = [0u8; 4096];
                while let Ok(n) = err.read(&mut sink).await {
                    if n == 0 {
                        break;
                    }
                }
            })
        });
        Ok(Self::from_parts(
            Box::new(stdout),
            Box::new(stdin),
            Some(child),
            stderr_task,
            settings,
        ))
    }

    /// Construct a client over arbitrary async streams (used by tests).
    #[cfg(test)]
    pub(crate) fn from_streams<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::from_parts(Box::new(reader), Box::new(writer), None, None, None)
    }

    fn from_parts(
        reader: Box<dyn AsyncRead + Unpin + Send>,
        writer: Box<dyn AsyncWrite + Unpin + Send>,
        child: Option<Box<dyn ChildWrapper>>,
        stderr_task: Option<tokio::task::JoinHandle<()>>,
        settings: Option<Value>,
    ) -> Self {
        let writer = Arc::new(Mutex::new(writer));
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(BTreeMap::new()));
        let diagnostics: Arc<Mutex<BTreeMap<String, DiagEntry>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let diag_notify = Arc::new(Notify::new());
        let diag_seq = Arc::new(AtomicU64::new(0));
        let closed = Arc::new(AtomicBool::new(false));

        let r_writer = Arc::clone(&writer);
        let r_pending = Arc::clone(&pending);
        let r_diag = Arc::clone(&diagnostics);
        let r_notify = Arc::clone(&diag_notify);
        let r_closed = Arc::clone(&closed);

        let reader_task = tokio::spawn(async move {
            reader_loop(
                reader, r_writer, r_pending, r_diag, r_notify, diag_seq, r_closed, settings,
            )
            .await;
        });

        Self {
            writer,
            pending,
            diagnostics,
            diag_notify,
            next_id: AtomicI64::new(1),
            closed,
            reader_task,
            stderr_task,
            _child: child.map(Mutex::new),
        }
    }

    /// Best-effort graceful shutdown notification before dropping the client.
    pub(crate) async fn shutdown(&self) {
        if !self.is_closed() {
            let _ = self.notify("exit", Value::Null).await;
            self.closed.store(true, Ordering::Relaxed);
        }
    }

    /// Whether the underlying server transport has closed.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Send a JSON-RPC request and await its result up to `timeout`.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        if self.is_closed() {
            return Err(HrnError::Tool("LSP server disconnected".to_owned()));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(err) = write_frame(&self.writer, &payload).await {
            self.pending.lock().await.remove(&id);
            return Err(err);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(HrnError::Tool(format!(
                "LSP server closed while awaiting `{method}`"
            ))),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(HrnError::Tool(format!(
                    "LSP request `{method}` timed out after {}s",
                    timeout.as_secs()
                )))
            }
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<()> {
        if self.is_closed() {
            return Err(HrnError::Tool("LSP server disconnected".to_owned()));
        }
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        write_frame(&self.writer, &payload).await
    }

    /// Current diagnostic publication sequence number for a URI.
    pub(crate) async fn diag_seq_for(&self, uri: &str) -> u64 {
        let key = canonical_uri_key(uri);
        self.diagnostics
            .lock()
            .await
            .get(&key)
            .map_or(0, |entry| entry.seq)
    }

    /// Wait up to `timeout` for a `publishDiagnostics` update newer than `after_seq`, then return
    /// the cached diagnostics for `uri`.
    pub(crate) async fn wait_diagnostics(
        &self,
        uri: &str,
        after_seq: u64,
        timeout: Duration,
    ) -> Vec<DiagnosticItem> {
        let key = canonical_uri_key(uri);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.diag_notify.notified();
            {
                let map = self.diagnostics.lock().await;
                if let Some(entry) = map.get(&key)
                    && entry.seq > after_seq
                {
                    return entry.items.clone();
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline || self.is_closed() {
                break;
            }
            if tokio::time::timeout(deadline - now, notified)
                .await
                .is_err()
            {
                break;
            }
        }
        self.diagnostics
            .lock()
            .await
            .get(&key)
            .map(|e| e.items.clone())
            .unwrap_or_default()
    }

    /// Return total cached diagnostics across all files.
    pub(crate) async fn total_diagnostics(&self) -> usize {
        self.diagnostics
            .lock()
            .await
            .values()
            .map(|e| e.items.len())
            .sum()
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        self.reader_task.abort();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

async fn write_frame(
    writer: &Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    payload: &Value,
) -> Result<()> {
    let body = serde_json::to_vec(payload)
        .map_err(|e| HrnError::Tool(format!("LSP JSON encode error: {e}")))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut guard = writer.lock().await;
    guard
        .write_all(header.as_bytes())
        .await
        .map_err(|e| HrnError::Tool(format!("LSP write header failed: {e}")))?;
    guard
        .write_all(&body)
        .await
        .map_err(|e| HrnError::Tool(format!("LSP write body failed: {e}")))?;
    guard
        .flush()
        .await
        .map_err(|e| HrnError::Tool(format!("LSP flush failed: {e}")))?;
    Ok(())
}

async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
) -> Result<usize> {
    buf.clear();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| HrnError::Tool(format!("LSP header read failed: {e}")))?;
        if available.is_empty() {
            return Ok(buf.len());
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let take = newline.map_or(available.len(), |i| i + 1);
        if buf.len().saturating_add(take) > MAX_HEADER_LINE_BYTES {
            return Err(HrnError::Tool(format!(
                "LSP header line exceeds {MAX_HEADER_LINE_BYTES} bytes"
            )));
        }
        if let Some(slice) = available.get(..take) {
            buf.extend_from_slice(slice);
        }
        reader.consume(take);
        if newline.is_some() {
            return Ok(buf.len());
        }
    }
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut raw_line = Vec::with_capacity(128);
    let mut saw_header = false;
    for _ in 0..=MAX_HEADER_LINES {
        let n = read_bounded_line(reader, &mut raw_line).await?;
        if n == 0 {
            if !saw_header {
                return Ok(None);
            }
            return Err(HrnError::Tool(
                "LSP truncated header block before body".to_owned(),
            ));
        }
        let line = String::from_utf8_lossy(&raw_line);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if saw_header {
                break;
            }
            continue;
        }
        saw_header = true;
        if let Some((k, v)) = trimmed.split_once(':')
            && k.trim().eq_ignore_ascii_case("Content-Length")
        {
            let parsed = v.trim().parse::<usize>().map_err(|_| {
                HrnError::Tool(format!("LSP invalid Content-Length header `{}`", v.trim()))
            })?;
            content_length = Some(parsed);
        }
    }
    let Some(len) = content_length else {
        return Err(HrnError::Tool(
            "LSP frame missing Content-Length header".to_owned(),
        ));
    };
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(HrnError::Tool(format!(
            "LSP Content-Length {len} outside 1..={MAX_FRAME_BYTES}"
        )));
    }
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .await
        .map_err(|e| HrnError::Tool(format!("LSP truncated frame body: {e}")))?;
    serde_json::from_slice(&buf)
        .map(Some)
        .map_err(|e| HrnError::Tool(format!("LSP malformed JSON payload: {e}")))
}

#[expect(
    clippy::too_many_arguments,
    reason = "background JSON-RPC reader task owns channel handles"
)]
async fn reader_loop(
    reader: Box<dyn AsyncRead + Unpin + Send>,
    writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    pending: Arc<Mutex<PendingMap>>,
    diagnostics: Arc<Mutex<BTreeMap<String, DiagEntry>>>,
    diag_notify: Arc<Notify>,
    diag_seq: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    settings: Option<Value>,
) {
    let mut buf = BufReader::new(reader);
    let mut exit_reason = "LSP server exited".to_owned();
    loop {
        let msg = match read_frame(&mut buf).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break,
            Err(err) => {
                exit_reason = err.to_string();
                break;
            }
        };
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id");

        match (method, id) {
            // Response to one of our requests.
            (None, Some(id_val)) => {
                if let Some(num) = id_val.as_i64()
                    && let Some(tx) = pending.lock().await.remove(&num)
                {
                    let res = if let Some(err) = msg.get("error") {
                        let text = err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("LSP error");
                        Err(HrnError::Tool(format!("LSP error: {text}")))
                    } else {
                        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(res);
                }
            }
            // Server-to-client request: reply benignly so the server never stalls.
            (Some(meth), Some(id_val)) => {
                let result = if meth == "workspace/configuration" {
                    let count = msg
                        .pointer("/params/items")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    let entry = settings.clone().unwrap_or_else(|| json!({}));
                    Value::Array(vec![entry; count])
                } else {
                    Value::Null
                };
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id_val,
                    "result": result,
                });
                let _ = write_frame(&writer, &reply).await;
            }
            // Server notification.
            (Some("textDocument/publishDiagnostics"), None) => {
                if let Some(params) = msg.get("params")
                    && let Some(uri) = params.get("uri").and_then(Value::as_str)
                {
                    let items = parse_diagnostics(params);
                    let seq = diag_seq.fetch_add(1, Ordering::Relaxed) + 1;
                    diagnostics
                        .lock()
                        .await
                        .insert(canonical_uri_key(uri), DiagEntry { seq, items });
                    diag_notify.notify_waiters();
                }
            }
            _ => {}
        }
    }
    closed.store(true, Ordering::Relaxed);
    let mut map = pending.lock().await;
    for (_, tx) in std::mem::take(&mut *map) {
        let _ = tx.send(Err(HrnError::Tool(exit_reason.clone())));
    }
    diag_notify.notify_waiters();
}

fn parse_diagnostics(params: &Value) -> Vec<DiagnosticItem> {
    let Some(arr) = params.get("diagnostics").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|d| {
            let line = u32::try_from(d.pointer("/range/start/line")?.as_u64()?).ok()?;
            let col = u32::try_from(d.pointer("/range/start/character")?.as_u64()?).ok()?;
            let severity = d
                .get("severity")
                .and_then(Value::as_u64)
                .and_then(|v| u8::try_from(v).ok())
                .unwrap_or(1);
            let source = d.get("source").and_then(Value::as_str).map(str::to_owned);
            let code = d.get("code").map(|c| match c {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
            let message = d.get("message")?.as_str()?.trim().to_owned();
            Some(DiagnosticItem {
                line,
                col,
                severity,
                source,
                code,
                message,
            })
        })
        .collect()
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    #[tokio::test]
    async fn read_frame_handles_clean_eof_non_utf8_headers_and_malformed_frames() {
        let mut empty = BufReader::new(&b""[..]);
        assert!(read_frame(&mut empty).await.expect("eof").is_none());

        let mut wire = Vec::new();
        wire.extend_from_slice(b"X-Bin: \xff\xfe\r\ncontent-length: 15\r\n\r\n{\"jsonrpc\":\"2\"}");
        let mut valid = BufReader::new(wire.as_slice());
        let msg = read_frame(&mut valid).await.expect("valid").expect("frame");
        assert_eq!(msg["jsonrpc"], "2");

        let mut bad_json = BufReader::new(&b"Content-Length: 4\r\n\r\n{bad"[..]);
        assert!(read_frame(&mut bad_json).await.is_err());

        let mut missing_len = BufReader::new(&b"X-Other: 1\r\n\r\n{}"[..]);
        assert!(read_frame(&mut missing_len).await.is_err());

        let mut huge = BufReader::new(&b"Content-Length: 999999999\r\n\r\n"[..]);
        assert!(read_frame(&mut huge).await.is_err());

        let long_hdr = vec![b'A'; MAX_HEADER_LINE_BYTES + 16];
        let mut too_long = BufReader::new(long_hdr.as_slice());
        assert!(read_frame(&mut too_long).await.is_err());
    }
}
