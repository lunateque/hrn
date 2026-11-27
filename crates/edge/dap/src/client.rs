//! One DAP connection: request/response correlation, events and reverse requests over a child's
//! stdio or a TCP socket.

use crate::frame::{read_message, write_message};
use hrn_edge_proc::{ChildWrapper, build_stdio_command};
use hrn_kernel::{BoxFuture, HrnError, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio::task::JoinHandle;

/// What an adapter's stderr (and stdout, when it is not the protocol channel) keeps for error
/// messages.
const LOG_TAIL_BYTES: usize = 16 * 1024;
/// How often a freshly spawned TCP adapter is probed until it listens.
const CONNECT_POLL: Duration = Duration::from_millis(50);

/// The receiving end of a connection: whatever the adapter sends that is not a response.
pub trait Inbound: Send + Sync + 'static {
    /// An event (`stopped`, `output`, …). Runs in the reader task: it must not block.
    fn event(&self, name: &str, body: Value);

    /// A request from the adapter (`runInTerminal`, `startDebugging`); the answer goes back to it.
    fn reverse_request(&self, command: String, args: Value) -> BoxFuture<Result<Value>>;

    /// The connection ended (adapter exit or socket close).
    fn closed(&self);
}

/// How to start an adapter process.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    /// Executable (resolved against `PATH`/`PATHEXT` by the process edge).
    pub program: String,
    /// Arguments, already substituted.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Extra environment.
    pub env: BTreeMap<String, String>,
}

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value>>>>>;
type Writer = Arc<AsyncMutex<Box<dyn AsyncWrite + Unpin + Send>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A request in flight; resolved by [`PendingResponse::wait`].
pub struct PendingResponse {
    seq: i64,
    command: String,
    rx: oneshot::Receiver<Result<Value>>,
    pending: PendingMap,
}

impl Drop for PendingResponse {
    fn drop(&mut self) {
        // Abandoned (timed out or cancelled) before the answer arrived: stop the reader from
        // holding the body for a waiter that is gone.
        lock(&self.pending).remove(&self.seq);
    }
}

impl PendingResponse {
    /// The response body.
    ///
    /// # Errors
    ///
    /// The adapter's error response, a closed connection, or the timeout.
    pub async fn wait(mut self, timeout: Duration) -> Result<Value> {
        match tokio::time::timeout(timeout, &mut self.rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(HrnError::Tool(format!(
                "DAP `{}`: connection dropped before the response",
                self.command
            ))),
            Err(_) => {
                lock(&self.pending).remove(&self.seq);
                Err(HrnError::Tool(format!(
                    "DAP `{}` timed out after {}s",
                    self.command,
                    timeout.as_secs()
                )))
            }
        }
    }
}

/// A live connection to a debug adapter.
pub struct DapClient {
    writer: Writer,
    pending: PendingMap,
    next_seq: Arc<AtomicI64>,
    alive: Arc<AtomicBool>,
    log: Arc<Mutex<String>>,
    port: Option<u16>,
    tasks: Vec<JoinHandle<()>>,
    _child: Option<Mutex<Box<dyn ChildWrapper>>>,
}

impl Drop for DapClient {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn push_log(log: &Mutex<String>, chunk: &[u8]) {
    let mut text = lock(log);
    text.push_str(&String::from_utf8_lossy(chunk));
    if text.len() > LOG_TAIL_BYTES {
        let cut = text.ceil_char_boundary(text.len() - LOG_TAIL_BYTES);
        text.drain(..cut);
    }
}

fn drain(
    mut pipe: impl AsyncRead + Unpin + Send + 'static,
    log: Arc<Mutex<String>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut buf).await {
            if n == 0 {
                break;
            }
            if let Some(chunk) = buf.get(..n) {
                push_log(&log, chunk);
            }
        }
    })
}

impl DapClient {
    /// A client over arbitrary streams; `child` is kept alive (and killed on drop) with it.
    pub fn from_streams(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
        child: Option<Box<dyn ChildWrapper>>,
        port: Option<u16>,
        log: Arc<Mutex<String>>,
        mut tasks: Vec<JoinHandle<()>>,
        inbound: Arc<dyn Inbound>,
    ) -> Self {
        let writer: Writer = Arc::new(AsyncMutex::new(Box::new(writer)));
        let pending: PendingMap = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let next_seq = Arc::new(AtomicI64::new(1));
        tasks.push(tokio::spawn(read_loop(
            BufReader::new(reader),
            Arc::clone(&writer),
            Arc::clone(&pending),
            Arc::clone(&next_seq),
            Arc::clone(&alive),
            Arc::clone(&log),
            inbound,
        )));
        Self {
            writer,
            pending,
            next_seq,
            alive,
            log,
            port,
            tasks,
            _child: child.map(Mutex::new),
        }
    }

    /// Start an adapter that speaks DAP on its stdin/stdout.
    ///
    /// # Errors
    ///
    /// When the process cannot be spawned.
    pub fn spawn_stdio(spec: &SpawnSpec, inbound: Arc<dyn Inbound>) -> Result<Self> {
        let mut child = spawn_child(spec)?;
        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| HrnError::Tool(format!("`{}`: stdin unavailable", spec.program)))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| HrnError::Tool(format!("`{}`: stdout unavailable", spec.program)))?;
        let log = Arc::new(Mutex::new(String::new()));
        let tasks = child
            .stderr()
            .take()
            .map(|err| drain(err, Arc::clone(&log)))
            .into_iter()
            .collect();
        Ok(Self::from_streams(
            stdout,
            stdin,
            Some(child),
            None,
            log,
            tasks,
            inbound,
        ))
    }

    /// Start an adapter that listens on a loopback TCP port (`${port}` in `spec.args` is replaced
    /// by the port) and connect to it.
    ///
    /// # Errors
    ///
    /// When the process cannot be spawned, exits early, or does not listen within `timeout`.
    pub async fn spawn_tcp(
        spec: &SpawnSpec,
        timeout: Duration,
        inbound: Arc<dyn Inbound>,
    ) -> Result<Self> {
        let port = free_port().await?;
        let mut spec = spec.clone();
        for arg in &mut spec.args {
            *arg = arg.replace("${port}", &port.to_string());
        }
        let mut child = spawn_child(&spec)?;
        let log = Arc::new(Mutex::new(String::new()));
        let mut tasks = Vec::new();
        if let Some(out) = child.stdout().take() {
            tasks.push(drain(out, Arc::clone(&log)));
        }
        if let Some(err) = child.stderr().take() {
            tasks.push(drain(err, Arc::clone(&log)));
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(stream) => {
                    return Ok(Self::over_tcp(
                        stream,
                        Some(child),
                        port,
                        log,
                        tasks,
                        inbound,
                    ));
                }
                Err(e) => {
                    let exited = child
                        .inner_mut()
                        .try_wait()
                        .map_err(|e| HrnError::Io(format!("`{}`: {e}", spec.program)))?;
                    if let Some(status) = exited {
                        return Err(HrnError::Tool(format!(
                            "`{}` exited ({status}) before listening on port {port}{}",
                            spec.program,
                            tail_suffix(&log)
                        )));
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(HrnError::Tool(format!(
                            "`{}` did not listen on port {port} within {}s ({e}){}",
                            spec.program,
                            timeout.as_secs(),
                            tail_suffix(&log)
                        )));
                    }
                    tokio::time::sleep(CONNECT_POLL).await;
                }
            }
        }
    }

    /// Connect to an adapter that is already listening.
    ///
    /// # Errors
    ///
    /// When the connection is refused or times out.
    pub async fn connect_tcp(
        host: &str,
        port: u16,
        timeout: Duration,
        inbound: Arc<dyn Inbound>,
    ) -> Result<Self> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| HrnError::Tool(format!("connect {host}:{port} timed out")))?
            .map_err(|e| HrnError::Io(format!("connect {host}:{port}: {e}")))?;
        Ok(Self::over_tcp(
            stream,
            None,
            port,
            Arc::default(),
            Vec::new(),
            inbound,
        ))
    }

    fn over_tcp(
        stream: TcpStream,
        child: Option<Box<dyn ChildWrapper>>,
        port: u16,
        log: Arc<Mutex<String>>,
        tasks: Vec<JoinHandle<()>>,
        inbound: Arc<dyn Inbound>,
    ) -> Self {
        // Nagle only delays small request frames; failing to disable it is harmless.
        let _ = stream.set_nodelay(true);
        let (reader, writer) = stream.into_split();
        Self::from_streams(reader, writer, child, Some(port), log, tasks, inbound)
    }

    /// The TCP port of the adapter, for adapters that accept child connections.
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// Whether the connection is still open.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// The tail of what the adapter wrote to stderr.
    pub fn log_tail(&self) -> String {
        lock(&self.log).clone()
    }

    /// Send a request without waiting for its response.
    ///
    /// # Errors
    ///
    /// When the connection is closed or the write fails.
    pub async fn send(&self, command: &str, arguments: Value) -> Result<PendingResponse> {
        if !self.is_alive() {
            return Err(HrnError::Tool(format!(
                "DAP `{command}`: the adapter connection is closed{}",
                tail_suffix(&self.log)
            )));
        }
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(seq, tx);
        let mut message = json!({"seq": seq, "type": "request", "command": command});
        if !arguments.is_null()
            && let Some(map) = message.as_object_mut()
        {
            map.insert("arguments".to_owned(), arguments);
        }
        let written = write_message(&mut *self.writer.lock().await, &message).await;
        if let Err(e) = written {
            lock(&self.pending).remove(&seq);
            return Err(e);
        }
        Ok(PendingResponse {
            seq,
            command: command.to_owned(),
            rx,
            pending: Arc::clone(&self.pending),
        })
    }

    /// Send a request and wait for its response body.
    ///
    /// # Errors
    ///
    /// See [`DapClient::send`] and [`PendingResponse::wait`].
    pub async fn request(
        &self,
        command: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.send(command, arguments).await?.wait(timeout).await
    }
}

fn tail_suffix(log: &Mutex<String>) -> String {
    let text = lock(log);
    let text = text.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!("\nadapter output:\n{text}")
    }
}

fn spawn_child(spec: &SpawnSpec) -> Result<Box<dyn ChildWrapper>> {
    build_stdio_command(&spec.program, &spec.args, Some(&spec.cwd), &spec.env)
        .spawn()
        .map_err(|e| HrnError::Tool(format!("failed to start `{}`: {e}", spec.program)))
}

async fn free_port() -> Result<u16> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| HrnError::Io(format!("reserve a loopback port: {e}")))?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| HrnError::Io(format!("reserve a loopback port: {e}")))
}

fn response_error(command: &str, message: &Value) -> HrnError {
    let text = message
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("request failed");
    let detail = message
        .pointer("/body/error/format")
        .and_then(Value::as_str)
        .filter(|d| *d != text);
    HrnError::Tool(match detail {
        Some(d) => format!("DAP `{command}`: {text}: {d}"),
        None => format!("DAP `{command}`: {text}"),
    })
}

async fn read_loop(
    mut reader: BufReader<impl AsyncRead + Unpin + Send + 'static>,
    writer: Writer,
    pending: PendingMap,
    next_seq: Arc<AtomicI64>,
    alive: Arc<AtomicBool>,
    log: Arc<Mutex<String>>,
    inbound: Arc<dyn Inbound>,
) {
    let end = loop {
        match read_message(&mut reader).await {
            Ok(Some(message)) => dispatch(&message, &writer, &pending, &next_seq, &inbound),
            Ok(None) => break None,
            Err(e) => break Some(e),
        }
    };
    alive.store(false, Ordering::SeqCst);
    let reason = match end {
        Some(e) => format!("DAP connection failed: {e}"),
        None => "the adapter closed the connection".to_owned(),
    };
    let senders: Vec<_> = lock(&pending).drain().map(|(_, tx)| tx).collect();
    let suffix = tail_suffix(&log);
    for tx in senders {
        // The waiter may have timed out and gone; nobody is left to tell.
        let _ = tx.send(Err(HrnError::Tool(format!("{reason}{suffix}"))));
    }
    inbound.closed();
}

fn dispatch(
    message: &Value,
    writer: &Writer,
    pending: &PendingMap,
    next_seq: &Arc<AtomicI64>,
    inbound: &Arc<dyn Inbound>,
) {
    let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "response" => {
            let Some(seq) = message.get("request_seq").and_then(Value::as_i64) else {
                return;
            };
            let command = message
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let result = if message.get("success").and_then(Value::as_bool) == Some(true) {
                Ok(message.get("body").cloned().unwrap_or(Value::Null))
            } else {
                Err(response_error(command, message))
            };
            // The waiter may have timed out and gone; its error is then of no use to anyone.
            let _ = lock(pending).remove(&seq).map(|tx| tx.send(result));
        }
        "event" => {
            let name = message.get("event").and_then(Value::as_str).unwrap_or("");
            inbound.event(name, message.get("body").cloned().unwrap_or(Value::Null));
        }
        "request" => {
            let request_seq = message.get("seq").and_then(Value::as_i64).unwrap_or(0);
            let command = message
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let args = message.get("arguments").cloned().unwrap_or(Value::Null);
            let inbound = Arc::clone(inbound);
            let writer = Arc::clone(writer);
            let next_seq = Arc::clone(next_seq);
            tokio::spawn(async move {
                // The reader loop keeps running, so the response cannot be missed.
                let outcome = inbound.reverse_request(command.clone(), args).await;
                let seq = next_seq.fetch_add(1, Ordering::SeqCst);
                let mut reply = json!({
                    "seq": seq, "type": "response", "request_seq": request_seq,
                    "command": command, "success": outcome.is_ok(),
                });
                if let Some(map) = reply.as_object_mut() {
                    match outcome {
                        Ok(body) => {
                            map.insert("body".to_owned(), body);
                        }
                        Err(e) => {
                            map.insert("message".to_owned(), Value::String(e.to_string()));
                        }
                    }
                }
                // A closed socket ends the read loop, which reports the loss.
                let _ = write_message(&mut *writer.lock().await, &reply).await;
            });
        }
        _ => {}
    }
}
