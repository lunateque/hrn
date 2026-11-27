//! Warm ACP v1 NDJSON stdio JSON-RPC 2.0 client for `agy acp`.

use super::config::AgyToolMode;
use super::wire::{
    AcpCapabilities, AcpPermissionRequest, AcpPromptResult, AcpSessionNewResult, AcpUpdate,
    MAX_NDJSON_LINE_BYTES, classify_jsonrpc_error, is_hrn_bridge_permission, parse_capabilities,
    parse_permission_request, parse_prompt_result, parse_session_new, parse_session_update,
    read_bounded_line,
};
use hrn_edge_proc::ChildWrapper;
use hrn_edge_proc::build_stdio_command;
use hrn_kernel::{HrnError, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

/// Process spawn parameters for launching an ACP server child.
#[derive(Clone, Debug)]
pub struct AcpSpawnSpec {
    /// Executable path (e.g. `agy.exe` or a mock binary in tests).
    pub binary: PathBuf,
    /// Arguments (e.g. `["acp", "--stdio", "--agent", "..."]`).
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Extra environment variables (always includes merged `NO_PROXY`/`no_proxy`).
    pub env: BTreeMap<String, String>,
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
type UpdateSubs = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<AcpUpdate>>>>;
type StderrTail = Arc<Mutex<VecDeque<String>>>;

const STDERR_TAIL_LINES: usize = 8;

/// Warm ACP v1 stdio JSON-RPC 2.0 client multiplexing multiple sessions over one `agy acp` process.
pub struct AcpClient {
    child: Mutex<Option<Box<dyn ChildWrapper>>>,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    next_id: AtomicU64,
    pending: PendingMap,
    subscribers: UpdateSubs,
    stderr_tail: StderrTail,
    alive: Arc<AtomicBool>,
    capabilities: AcpCapabilities,
    reader_task: tokio::task::JoinHandle<()>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        self.reader_task.abort();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        if let Some(mut child) = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = child.start_kill();
        }
    }
}

impl AcpClient {
    /// Spawn the ACP child process and run the ACP v1 `initialize` handshake.
    pub async fn spawn_and_initialize(
        spec: AcpSpawnSpec,
        tool_mode: AgyToolMode,
        init_timeout: Duration,
    ) -> Result<Self> {
        let mut wrap = build_stdio_command(
            &spec.binary.to_string_lossy(),
            &spec.args,
            Some(&spec.cwd),
            &spec.env,
        );
        let mut child = wrap
            .spawn()
            .map_err(|e| HrnError::Io(format!("spawn `{}`: {e}", spec.binary.display())))?;

        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| HrnError::Io("ACP child stdin pipe unavailable".to_owned()))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| HrnError::Io("ACP child stdout pipe unavailable".to_owned()))?;
        let stderr = child.stderr().take();

        let stdin = Arc::new(AsyncMutex::new(stdin));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let subscribers: UpdateSubs = Arc::new(Mutex::new(HashMap::new()));
        let stderr_tail: StderrTail =
            Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        let alive = Arc::new(AtomicBool::new(true));

        let stderr_task = stderr.map(|pipe| {
            let tail = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                let mut reader = BufReader::new(pipe);
                while let Ok(Some(line)) = read_bounded_line(&mut reader, 64 * 1024).await {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let clipped: String = trimmed.chars().take(240).collect();
                    let mut guard = tail
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if guard.len() >= STDERR_TAIL_LINES {
                        guard.pop_front();
                    }
                    guard.push_back(clipped);
                }
            })
        });

        let reader_task = tokio::spawn(run_reader_loop(
            stdout,
            Arc::clone(&stdin),
            Arc::clone(&pending),
            Arc::clone(&subscribers),
            Arc::clone(&alive),
            tool_mode,
        ));

        let mut client = Self {
            child: Mutex::new(Some(child)),
            stdin,
            next_id: AtomicU64::new(1),
            pending,
            subscribers,
            stderr_tail,
            alive,
            capabilities: AcpCapabilities::default(),
            reader_task,
            stderr_task,
        };

        let init_val = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": {
                        "name": "hrn",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
                init_timeout,
            )
            .await?;
        client.capabilities = parse_capabilities(&init_val);
        Ok(client)
    }

    /// Whether the underlying `agy acp` child stdout stream is still open and responsive.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Capabilities reported by `initialize`.
    pub const fn capabilities(&self) -> &AcpCapabilities {
        &self.capabilities
    }

    /// Invoke ACP `authenticate` for `method_id` (`oauth-personal`, `oauth-business`, ...).
    pub async fn authenticate(&self, method_id: &str, timeout: Duration) -> Result<Value> {
        self.request("authenticate", json!({ "methodId": method_id }), timeout)
            .await
    }

    /// Create a fresh ACP session (`session/new`).
    pub async fn session_new(
        &self,
        cwd: &Path,
        mcp_servers: &[Value],
        timeout: Duration,
    ) -> Result<AcpSessionNewResult> {
        let val = self
            .request(
                "session/new",
                json!({
                    "cwd": cwd.to_string_lossy(),
                    "mcpServers": mcp_servers,
                }),
                timeout,
            )
            .await?;
        parse_session_new(&val).ok_or_else(|| {
            provider_err(
                "ACP `session/new` response missing `sessionId`".to_owned(),
                false,
            )
        })
    }

    /// Resume an existing ACP session (`session/resume`).
    pub async fn session_resume(
        &self,
        session_id: &str,
        cwd: &Path,
        mcp_servers: &[Value],
        timeout: Duration,
    ) -> Result<Value> {
        self.request(
            "session/resume",
            json!({
                "sessionId": session_id,
                "cwd": cwd.to_string_lossy(),
                "mcpServers": mcp_servers,
            }),
            timeout,
        )
        .await
    }

    /// Fork an existing ACP session (`session/fork`) when supported by server capabilities.
    pub async fn session_fork(
        &self,
        session_id: &str,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<String> {
        let val = self
            .request(
                "session/fork",
                json!({
                    "sessionId": session_id,
                    "cwd": cwd.to_string_lossy(),
                }),
                timeout,
            )
            .await?;
        val.get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                provider_err(
                    "ACP `session/fork` response missing `sessionId`".to_owned(),
                    false,
                )
            })
    }

    /// Change a session configuration option (`model`, `thought_level`, `mode`).
    pub async fn session_set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
        timeout: Duration,
    ) -> Result<Value> {
        self.request(
            "session/set_config_option",
            json!({
                "sessionId": session_id,
                "configId": config_id,
                "value": value,
            }),
            timeout,
        )
        .await
    }

    /// Submit a non-blocking `session/prompt` request and return a receiver for its completion.
    pub async fn start_prompt(
        &self,
        session_id: &str,
        prompt_blocks: Vec<Value>,
    ) -> Result<oneshot::Receiver<Result<AcpPromptResult>>> {
        let (_id, rx) = self
            .send_request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": prompt_blocks,
                }),
            )
            .await?;
        let (tx, out_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mapped = match rx.await {
                Ok(Ok(val)) => Ok(parse_prompt_result(&val)),
                Ok(Err(err)) => Err(err),
                Err(_) => Err(provider_err(
                    "ACP process exited while prompt was in flight".to_owned(),
                    true,
                )),
            };
            let _ = tx.send(mapped);
        });
        Ok(out_rx)
    }

    /// Send an ACP `session/cancel` notification for an active session.
    pub async fn session_cancel(&self, session_id: &str) -> Result {
        self.send_notification("session/cancel", json!({ "sessionId": session_id }))
            .await
    }

    /// Close an ACP session cleanly (`session/close`).
    pub async fn session_close(&self, session_id: &str, timeout: Duration) -> Result {
        let _ = self
            .request("session/close", json!({ "sessionId": session_id }), timeout)
            .await?;
        Ok(())
    }

    /// Register a `session/update` subscriber channel for `session_id`.
    pub fn subscribe_updates(&self, session_id: &str) -> mpsc::UnboundedReceiver<AcpUpdate> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_owned(), tx);
        rx
    }

    /// Remove a `session/update` subscriber channel for `session_id`.
    pub fn unsubscribe_updates(&self, session_id: &str) {
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let (id, rx) = self.send_request(method, params).await?;
        let _guard = PendingGuard {
            pending: &self.pending,
            id,
        };
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(provider_err(
                format!(
                    "ACP connection closed waiting for `{method}`{}",
                    self.stderr_suffix()
                ),
                true,
            )),
            Err(_) => Err(provider_err(
                format!(
                    "ACP request `{method}` timed out after {}ms{}",
                    timeout.as_millis(),
                    self.stderr_suffix()
                ),
                true,
            )),
        }
    }

    fn stderr_suffix(&self) -> String {
        let guard = self
            .stderr_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_empty() {
            String::new()
        } else {
            format!(
                "; stderr: {}",
                guard.iter().cloned().collect::<Vec<_>>().join(" | ")
            )
        }
    }

    async fn send_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(u64, oneshot::Receiver<Result<Value>>)> {
        if !self.is_alive() {
            return Err(provider_err(
                format!("ACP child process is not running{}", self.stderr_suffix()),
                true,
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(err) = write_json_line(&self.stdin, &payload).await {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            return Err(err);
        }
        Ok((id, rx))
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result {
        if !self.is_alive() {
            return Ok(());
        }
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        write_json_line(&self.stdin, &payload).await
    }
}

async fn write_json_line(stdin: &Arc<AsyncMutex<ChildStdin>>, payload: &Value) -> Result {
    let mut bytes = serde_json::to_vec(payload)?;
    bytes.push(b'\n');
    let mut guard = stdin.lock().await;
    guard
        .write_all(&bytes)
        .await
        .map_err(|e| HrnError::Io(format!("write ACP stdin: {e}")))?;
    guard
        .flush()
        .await
        .map_err(|e| HrnError::Io(format!("flush ACP stdin: {e}")))?;
    Ok(())
}

async fn run_reader_loop(
    stdout: tokio::process::ChildStdout,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    pending: PendingMap,
    subscribers: UpdateSubs,
    alive: Arc<AtomicBool>,
    tool_mode: AgyToolMode,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let line = match read_bounded_line(&mut reader, MAX_NDJSON_LINE_BYTES).await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(err) => {
                let msg = format!("ACP stdout framing error: {err}");
                fail_all_pending(&pending, &msg, false);
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id");

        match (method, id) {
            (Some(method_name), Some(rpc_id)) => {
                handle_server_request(
                    &stdin,
                    method_name,
                    rpc_id.clone(),
                    msg.get("params").unwrap_or(&Value::Null),
                    tool_mode,
                )
                .await;
            }
            (Some("session/update"), None) => {
                if let Some(params) = msg.get("params")
                    && let Some((sid, update)) = parse_session_update(params)
                {
                    let sub = subscribers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&sid)
                        .cloned();
                    if let Some(tx) = sub {
                        let _ = tx.send(update);
                    }
                }
            }
            (None, Some(rpc_id)) => {
                if let Some(num_id) = rpc_id.as_u64() {
                    let waiter = pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&num_id);
                    if let Some(tx) = waiter {
                        let res = if let Some(err) = msg.get("error") {
                            let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32603);
                            let message = err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown ACP error");
                            Err(classify_jsonrpc_error(code, message, err.get("data")))
                        } else {
                            Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = tx.send(res);
                    }
                }
            }
            _ => {}
        }
    }

    alive.store(false, Ordering::SeqCst);
    fail_all_pending(&pending, "ACP child process terminated", true);
}

async fn handle_server_request(
    stdin: &Arc<AsyncMutex<ChildStdin>>,
    method: &str,
    rpc_id: Value,
    params: &Value,
    tool_mode: AgyToolMode,
) {
    if method == "session/request_permission" {
        let req = parse_permission_request(rpc_id.clone(), params);
        let reply = build_permission_reply(&req, tool_mode);
        let _ = write_json_line(stdin, &reply).await;
        return;
    }
    let reply = json!({
        "jsonrpc": "2.0",
        "id": rpc_id,
        "error": {
            "code": -32601,
            "message": format!("unsupported client method `{method}`")
        }
    });
    let _ = write_json_line(stdin, &reply).await;
}

/// Build the JSON-RPC reply to `session/request_permission` honoring `tool_mode` (fail-closed in `Hrn` mode).
pub fn build_permission_reply(req: &AcpPermissionRequest, tool_mode: AgyToolMode) -> Value {
    let allow = match tool_mode {
        AgyToolMode::Hrn => is_hrn_bridge_permission(req),
        AgyToolMode::Native => true,
    };
    let outcome = if allow {
        let opt_id = req.allow_option_id.as_deref().unwrap_or("allow_once");
        json!({ "outcome": "selected", "optionId": opt_id })
    } else if let Some(rej_id) = req.reject_option_id.as_deref() {
        json!({ "outcome": "selected", "optionId": rej_id })
    } else {
        json!({ "outcome": "cancelled" })
    };
    json!({
        "jsonrpc": "2.0",
        "id": req.rpc_id,
        "result": { "outcome": outcome }
    })
}

fn provider_err(message: String, retryable: bool) -> HrnError {
    HrnError::Provider {
        message,
        retryable,
        retry_after_secs: None,
    }
}

fn fail_all_pending(pending: &PendingMap, message: &str, retryable: bool) {
    let drained: Vec<_> = {
        let mut guard = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.drain().map(|(_, tx)| tx).collect()
    };
    for tx in drained {
        let _ = tx.send(Err(provider_err(message.to_owned(), retryable)));
    }
}

struct PendingGuard<'a> {
    pending: &'a PendingMap,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}
