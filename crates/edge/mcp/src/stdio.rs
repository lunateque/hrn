//! Stdio MCP transport backed by `hrn-edge-proc` (`process-wrap` Job Object / `ProcessGroup`).

use crate::protocol::{
    InitializeResult, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, initialize_params,
};
use hrn_edge_proc::build_stdio_command;
use hrn_kernel::{CancelFlag, HrnError, Result};
use process_wrap::tokio::ChildWrapper;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::Mutex;

/// Maximum retained stderr bytes kept for error diagnostics.
const STDERR_CAP: usize = 4096;
/// Maximum bytes accumulated for a single JSON-RPC response line (4 MiB) to prevent OOM.
const MAX_JSONRPC_LINE_BYTES: usize = 4 * 1024 * 1024;

struct StdioSpec {
    server_name: String,
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
    timeout: Duration,
}

struct LiveChild {
    child: Box<dyn ChildWrapper>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<StdMutex<String>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

impl LiveChild {
    fn spawn(spec: &StdioSpec) -> Result<Self> {
        let mut wrap =
            build_stdio_command(&spec.command, &spec.args, spec.cwd.as_deref(), &spec.env);
        let mut child = wrap.spawn().map_err(|e| {
            HrnError::Io(format!(
                "MCP `{}`: spawn `{}` failed: {e}",
                spec.server_name, spec.command
            ))
        })?;
        let stdin = child.stdin().take().ok_or_else(|| {
            HrnError::Io(format!("MCP `{}`: missing stdin pipe", spec.server_name))
        })?;
        let stdout = child.stdout().take().ok_or_else(|| {
            HrnError::Io(format!("MCP `{}`: missing stdout pipe", spec.server_name))
        })?;
        let stderr_tail = Arc::new(StdMutex::new(String::new()));
        let stderr_task = child.stderr().take().map(|mut err_pipe| {
            let buf_ref = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                let mut chunk = [0u8; 2048];
                while let Ok(n) = err_pipe.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    let text = String::from_utf8_lossy(chunk.get(..n).unwrap_or_default());
                    let mut guard = buf_ref
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push_str(&text);
                    if guard.len() > STDERR_CAP {
                        let drop_to = guard.ceil_char_boundary(guard.len() - STDERR_CAP);
                        guard.drain(..drop_to);
                    }
                }
            })
        });
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_tail,
            stderr_task,
        })
    }

    fn stderr_excerpt(&self) -> String {
        let guard = self
            .stderr_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let trimmed = guard.trim();
        if trimmed.is_empty() {
            String::new()
        } else {
            format!(" (stderr: {trimmed})")
        }
    }

    fn kill_now(&mut self) {
        let _ = self.child.start_kill();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

impl Drop for LiveChild {
    fn drop(&mut self) {
        self.kill_now();
    }
}

/// Long-lived stdio MCP transport with transparent single reconnect on pipe EOF/failure.
pub(crate) struct StdioMcpTransport {
    spec: StdioSpec,
    next_id: AtomicU64,
    conn: Mutex<Option<LiveChild>>,
}

impl StdioMcpTransport {
    pub(crate) async fn connect(
        server_name: String,
        command: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<(Self, InitializeResult)> {
        let transport = Self {
            spec: StdioSpec {
                server_name,
                command,
                args,
                cwd,
                env,
                timeout,
            },
            next_id: AtomicU64::new(1),
            conn: Mutex::new(None),
        };
        let mut guard = transport.conn.lock().await;
        let init = transport.spawn_and_initialize(&mut guard).await?;
        drop(guard);
        Ok((transport, init))
    }

    async fn spawn_and_initialize(&self, slot: &mut Option<LiveChild>) -> Result<InitializeResult> {
        if let Some(mut old) = slot.take() {
            old.kill_now();
        }
        let mut child = LiveChild::spawn(&self.spec)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(id, "initialize", Some(initialize_params()));
        let value = self
            .exchange_locked(&mut child, &req, &CancelFlag::new())
            .await?;
        let init: InitializeResult = serde_json::from_value(value).map_err(|e| {
            HrnError::Tool(format!(
                "MCP `{}`: invalid initialize response: {e}",
                self.spec.server_name
            ))
        })?;
        let notif = JsonRpcNotification::new("notifications/initialized", None);
        write_json_line(&mut child.stdin, &notif, &self.spec.server_name).await?;
        *slot = Some(child);
        Ok(init)
    }

    /// Execute one JSON-RPC request; retries once after respawning if the pipe broke.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        cancel: &CancelFlag,
    ) -> Result<Value> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            self.spawn_and_initialize(&mut guard).await?;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(id, method, params.clone());
        let first_attempt = if let Some(child) = guard.as_mut() {
            self.exchange_locked(child, &req, cancel).await
        } else {
            Err(HrnError::Io("MCP stdio child missing".into()))
        };
        match first_attempt {
            Ok(val) => Ok(val),
            Err(HrnError::Io(_)) if !cancel.is_cancelled() => {
                // Broken pipe or unexpected EOF: respawn once and retry.
                self.spawn_and_initialize(&mut guard).await?;
                let retry_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                let retry_req = JsonRpcRequest::new(retry_id, method, params);
                let child = guard
                    .as_mut()
                    .ok_or_else(|| HrnError::Io("MCP stdio child missing after respawn".into()))?;
                self.exchange_locked(child, &retry_req, cancel).await
            }
            Err(other) => Err(other),
        }
    }

    async fn exchange_locked(
        &self,
        child: &mut LiveChild,
        req: &JsonRpcRequest,
        cancel: &CancelFlag,
    ) -> Result<Value> {
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        write_json_line(&mut child.stdin, req, &self.spec.server_name).await?;
        let deadline = tokio::time::Instant::now() + self.spec.timeout;
        let expected_id = &req.id;
        let mut buf = Vec::new();

        loop {
            buf.clear();
            let poll_res = tokio::select! {
                biased;
                () = cancel.cancelled() => PollResult::Cancelled,
                res = read_bounded_line(&mut child.stdout, &mut buf, MAX_JSONRPC_LINE_BYTES) => {
                    PollResult::Read(res)
                }
                () = tokio::time::sleep_until(deadline) => PollResult::TimedOut,
            };
            let read_res = match poll_res {
                PollResult::Read(res) => res,
                PollResult::Cancelled => return Err(HrnError::Cancelled),
                PollResult::TimedOut => {
                    return Err(HrnError::Tool(format!(
                        "MCP `{}`: `{}` timed out after {}s{}",
                        self.spec.server_name,
                        req.method,
                        self.spec.timeout.as_secs(),
                        child.stderr_excerpt()
                    )));
                }
            };
            match read_res.map_err(|e| {
                HrnError::Io(format!(
                    "MCP `{}`: stdout read failed: {e}{}",
                    self.spec.server_name,
                    child.stderr_excerpt()
                ))
            })? {
                BoundedLine::Eof => {
                    return Err(HrnError::Io(format!(
                        "MCP `{}`: server closed stdout unexpectedly{}",
                        self.spec.server_name,
                        child.stderr_excerpt()
                    )));
                }
                BoundedLine::TooLong => {
                    child.kill_now();
                    return Err(HrnError::Tool(format!(
                        "MCP `{}`: stdout JSON-RPC line exceeded {MAX_JSONRPC_LINE_BYTES}-byte limit",
                        self.spec.server_name
                    )));
                }
                BoundedLine::Line => {}
            }
            let current = String::from_utf8_lossy(&buf);
            let trimmed = current.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(trimmed) else {
                // Skip non-JSON diagnostic noise or unsolicited server notifications.
                continue;
            };
            if resp.id.as_ref() != Some(expected_id) {
                continue;
            }
            if let Some(err) = resp.error {
                return Err(HrnError::Tool(format!(
                    "MCP `{}`: {err}",
                    self.spec.server_name
                )));
            }
            return Ok(resp.result.unwrap_or(Value::Null));
        }
    }

    pub(crate) async fn shutdown(&self) {
        let mut guard = self.conn.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill_now();
        }
    }
}

enum PollResult {
    Read(std::io::Result<BoundedLine>),
    TimedOut,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoundedLine {
    Eof,
    Line,
    TooLong,
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<BoundedLine> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                BoundedLine::Eof
            } else {
                BoundedLine::Line
            });
        }
        if let Some(nl) = available.iter().position(|&b| b == b'\n') {
            let take = nl + 1;
            if buf.len().saturating_add(take) > max_bytes {
                reader.consume(take);
                return Ok(BoundedLine::TooLong);
            }
            buf.extend_from_slice(available.get(..take).unwrap_or_default());
            reader.consume(take);
            return Ok(BoundedLine::Line);
        }
        let len = available.len();
        if buf.len().saturating_add(len) > max_bytes {
            reader.consume(len);
            return Ok(BoundedLine::TooLong);
        }
        buf.extend_from_slice(available);
        reader.consume(len);
    }
}

async fn write_json_line<T: serde::Serialize>(
    stdin: &mut ChildStdin,
    payload: &T,
    server_name: &str,
) -> Result {
    let mut encoded = serde_json::to_string(payload)
        .map_err(|e| HrnError::Internal(format!("MCP `{server_name}`: encode JSON-RPC: {e}")))?;
    encoded.push('\n');
    stdin
        .write_all(encoded.as_bytes())
        .await
        .map_err(|e| HrnError::Io(format!("MCP `{server_name}`: write stdin: {e}")))?;
    stdin
        .flush()
        .await
        .map_err(|e| HrnError::Io(format!("MCP `{server_name}`: flush stdin: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cache::McpCache;
    use crate::client::McpClient;
    use crate::config::{McpServerConfig, McpTransport};
    use crate::manager::McpManager;
    use crate::protocol::{CallToolResult, McpToolSpec, default_object_schema};
    use crate::server::{McpServerHandler, serve_stdio};
    use hrn_edge_http::HttpClient;
    use hrn_kernel::{BoxFuture, CancelFlag, Result, ToolApproval, ToolEffect};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    struct TestServer;

    impl McpServerHandler for TestServer {
        fn server_name(&self) -> &'static str {
            "test-mcp"
        }

        fn list_tools(&self) -> Vec<McpToolSpec> {
            vec![
                McpToolSpec {
                    name: "add".to_owned(),
                    description: Some("Add two numbers".to_owned()),
                    input_schema: default_object_schema(),
                },
                McpToolSpec {
                    name: "secret".to_owned(),
                    description: Some("Filtered out".to_owned()),
                    input_schema: default_object_schema(),
                },
            ]
        }

        fn call_tool(&self, name: &str, arguments: Value) -> BoxFuture<Result<CallToolResult>> {
            let name = name.to_owned();
            Box::pin(async move {
                if name == "add" {
                    let a = arguments.get("a").and_then(Value::as_i64).unwrap_or(0);
                    let b = arguments.get("b").and_then(Value::as_i64).unwrap_or(0);
                    Ok(CallToolResult::text((a + b).to_string()))
                } else {
                    Ok(CallToolResult::error(format!("unknown `{name}`")))
                }
            })
        }
    }

    /// Child process entrypoint re-execed by `stdio_roundtrip_over_real_pipes`.
    #[tokio::test]
    async fn stdio_server_helper() {
        if std::env::var("HRN_MCP_TEST_SERVER").is_err() {
            return;
        }
        serve_stdio(&TestServer).await.expect("serve stdio");
    }

    fn self_mcp_config() -> McpServerConfig {
        McpServerConfig {
            name: "demo".to_owned(),
            transport: McpTransport::Stdio {
                command: std::env::current_exe()
                    .expect("current exe")
                    .to_string_lossy()
                    .into_owned(),
                args: vec![
                    "--exact".to_owned(),
                    "stdio::tests::stdio_server_helper".to_owned(),
                    "--nocapture".to_owned(),
                ],
                cwd: None,
                env: BTreeMap::from([("HRN_MCP_TEST_SERVER".to_owned(), "1".to_owned())]),
            },
            enabled: true,
            allow_tools: Vec::new(),
            deny_tools: vec!["secret".to_owned()],
            timeout_secs: 15,
            auto_approve: true,
            read_only: true,
            source: "test".to_owned(),
        }
    }

    #[tokio::test]
    async fn stdio_roundtrip_over_real_pipes() {
        let http = HttpClient::new().expect("http");
        let client = McpClient::connect(self_mcp_config(), http.clone())
            .await
            .expect("connect");
        assert_eq!(client.init_result().server_info.name, "test-mcp");
        let _rtt = client.ping().await.expect("ping");

        let tools = client.list_tools().await.expect("list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools.first().map(|t| t.name.as_str()), Some("add"));

        let res = client
            .call_tool("add", json!({"a": 19, "b": 23}), &CancelFlag::new())
            .await
            .expect("call");
        assert!(!res.is_error);
        assert_eq!(res.render_text(), "42");
        client.shutdown().await;

        // Also test McpManager concurrent connect + graceful degradation for an offline server.
        let offline = McpServerConfig {
            name: "offline".to_owned(),
            transport: McpTransport::Stdio {
                command: "hrn-definitely-missing-mcp-binary-xyz".to_owned(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
            },
            enabled: true,
            allow_tools: Vec::new(),
            deny_tools: Vec::new(),
            timeout_secs: 5,
            auto_approve: true,
            read_only: false,
            source: "test".to_owned(),
        };
        let mgr = McpManager::open(vec![self_mcp_config(), offline], http, None).await;
        let loaded = mgr.tool_set(usize::MAX);
        assert_eq!(loaded.loaded.len(), 1);
        assert!(loaded.deferred.is_empty());
        let deferred = mgr.tool_set(0);
        assert!(deferred.loaded.is_empty());
        let entry = deferred.deferred.first().expect("deferred tool");
        assert_eq!(
            (entry.group.as_str(), entry.short_name.as_str()),
            ("demo", "add")
        );
        let tool = loaded.loaded.first().cloned().expect("tool");
        assert_eq!(tool.spec().name, "mcp__demo__add");
        assert_eq!(tool.approval(&json!({})), ToolApproval::read());
        assert_eq!(tool.effect(), ToolEffect::ReadOnly);
        assert_eq!(mgr.warnings().len(), 1);
        mgr.shutdown().await;

        // A cached tool list advertises the tools without starting the server.
        let dir = std::env::temp_dir().join(format!("hrn-mcp-mgr-{}", std::process::id()));
        let cache = McpCache::new(dir.join("cache.json"));
        let first = McpManager::open(
            vec![self_mcp_config()],
            HttpClient::new().expect("http"),
            Some(&cache),
        )
        .await;
        assert!(first.statuses().first().is_some_and(|s| s.connected));
        first.shutdown().await;
        let second = McpManager::open(
            vec![self_mcp_config()],
            HttpClient::new().expect("http"),
            Some(&cache),
        )
        .await;
        let status = second.statuses().first().expect("status");
        assert!(!status.connected && status.error.is_none() && status.tool_count == 1);
        assert_eq!(second.tool_set(usize::MAX).loaded.len(), 1);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn read_bounded_line_rejects_oversized_newline_free_stream() {
        use super::{BoundedLine, read_bounded_line};
        use tokio::io::BufReader;

        let ok_data = b"hello\nworld";
        let mut reader = BufReader::new(&ok_data[..]);
        let mut buf = Vec::new();
        assert_eq!(
            read_bounded_line(&mut reader, &mut buf, 16).await.unwrap(),
            BoundedLine::Line
        );
        assert_eq!(buf, b"hello\n");
        buf.clear();
        assert_eq!(
            read_bounded_line(&mut reader, &mut buf, 16).await.unwrap(),
            BoundedLine::Line
        );
        assert_eq!(buf, b"world");
        buf.clear();
        assert_eq!(
            read_bounded_line(&mut reader, &mut buf, 16).await.unwrap(),
            BoundedLine::Eof
        );

        let huge = vec![b'x'; 256];
        let mut huge_reader = BufReader::with_capacity(32, &huge[..]);
        buf.clear();
        assert_eq!(
            read_bounded_line(&mut huge_reader, &mut buf, 64)
                .await
                .unwrap(),
            BoundedLine::TooLong
        );
        assert!(buf.len() <= 64);
    }
}
