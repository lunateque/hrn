//! `hrn acp` / `hrn --mode acp`: Zed's Agent Client Protocol over stdio.
//!
//! JSON-RPC 2.0, one object per line. The agent side implements `initialize`, `session/new`,
//! `session/load`, `session/list`, `session/prompt`, `session/cancel`, `session/set_mode` and
//! `session/set_config_option`; it sends `session/update` notifications, `session/request_permission`
//! for tool approvals, `elicitation/create` for `ask` forms, and delegates whole-file writes back
//! to the client through `fs/write_text_file` when the client advertises it.

mod client;
mod fs_hook;
mod server;
mod shape;
#[cfg(test)]
mod tests;
mod update;

use crate::backend::Backend;
use crate::wire::{Out, Pending};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};

/// The ACP protocol version this agent speaks.
const PROTOCOL_VERSION: u64 = 1;

/// What the host asked the process to do at startup.
#[derive(Clone, Debug)]
pub struct Options {
    /// Directory sessions start in when the client does not name one.
    pub cwd: PathBuf,
    /// Ask the client to approve gated tools (`false` for `--no-ui`, failing closed).
    pub approvals: bool,
    /// Approve routine mutating tools without asking the client.
    pub auto_approve: bool,
}

/// One ACP session: a backend conversation plus its live cancellation and stream state.
pub(crate) struct AcpSession {
    /// The working directory the client asked for.
    pub(crate) cwd: PathBuf,
    /// Model alias in effect.
    pub(crate) model: String,
    /// Thinking effort in effect.
    pub(crate) thinking: hrn_kernel::ThinkingLevel,
    /// Permission mode in effect.
    pub(crate) mode: hrn_kernel::PermissionMode,
    /// Cancellation of the live prompt turn.
    pub(crate) cancel: Option<hrn_kernel::CancelFlag>,
    /// Tools the client allowed for the whole session, keyed by tool name -> granted risk tier.
    pub(crate) grants: BTreeMap<String, hrn_kernel::ToolTier>,
    /// The per-prompt event translator.
    pub(crate) translator: update::Update,
}

/// Everything the request handlers share.
pub(crate) struct Conn {
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) out: Out,
    pub(crate) pending: Pending,
    pub(crate) opts: Options,
    /// Whether the client finished `initialize` (requests before it are refused).
    pub(crate) initialized: Mutex<bool>,
    /// The client's advertised capabilities, verbatim.
    pub(crate) client_caps: Mutex<Value>,
    pub(crate) sessions: Mutex<BTreeMap<String, AcpSession>>,
}

impl Conn {
    /// Lock a mutex without ever panicking on a poisoned guard.
    pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write one frame.
    pub(crate) async fn send(&self, frame: &Value) {
        let _ = self.out.send(frame).await;
    }

    /// A JSON-RPC result for `id`.
    pub(crate) async fn reply(&self, id: Value, result: Value) {
        self.send(&serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .await;
    }

    /// A JSON-RPC error for `id`, using the standard code `code`.
    pub(crate) async fn reply_err(&self, id: Option<Value>, code: i64, message: &str) {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }))
        .await;
    }

    /// Every live session id.
    pub(crate) fn session_ids(&self) -> Vec<String> {
        Conn::lock(&self.sessions).keys().cloned().collect()
    }
}

/// Serve one ACP connection over `reader`/`writer`; returns when stdin closes.
///
/// # Errors
/// stdin or stdout failed.
pub async fn serve(
    backend: Arc<dyn Backend>,
    opts: Options,
    reader: impl AsyncRead + Unpin + Send + 'static,
    writer: impl AsyncWrite + Unpin + Send + 'static,
) -> std::io::Result<()> {
    let (out, writer_task) = Out::spawn(writer, None);
    let conn = Arc::new(Conn {
        backend,
        out,
        pending: Pending::default(),
        opts,
        initialized: Mutex::new(false),
        client_caps: Mutex::new(Value::Null),
        sessions: Mutex::new(BTreeMap::new()),
    });
    let mut lines = BufReader::new(reader).lines();
    let mut io_result: std::io::Result<()> = Ok(());
    loop {
        match lines.next_line().await {
            Ok(None) => break,
            Ok(Some(line)) if line.trim().is_empty() => {}
            Ok(Some(line)) => server::handle_line(&conn, &line).await,
            Err(err) => {
                io_result = Err(err);
                break;
            }
        }
    }
    conn.pending.fail_all();
    for id in conn.session_ids() {
        if let Some(cancel) = Conn::lock(&conn.sessions)
            .get(&id)
            .and_then(|s| s.cancel.clone())
        {
            cancel.cancel();
        }
    }
    conn.backend.shutdown().await;
    drop(conn.out.clone());
    drop(conn);
    let write = match writer_task.await {
        Ok(result) => result,
        Err(err) => Err(std::io::Error::other(err.to_string())),
    };
    io_result.and(write)
}
