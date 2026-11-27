//! `hrn --mode rpc`: NDJSON commands in, omp's event stream out.
//!
//! One connection, one conversation. The session row is created on the first prompt, so a host
//! that only inspects state leaves no empty session behind.

mod commands;
mod prompt;
mod state;
mod table;
mod ui;

use crate::backend::Backend;
use crate::events::Translator;
use crate::wire::{Out, Pending, put};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, EventSink, PermissionMode, Result, ThinkingLevel, TodoPhase,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::Notify;

/// The frame cap of protocol v1; hrn does not chunk, and says so in `ready`.
const MAX_FRAME: usize = 1024 * 1024;

/// What the host asked the process to do at startup.
#[derive(Clone, Debug)]
pub struct Options {
    /// Directory new sessions start in.
    pub cwd: PathBuf,
    /// Answer `ask` form dialogs (omp's `rpc-ui`; plain `rpc` has no tool UI).
    pub tool_ui: bool,
    /// Ask the host to approve mutating tools. omp pins approvals through its extension runner,
    /// which hrn has none of, so both RPC modes ask unless `--no-ui` is given.
    pub approvals: bool,
    /// Start sessions in `yolo` so nothing asks at all.
    pub auto_approve: bool,
}

/// The mutable part of a connection.
#[expect(
    clippy::struct_excessive_bools,
    reason = "one flag per host-settable protocol state"
)]
pub(crate) struct State {
    /// The conversation, created on the first prompt.
    pub(crate) session: Option<String>,
    pub(crate) session_name: Option<String>,
    pub(crate) model: String,
    pub(crate) thinking: ThinkingLevel,
    pub(crate) mode: PermissionMode,
    pub(crate) cwd: PathBuf,
    pub(crate) translator: Translator,
    pub(crate) auto_compaction: bool,
    pub(crate) ask_dialog: bool,
    pub(crate) fast_mode_enabled: bool,
    pub(crate) slow_mode_enabled: bool,
    /// Event types the host wants forwarded; `None` is all.
    pub(crate) events: Option<Vec<String>>,
    /// Tools the host approved for the whole session, keyed by tool name -> granted risk tier.
    pub(crate) grants: std::collections::BTreeMap<String, hrn_kernel::ToolTier>,
    /// Chips of steering messages the live run has not injected yet.
    pub(crate) steering: Vec<String>,
    /// Prompts waiting for the agent to finish what it was doing.
    pub(crate) follow_up: Vec<String>,
    pub(crate) steering_mode: String,
    pub(crate) follow_up_mode: String,
    pub(crate) interrupt_mode: String,
    /// The queues as the host last saw them, so unchanged ones are not resent.
    queue_sent: (Vec<String>, Vec<String>),
    pub(crate) todos: Vec<TodoPhase>,
    pub(crate) compacting: bool,
    /// Cancellation of the compaction in flight.
    pub(crate) compaction: Option<CancelFlag>,
    /// Cancellation of the live run; `None` when nothing runs.
    pub(crate) run: Option<CancelFlag>,
    /// A run task is looping; its turn may not have noticed the cancellation yet.
    pub(crate) run_active: bool,
    /// Bumped by an abort or a session change; the looping run task stops when it notices.
    pub(crate) generation: u64,
    /// Ids of the prompts the live run must report its outcome to.
    pub(crate) waiters: Vec<Value>,
    /// The prompt and result of the last completed run, for `resume`.
    pub(crate) last_run: Option<(String, crate::backend::TurnSummary)>,
}

/// Everything the command handlers share.
pub(crate) struct Core {
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) out: Out,
    pub(crate) pending: Pending,
    pub(crate) opts: Options,
    /// Woken when a run or compaction ends.
    pub(crate) idle: Notify,
    pub(crate) state: Mutex<State>,
}

/// A fresh connection state from `opts` and the runtime's defaults.
fn initial_state(backend: &dyn Backend, opts: &Options) -> Result<State> {
    Ok(State {
        session: None,
        session_name: None,
        model: backend.default_model()?.alias,
        thinking: backend.default_thinking(),
        mode: if opts.auto_approve {
            PermissionMode::Yolo
        } else {
            backend.default_mode()
        },
        cwd: opts.cwd.clone(),
        translator: Translator::default(),
        auto_compaction: true,
        ask_dialog: false,
        fast_mode_enabled: backend.fast_mode_enabled(),
        slow_mode_enabled: backend.slow_mode_enabled(),
        events: None,
        grants: std::collections::BTreeMap::new(),
        steering: Vec::new(),
        follow_up: Vec::new(),
        steering_mode: "one-at-a-time".to_owned(),
        follow_up_mode: "one-at-a-time".to_owned(),
        interrupt_mode: "immediate".to_owned(),
        queue_sent: (Vec::new(), Vec::new()),
        todos: Vec::new(),
        compacting: false,
        compaction: None,
        run: None,
        run_active: false,
        generation: 0,
        waiters: Vec::new(),
        last_run: None,
    })
}

impl Core {
    /// Lock the state, keeping the connection alive after a panicked holder.
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write one frame. An oversized response becomes a failure response and an oversized
    /// `agent_end` keeps only its message count (omp's terminal-frame compaction); anything else
    /// too big for the transport is reported as `rpc_frame_error`.
    pub(crate) async fn send(&self, frame: &Value) {
        let mut attempt = frame.clone();
        for retry in 0..2 {
            match self.out.send(&attempt).await {
                Ok(_) => return,
                Err(err) => {
                    if retry == 0 {
                        attempt = shrink(frame, err.0);
                    } else {
                        return;
                    }
                }
            }
        }
    }

    /// Write every frame of a translated event run.
    pub(crate) async fn send_all(&self, frames: &[Value]) {
        for frame in frames {
            self.send(frame).await;
        }
    }

    /// A `response` frame for `command`, echoing `id` when the command carried one.
    pub(crate) async fn respond(&self, id: Option<&Value>, command: &str, body: &Value) {
        let mut frame = match body.as_object() {
            Some(body) => Value::Object(body.clone()),
            None => json!({}),
        };
        if let Some(object) = frame.as_object_mut() {
            object.insert("type".to_owned(), json!("response"));
            object.insert("command".to_owned(), json!(command));
            if let Some(id) = id {
                object.insert("id".to_owned(), id.clone());
            }
            object.entry("success".to_owned()).or_insert(json!(true));
        }
        self.send(&frame).await;
    }

    /// The success shape of a response.
    pub(crate) fn ok(data: Option<Value>) -> Value {
        match data {
            Some(data) => json!({ "success": true, "data": data }),
            None => json!({ "success": true }),
        }
    }

    /// The failure shape of a response.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "`json!` consumes the string into the frame"
    )]
    pub(crate) fn fail(error: String, code: Option<&str>) -> Value {
        let mut frame = json!({ "success": false, "error": error });
        if let Some(code) = code {
            put(&mut frame, "code", json!(code));
        }
        frame
    }

    /// The `queue_update` frame, `None` when the queues are as the host last saw them.
    fn queue_update(&self) -> Option<Value> {
        let mut state = self.lock();
        let now = (state.steering.clone(), state.follow_up.clone());
        if state.queue_sent == now {
            return None;
        }
        state.queue_sent = now.clone();
        Some(json!({ "type": "queue_update", "steering": now.0, "followUp": now.1 }))
    }

    /// Tell the host about the queues when they changed.
    pub(crate) async fn sync_queue(&self) {
        if let Some(frame) = self.queue_update() {
            self.send(&frame).await;
        }
    }

    /// The runtime events of a live run, translated and filtered.
    pub(crate) fn sink(self: &Arc<Self>) -> Arc<dyn EventSink> {
        Arc::new(Sink {
            core: Arc::clone(self),
        })
    }

    /// Whether nothing runs, nothing is queued and no background work can wake the session.
    pub(crate) fn is_settled(&self) -> bool {
        let state = self.lock();
        let pending = state
            .session
            .as_deref()
            .is_some_and(|id| self.backend.pending_async(id));
        !state.run_active
            && !state.compacting
            && state.steering.is_empty()
            && state.follow_up.is_empty()
            && !pending
    }

    /// The failure a command reports when it names a setting hrn has no equivalent of.
    pub(crate) fn unsupported(name: &str) -> Value {
        Self::fail(
            format!("`{name}` is not supported by hrn's runtime"),
            Some("unsupported"),
        )
    }
}

/// The fallback for a frame that will not fit.
fn shrink(frame: &Value, bytes: usize) -> Value {
    let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
    let id = frame.get("id").filter(|v| !v.is_null()).cloned();
    match kind {
        "response" => {
            let mut body = Core::fail(
                format!("RPC response exceeded the transport limit ({bytes} bytes)"),
                Some("frame_overflow"),
            );
            if let (Some(object), Some(body_object)) = (body.as_object_mut(), frame.as_object()) {
                object.insert(
                    "command".to_owned(),
                    body_object.get("command").cloned().unwrap_or(Value::Null),
                );
            }
            if let (Some(object), Some(id)) = (body.as_object_mut(), id) {
                object.insert("id".to_owned(), id);
            }
            body
        }
        "agent_end" => {
            let count = frame
                .get("messages")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            json!({
                "type": "agent_end",
                "messages": [],
                "messageCount": count,
                "isTerminal": true,
                "yielded": frame.get("yielded").cloned().unwrap_or(Value::Bool(true)),
            })
        }
        _ => {
            let mut notice = json!({
                "type": "rpc_frame_error",
                "frameType": kind,
                "reason": format!("frame of {bytes} bytes exceeded the transport limit"),
            });
            if let (Some(object), Some(id)) = (notice.as_object_mut(), id) {
                object.insert("id".to_owned(), id);
            }
            notice
        }
    }
}

/// Runtime events as RPC frames.
struct Sink {
    core: Arc<Core>,
}

impl EventSink for Sink {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        let core = Arc::clone(&self.core);
        Box::pin(async move {
            let frames = {
                let mut state = core.lock();
                observe(&mut state, &event);
                let mut frames = state.translator.handle(&event);
                if let Some(allowed) = &state.events {
                    frames.retain(|frame| {
                        frame
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| allowed.iter().any(|a| a == kind))
                    });
                }
                frames
            };
            core.send_all(&frames).await;
            core.sync_queue().await;
            true
        })
    }
}

/// What the connection must remember from an event.
fn observe(state: &mut State, event: &AgentEvent) {
    match event {
        AgentEvent::SteerInjected { text, .. } => {
            state.steering.retain(|chip| chip != text);
        }
        AgentEvent::TodosUpdated { phases } => state.todos.clone_from(phases),
        _ => {}
    }
}

/// The frame a fresh connection is told first.
fn ready_frame() -> Value {
    json!({
        "type": "ready",
        "protocolVersion": 1,
        "supportedProtocolVersions": [1],
        "maxFrameBytes": MAX_FRAME,
        "supportsBashExcludeFromContext": false,
    })
}

/// Serve one RPC connection over `reader`/`writer`; returns when stdin closes.
///
/// # Errors
/// stdin or stdout failed, or the runtime has no default model.
pub async fn serve(
    backend: Arc<dyn Backend>,
    opts: Options,
    reader: impl AsyncRead + Unpin + Send + 'static,
    writer: impl AsyncWrite + Unpin + Send + 'static,
) -> std::io::Result<()> {
    let (out, writer_task) = Out::spawn(writer, Some(MAX_FRAME));
    let state = initial_state(backend.as_ref(), &opts).map_err(std::io::Error::other)?;
    let core = Arc::new(Core {
        backend,
        out,
        pending: Pending::default(),
        opts,
        idle: Notify::new(),
        state: Mutex::new(state),
    });
    core.send(&ready_frame()).await;
    let commands = core.command_rows();
    core.send(&json!({ "type": "available_commands_update", "commands": commands }))
        .await;
    let mut lines = BufReader::new(reader).lines();
    let mut read_result: std::io::Result<()> = Ok(());
    loop {
        match lines.next_line().await {
            Ok(None) => break,
            Ok(Some(line)) if line.trim().is_empty() => {}
            Ok(Some(line)) => Core::handle_line(&core, &line).await,
            Err(err) => {
                read_result = Err(err);
                break;
            }
        }
    }
    // stdin is gone: no pending dialog can ever be answered.
    core.pending.fail_all();
    while core.lock().run_active || core.lock().compacting {
        core.idle.notified().await;
    }
    core.backend.shutdown().await;
    drop(core.out.clone());
    drop(core);
    let write = match writer_task.await {
        Ok(result) => result,
        Err(err) => Err(std::io::Error::other(err.to_string())),
    };
    read_result.and(write)
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_tiers;
