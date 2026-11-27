//! Native Chrome browser relay host (`hrn browser-relay serve --stdio`) and discovery state.
//!
//! Bridges authenticated loopback RFC 6455 WebSocket connections (`/control` and `/tab/<id>`)
//! to the Chrome MV3 extension over Chrome Native Messaging stdio framing.

use crate::relay_install::{DEFAULT_HKCU_KEY, install_relay, uninstall_relay};
use hrn_edge_ipc::{
    ServerWsEvent, accept_loopback_ws, generate_token, read_native_message,
    write_native_message_chunked,
};
use hrn_edge_rt::tokio;
use hrn_kernel::serde::{Deserialize, Serialize};
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{CancelFlag, CommandOutcome, HrnError, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

/// Periodic keepalive ping sent to the Chrome MV3 service worker so Chrome does not suspend it.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// Discovery file written by `hrn browser-relay serve --stdio` at `<data>/browser-relay.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "hrn_kernel::serde")]
pub struct RelayState {
    /// Bound loopback port serving the authenticated WebSocket bridge.
    pub port: u16,
    /// Ephemeral 256-bit bearer token required by all WebSocket clients.
    pub token: String,
    /// Process ID of the running relay host.
    pub pid: u32,
}

impl RelayState {
    /// Loopback WebSocket URL for control commands (`Relay.openTab`, `Relay.listTabs`, etc.).
    pub fn control_ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/control?token={}", self.port, self.token)
    }

    /// Loopback WebSocket URL scoped to a single Chrome tab ID.
    pub fn tab_ws_url(&self, tab_id: i64) -> String {
        format!(
            "ws://127.0.0.1:{}/tab/{tab_id}?token={}",
            self.port, self.token
        )
    }
}

/// Path to `<data_dir>/browser-relay.json`.
pub fn relay_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("browser-relay.json")
}

/// Read `<data_dir>/browser-relay.json`, returning an actionable hint if the relay is absent.
///
/// # Errors
///
/// Returns `HrnError::Tool` instructing the user to run `hrn browser-relay install` when the
/// discovery file is missing or malformed.
pub fn read_relay_state(data_dir: &Path) -> Result<RelayState> {
    let path = relay_state_path(data_dir);
    let raw = std::fs::read_to_string(&path).map_err(|_| {
        HrnError::Tool(
            "browser relay is not running; run `hrn browser-relay install` and load the unpacked \
             extension in Chrome"
                .to_owned(),
        )
    })?;
    serde_json::from_str(&raw).map_err(|_| {
        HrnError::Tool(
            "browser relay state is invalid; run `hrn browser-relay install` and reload the \
             extension in Chrome"
                .to_owned(),
        )
    })
}

fn write_relay_state(data_dir: &Path, state: &RelayState) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = relay_state_path(data_dir);
    let tmp = data_dir.join(format!("browser-relay.{}.tmp", state.pid));
    let data = serde_json::to_vec_pretty(state)?;
    std::fs::write(&tmp, data)?;
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn remove_relay_state_if_owned(data_dir: &Path, pid: u32) {
    if let Ok(existing) = read_relay_state(data_dir)
        && existing.pid == pid
    {
        let _ = std::fs::remove_file(relay_state_path(data_dir));
    }
}

type PendingReplies = Arc<Mutex<BTreeMap<i64, oneshot::Sender<Result<Value>>>>>;

#[derive(Clone, Debug)]
struct TabEvent {
    tab_id: i64,
    method: String,
    params: Value,
}

struct RelayHub {
    to_ext: mpsc::UnboundedSender<Value>,
    pending: PendingReplies,
    events: broadcast::Sender<TabEvent>,
    next_id: AtomicI64,
}

impl RelayHub {
    async fn call_extension(&self, mut cmd: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Some(obj) = cmd.as_object_mut() {
            obj.insert("id".to_owned(), json!(id));
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if self.to_ext.send(cmd).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(HrnError::Tool(
                "browser relay extension disconnected".to_owned(),
            ));
        }
        rx.await.map_err(|_| {
            HrnError::Tool("browser relay extension closed while awaiting reply".to_owned())
        })?
    }
}

/// Running relay server handle returned by [`start_relay_over_io`] (primarily for tests).
pub struct RelayServerHandle {
    /// Bound port, token, and PID written to `<data>/browser-relay.json`.
    pub state: RelayState,
    data_dir: PathBuf,
    cancel: CancelFlag,
    task: tokio::task::JoinHandle<()>,
}

impl RelayServerHandle {
    /// Stop the relay server and remove its discovery file if still owned by this PID.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        self.task.abort();
        let _ = self.task.await;
        remove_relay_state_if_owned(&self.data_dir, self.state.pid);
    }
}

/// Start the loopback WebSocket relay server bridged to Chrome Native Messaging I/O halves.
///
/// # Errors
///
/// When binding the loopback socket, generating the bearer token, or writing
/// `<data>/browser-relay.json` fails.
pub async fn start_relay_over_io<R, W>(
    data_dir: &Path,
    mut stdin: R,
    mut stdout: W,
    cancel: CancelFlag,
) -> Result<RelayServerHandle>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| HrnError::Io(format!("bind browser-relay loopback listener: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| HrnError::Io(format!("browser-relay local_addr: {e}")))?
        .port();
    let token = generate_token()?;
    let pid = std::process::id();
    let state = RelayState { port, token, pid };
    write_relay_state(data_dir, &state)?;

    let (to_ext_tx, mut to_ext_rx) = mpsc::unbounded_channel::<Value>();
    let pending: PendingReplies = Arc::new(Mutex::new(BTreeMap::new()));
    let (events_tx, _) = broadcast::channel::<TabEvent>(256);

    let hub = Arc::new(RelayHub {
        to_ext: to_ext_tx,
        pending: Arc::clone(&pending),
        events: events_tx.clone(),
        next_id: AtomicI64::new(1),
    });

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = to_ext_rx.recv().await {
            if write_native_message_chunked(&mut stdout, &msg)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let ping_tx = hub.to_ext.clone();
    let ping_cancel = cancel.clone();
    let ping_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(KEEPALIVE_INTERVAL);
        ticker.tick().await;
        loop {
            tokio::select! {
                () = ping_cancel.cancelled() => break,
                _ = ticker.tick() => {
                    if ping_tx.send(json!({ "cmd": "ping" })).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let reader_cancel = cancel.clone();
    let reader_pending = Arc::clone(&pending);
    let reader_events = events_tx;
    let reader_to_ext = hub.to_ext.clone();
    let reader_task = tokio::spawn(async move {
        while let Ok(Some(msg)) = read_native_message(&mut stdin).await {
            handle_extension_message(&msg, &reader_pending, &reader_events, &reader_to_ext).await;
        }
        reader_cancel.cancel();
        let mut map = reader_pending.lock().await;
        for (_, tx) in std::mem::take(&mut *map) {
            let _ = tx.send(Err(HrnError::Tool(
                "browser relay extension disconnected".to_owned(),
            )));
        }
    });

    let accept_token = state.token.clone();
    let accept_cancel = cancel.clone();
    let data_dir_owned = data_dir.to_path_buf();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = accept_cancel.cancelled() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break; };
                    let tok = accept_token.clone();
                    let hub = Arc::clone(&hub);
                    tokio::spawn(async move {
                        if let Ok(ws) = accept_loopback_ws(stream, &tok).await {
                            serve_ws_client(ws, hub).await;
                        }
                    });
                }
            }
        }
        ping_task.abort();
        writer_task.abort();
        reader_task.abort();
        remove_relay_state_if_owned(&data_dir_owned, pid);
    });

    Ok(RelayServerHandle {
        state,
        data_dir: data_dir.to_path_buf(),
        cancel,
        task,
    })
}

/// Serve `hrn browser-relay serve --stdio` on real process stdin/stdout until EOF or Ctrl-C.
///
/// # Errors
///
/// When binding the loopback socket or initializing relay state fails.
pub async fn serve_stdio(data_dir: &Path, cancel: CancelFlag) -> Result<()> {
    let handle = start_relay_over_io(
        data_dir,
        tokio::io::stdin(),
        tokio::io::stdout(),
        cancel.clone(),
    )
    .await?;
    cancel.cancelled().await;
    handle.shutdown().await;
    Ok(())
}

async fn handle_extension_message(
    msg: &Value,
    pending: &PendingReplies,
    events: &broadcast::Sender<TabEvent>,
    to_ext: &mpsc::UnboundedSender<Value>,
) {
    if msg.get("ping").and_then(Value::as_bool) == Some(true) {
        let _ = to_ext.send(json!({ "pong": true }));
        return;
    }
    if msg.get("pong").and_then(Value::as_bool) == Some(true) {
        return;
    }
    if msg.get("event").and_then(Value::as_bool) == Some(true) {
        if let (Some(tab_id), Some(method)) = (
            msg.get("tabId").and_then(Value::as_i64),
            msg.get("method").and_then(Value::as_str),
        ) {
            let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
            let _ = events.send(TabEvent {
                tab_id,
                method: method.to_owned(),
                params,
            });
        }
        return;
    }

    if let Some(id) = msg.get("id").and_then(Value::as_i64)
        && let Some(tx) = pending.lock().await.remove(&id)
    {
        let ok = msg.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let outcome = if ok {
            Ok(msg.get("result").cloned().unwrap_or_else(|| json!({})))
        } else {
            let err = msg
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("browser relay command failed");
            Err(HrnError::Tool(err.to_owned()))
        };
        let _ = tx.send(outcome);
    }
}

async fn serve_ws_client(ws: hrn_edge_ipc::ServerWs, hub: Arc<RelayHub>) {
    let scoped_tab_id = ws
        .path()
        .strip_prefix("/tab/")
        .and_then(|s| s.parse::<i64>().ok());
    let (mut reader, writer) = ws.into_split();
    let writer = Arc::new(Mutex::new(writer));

    let event_task = scoped_tab_id.map(|tab_id| {
        let mut rx = hub.events.subscribe();
        let writer = Arc::clone(&writer);
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if ev.tab_id != tab_id {
                    continue;
                }
                let payload = json!({ "method": ev.method, "params": ev.params }).to_string();
                if writer.lock().await.send_text(&payload).await.is_err() {
                    break;
                }
            }
        })
    });

    while let Ok(ev) = reader.next_event().await {
        match ev {
            ServerWsEvent::Text(text) => {
                let Ok(req) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let client_id = req.get("id").and_then(Value::as_i64).unwrap_or(0);
                let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
                let ext_cmd = translate_client_request(scoped_tab_id, method, &params);
                let reply = match ext_cmd {
                    Ok(cmd) => match hub.call_extension(cmd).await {
                        Ok(result) => json!({ "id": client_id, "result": result }),
                        Err(err) => {
                            json!({ "id": client_id, "error": { "message": err.to_string() } })
                        }
                    },
                    Err(err) => json!({ "id": client_id, "error": { "message": err.to_string() } }),
                };
                if writer
                    .lock()
                    .await
                    .send_text(&reply.to_string())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            ServerWsEvent::Ping(payload) => {
                let _ = writer.lock().await.send_pong(&payload).await;
            }
            ServerWsEvent::Closed => break,
        }
    }

    if let Some(task) = event_task {
        task.abort();
    }
}

fn translate_client_request(
    scoped_tab_id: Option<i64>,
    method: &str,
    params: &Value,
) -> Result<Value> {
    match method {
        "Relay.openTab" => Ok(json!({
            "cmd": "open_tab",
            "url": params.get("url").and_then(Value::as_str).unwrap_or("about:blank"),
        })),
        "Relay.listTabs" => Ok(json!({ "cmd": "list_tabs" })),
        "Relay.attachTab" => Ok(json!({
            "cmd": "attach_tab",
            "tabId": req_tab_id(scoped_tab_id, params)?,
        })),
        "Relay.detachTab" => Ok(json!({
            "cmd": "detach_tab",
            "tabId": req_tab_id(scoped_tab_id, params)?,
        })),
        "Relay.closeTab" => Ok(json!({
            "cmd": "close_tab",
            "tabId": req_tab_id(scoped_tab_id, params)?,
        })),
        other => Ok(json!({
            "cmd": "cdp",
            "tabId": req_tab_id(scoped_tab_id, params)?,
            "method": other,
            "params": params,
        })),
    }
}

fn req_tab_id(scoped_tab_id: Option<i64>, params: &Value) -> Result<i64> {
    scoped_tab_id
        .or_else(|| params.get("tabId").and_then(Value::as_i64))
        .ok_or_else(|| HrnError::Tool("browser relay request missing `tabId`".to_owned()))
}

/// Dispatch `hrn browser-relay <install|serve> ...`.
///
/// # Errors
///
/// When arguments are invalid or the requested subcommand fails.
pub(crate) async fn run_cli(
    data_dir: &Path,
    args: Vec<String>,
    cancel: CancelFlag,
) -> Result<CommandOutcome> {
    let sub = args.first().map_or("--help", String::as_str);
    match sub {
        "-h" | "--help" | "help" => Ok(CommandOutcome {
            stdout: relay_help(),
            stderr: String::new(),
            code: 0,
        }),
        "install" => {
            if args.iter().any(|a| a == "-h" || a == "--help") {
                return Ok(CommandOutcome {
                    stdout: "Usage: hrn browser-relay install [--uninstall]\n\n\
                             Install or remove the unpacked Chrome MV3 relay extension and \
                             NativeMessagingHosts manifest.\n"
                        .to_owned(),
                    stderr: String::new(),
                    code: 0,
                });
            }
            let uninstall = args.iter().any(|a| a == "--uninstall");
            if uninstall {
                uninstall_relay(data_dir, DEFAULT_HKCU_KEY)?;
                return Ok(CommandOutcome {
                    stdout: format!(
                        "Uninstalled hrn browser relay and removed `{DEFAULT_HKCU_KEY}`.\n"
                    ),
                    stderr: String::new(),
                    code: 0,
                });
            }
            let exe = std::env::current_exe()
                .map_err(|e| HrnError::Io(format!("locate current hrn binary: {e}")))?;
            let report = install_relay(data_dir, &exe, DEFAULT_HKCU_KEY)?;
            Ok(CommandOutcome {
                stdout: format!(
                    "Installed hrn browser relay native host.\n\
                     Extension ID:       {}\n\
                     Native manifest:    {}\n\
                     Registry key:       {}\n\
                     Unpacked extension: {}\n\n\
                     Next step: open chrome://extensions, enable Developer mode, click \"Load unpacked\", \
                     and select the unpacked extension directory above.\n",
                    report.extension_id,
                    report.manifest_path.display(),
                    report.registry_key,
                    report.extension_dir.display(),
                ),
                stderr: String::new(),
                code: 0,
            })
        }
        "serve" => {
            if args.iter().any(|a| a == "-h" || a == "--help") {
                return Ok(CommandOutcome {
                    stdout: "Usage: hrn browser-relay serve --stdio\n\n\
                             Run the Chrome Native Messaging host on stdin/stdout.\n"
                        .to_owned(),
                    stderr: String::new(),
                    code: 0,
                });
            }
            if !args.iter().any(|a| a == "--stdio") {
                return Err(HrnError::Config(
                    "`hrn browser-relay serve` requires `--stdio`".to_owned(),
                ));
            }
            serve_stdio(data_dir, cancel).await?;
            Ok(CommandOutcome::default())
        }
        other => Err(HrnError::Config(format!(
            "unknown `browser-relay` subcommand `{other}` (expected `install` or `serve --stdio`)"
        ))),
    }
}

fn relay_help() -> String {
    "Usage: hrn browser-relay <COMMAND>\n\n\
     Commands:\n  \
       install [--uninstall]  Write the NativeMessagingHosts manifest, register HKCU, and print the unpacked extension path\n  \
       serve --stdio          Serve the Chrome Native Messaging relay host on stdin/stdout\n"
        .to_owned()
}
