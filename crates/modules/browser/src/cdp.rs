//! Async Chrome `DevTools` Protocol (CDP) JSON-RPC client with console & network ring buffers.

use hrn_edge_http::ws::{WsEvent, WsReader, WsWriter, connect_loopback_ws};
use hrn_edge_rt::tokio;
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{HrnError, Result};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};

const MAX_LOG_ENTRIES: usize = 120;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

type PendingMap = BTreeMap<i64, oneshot::Sender<Result<Value>>>;

/// Active CDP WebSocket session attached to a browser page target.
pub(crate) struct CdpClient {
    writer: Arc<Mutex<WsWriter>>,
    pending: Arc<Mutex<PendingMap>>,
    console: Arc<Mutex<VecDeque<String>>>,
    network: Arc<Mutex<VecDeque<String>>>,
    next_id: AtomicI64,
    closed: Arc<AtomicBool>,
    reader_task: tokio::task::JoinHandle<()>,
}

impl CdpClient {
    /// Connect to a page's `ws://127.0.0.1:<port>/devtools/page/...` WebSocket endpoint.
    pub(crate) async fn connect(ws_url: &str) -> Result<Self> {
        let ws = connect_loopback_ws(ws_url, CONNECT_TIMEOUT).await?;
        let (reader, writer) = ws.into_split();
        let writer = Arc::new(Mutex::new(writer));
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(BTreeMap::new()));
        let console: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let network: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let closed = Arc::new(AtomicBool::new(false));

        let r_writer = Arc::clone(&writer);
        let r_pending = Arc::clone(&pending);
        let r_console = Arc::clone(&console);
        let r_network = Arc::clone(&network);
        let r_closed = Arc::clone(&closed);

        let reader_task = tokio::spawn(async move {
            read_loop(reader, r_writer, r_pending, r_console, r_network, r_closed).await;
        });

        Ok(Self {
            writer,
            pending,
            console,
            network,
            next_id: AtomicI64::new(1),
            closed,
            reader_task,
        })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Invoke a CDP command and await its `result` payload.
    pub(crate) async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        if self.is_closed() {
            return Err(HrnError::Tool(
                "CDP browser session disconnected".to_owned(),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        let text = serde_json::to_string(&msg)
            .map_err(|e| HrnError::Tool(format!("CDP encode failed: {e}")))?;

        if let Err(err) = self.writer.lock().await.send_text(&text).await {
            self.pending.lock().await.remove(&id);
            return Err(err);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(HrnError::Tool(format!(
                "CDP connection closed while awaiting `{method}`"
            ))),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(HrnError::Tool(format!(
                    "CDP `{method}` timed out after {}s",
                    timeout.as_secs()
                )))
            }
        }
    }

    /// Enable core CDP domains (`Page`, `Runtime`, `DOM`, `Network`, `Accessibility`).
    pub(crate) async fn enable_domains(&self) -> Result<()> {
        let t = Duration::from_secs(10);
        for domain in [
            "Page.enable",
            "Runtime.enable",
            "DOM.enable",
            "Network.enable",
            "Accessibility.enable",
        ] {
            let _ = self.call(domain, json!({}), t).await;
        }
        Ok(())
    }

    /// Snapshot (and optionally clear) buffered browser console entries.
    pub(crate) async fn console_logs(&self, clear: bool) -> Vec<String> {
        let mut guard = self.console.lock().await;
        let items: Vec<String> = guard.iter().cloned().collect();
        if clear {
            guard.clear();
        }
        items
    }

    /// Snapshot (and optionally clear) buffered network responses/failures.
    pub(crate) async fn network_logs(&self, clear: bool) -> Vec<String> {
        let mut guard = self.network.lock().await;
        let items: Vec<String> = guard.iter().cloned().collect();
        if clear {
            guard.clear();
        }
        items
    }

    /// Gracefully send a WebSocket close frame.
    pub(crate) async fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.writer.lock().await.send_close().await;
    }
}

impl Drop for CdpClient {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        self.reader_task.abort();
    }
}

async fn read_loop(
    mut reader: WsReader,
    writer: Arc<Mutex<WsWriter>>,
    pending: Arc<Mutex<PendingMap>>,
    console: Arc<Mutex<VecDeque<String>>>,
    network: Arc<Mutex<VecDeque<String>>>,
    closed: Arc<AtomicBool>,
) {
    loop {
        match reader.next_event().await {
            Ok(WsEvent::Text(text)) => {
                if let Ok(val) = serde_json::from_str::<Value>(&text) {
                    dispatch_message(&val, &pending, &console, &network).await;
                }
            }
            Ok(WsEvent::Ping(payload)) => {
                let _ = writer.lock().await.send_pong(&payload).await;
            }
            Ok(WsEvent::Binary(_)) => {}
            Ok(WsEvent::Closed) | Err(_) => break,
        }
    }
    closed.store(true, Ordering::Relaxed);
    let mut map = pending.lock().await;
    for (_, tx) in std::mem::take(&mut *map) {
        let _ = tx.send(Err(HrnError::Tool("CDP session closed".to_owned())));
    }
}

async fn dispatch_message(
    msg: &Value,
    pending: &Arc<Mutex<PendingMap>>,
    console: &Arc<Mutex<VecDeque<String>>>,
    network: &Arc<Mutex<VecDeque<String>>>,
) {
    if let Some(id) = msg.get("id").and_then(Value::as_i64) {
        if let Some(tx) = pending.lock().await.remove(&id) {
            let res = if let Some(err) = msg.get("error") {
                let text = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("CDP error");
                Err(HrnError::Tool(format!("CDP error: {text}")))
            } else {
                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = tx.send(res);
        }
        return;
    }

    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = msg.get("params").unwrap_or(&Value::Null);
    match method {
        "Runtime.consoleAPICalled" => {
            let level = params.get("type").and_then(Value::as_str).unwrap_or("log");
            let text = params
                .get("args")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(format_remote_arg)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            push_bounded(console, format!("[{level}] {text}")).await;
        }
        "Runtime.exceptionThrown" => {
            let desc = params
                .pointer("/exceptionDetails/exception/description")
                .or_else(|| params.pointer("/exceptionDetails/text"))
                .and_then(Value::as_str)
                .unwrap_or("uncaught exception");
            push_bounded(console, format!("[exception] {desc}")).await;
        }
        "Network.responseReceived" => {
            let status = params
                .pointer("/response/status")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let mime = params
                .pointer("/response/mimeType")
                .and_then(Value::as_str)
                .unwrap_or("");
            let url = params
                .pointer("/response/url")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !url.starts_with("data:") {
                push_bounded(network, format!("{status} {mime} {url}")).await;
            }
        }
        "Network.loadingFailed" => {
            let reason = params
                .get("errorText")
                .and_then(Value::as_str)
                .unwrap_or("failed");
            let ty = params.get("type").and_then(Value::as_str).unwrap_or("");
            push_bounded(network, format!("FAIL {ty} {reason}")).await;
        }
        _ => {}
    }
}

async fn push_bounded(queue: &Arc<Mutex<VecDeque<String>>>, line: String) {
    let mut guard = queue.lock().await;
    if guard.len() >= MAX_LOG_ENTRIES {
        guard.pop_front();
    }
    guard.push_back(line);
}

fn format_remote_arg(v: &Value) -> String {
    if let Some(val) = v.get("value") {
        match val {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    } else if let Some(desc) = v.get("description").and_then(Value::as_str) {
        desc.to_owned()
    } else {
        v.get("type")
            .and_then(Value::as_str)
            .unwrap_or("object")
            .to_owned()
    }
}
