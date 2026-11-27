//! `hrn daemon` and `hrn attach`: multi-agent loopback HTTP+SSE daemon backed by `AgentRuntime`.

use crate::config::Config;
use crate::term::{err_line, out};
use crate::wiring::{Background, build_runtime_with_mcp, open_store};
use hrn_edge_ipc::auth::now_unix_ms;
use hrn_edge_ipc::{
    ApproveRequest, CreateSessionRequest, DaemonBackend, DaemonEvent, DaemonServer, HostTrustStore,
    IpcClient, RestoreSessionRequest, RunFile, SessionSummaryDto, SshTunnel, StartTurnRequest,
    ensure_host_identity, generate_token, parse_remote_target, remove_run_file_if_owned,
    write_run_file,
};
use hrn_harness::{AgentRuntime, SessionControl, TurnIo};
use hrn_kernel::{
    AgentEvent, AgentStatus, ApprovalDecision, ApprovalGate, ApprovalGrantScope, ApprovalRequest,
    BoxFuture, CancelFlag, EventSink, HrnError, PermissionMode, Result, SessionStore,
    ThinkingLevel,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, oneshot};

struct SessionChannel {
    control: SessionControl,
    tx: broadcast::Sender<DaemonEvent>,
    seq: AtomicU64,
    pending_approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl SessionChannel {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            control: SessionControl::default(),
            tx,
            seq: AtomicU64::new(1),
            pending_approvals: Mutex::new(HashMap::new()),
        }
    }

    fn publish(&self, session_id: &str, event: AgentEvent) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(DaemonEvent {
            seq,
            session_id: session_id.to_owned(),
            event,
        });
    }
}

struct BroadcastEventSink {
    session_id: String,
    chan: Arc<SessionChannel>,
}

impl EventSink for BroadcastEventSink {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        self.chan.publish(&self.session_id, event);
        Box::pin(async { true })
    }
}

struct DaemonApprovalGate {
    session_id: String,
    auto_approve: bool,
    chan: Arc<SessionChannel>,
}

impl ApprovalGate for DaemonApprovalGate {
    fn await_decision(
        &self,
        request: ApprovalRequest,
        cancel: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>> {
        let must_ask =
            request.choices.as_slice() == [ApprovalGrantScope::Once] || request.reason.is_some();
        if self.auto_approve && !must_ask {
            return Box::pin(async { Ok(ApprovalDecision::once()) });
        }
        // Fail closed if nobody is subscribed to receive and answer a mandatory approval prompt.
        if must_ask && self.chan.tx.receiver_count() == 0 {
            return Box::pin(async { Ok(ApprovalDecision::Deny) });
        }
        let approval_id = request.id.clone();
        let (tx, rx) = oneshot::channel();
        self.chan
            .pending_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(approval_id.clone(), tx);
        self.chan
            .publish(&self.session_id, AgentEvent::ApprovalRequested { request });
        let chan = Arc::clone(&self.chan);
        Box::pin(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
            tokio::pin!(rx);
            loop {
                tokio::select! {
                    res = &mut rx => {
                        return Ok(ApprovalDecision::from_bool(res.unwrap_or(false)));
                    }
                    _ = ticker.tick() => {
                        if cancel.is_cancelled() {
                            chan.pending_approvals
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .remove(&approval_id);
                            return Err(HrnError::Cancelled);
                        }
                    }
                }
            }
        })
    }
}

struct RuntimeDaemonBackend {
    runtime: AgentRuntime,
    store: Arc<dyn SessionStore>,
    channels: Mutex<HashMap<String, Arc<SessionChannel>>>,
}

impl RuntimeDaemonBackend {
    fn new(runtime: AgentRuntime, store: Arc<dyn SessionStore>) -> Self {
        Self {
            runtime,
            store,
            channels: Mutex::new(HashMap::new()),
        }
    }

    fn channel(&self, session_id: &str) -> Arc<SessionChannel> {
        let mut map = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            map.entry(session_id.to_owned())
                .or_insert_with(|| Arc::new(SessionChannel::new())),
        )
    }
    fn is_active(&self, session_id: &str) -> bool {
        self.channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .is_some_and(|c| c.control.is_running())
    }
}

impl DaemonBackend for RuntimeDaemonBackend {
    fn list_sessions(&self) -> BoxFuture<Result<Vec<SessionSummaryDto>>> {
        let store = Arc::clone(&self.store);
        let active: std::collections::HashSet<String> = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, c)| c.control.is_running())
            .map(|(id, _)| id.clone())
            .collect();
        Box::pin(async move {
            let metas = store.list_sessions(200)?;
            Ok(metas
                .into_iter()
                .map(|m| {
                    let active_turn = active.contains(&m.id);
                    SessionSummaryDto {
                        id: m.id,
                        title: m.title,
                        cwd: m.cwd,
                        model: m.model,
                        status: m.status,
                        active_turn,
                        updated_at_ms: u64::try_from(m.updated_at_ms).unwrap_or(0),
                    }
                })
                .collect())
        })
    }

    fn create_session(&self, req: CreateSessionRequest) -> BoxFuture<Result<SessionSummaryDto>> {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let cwd = PathBuf::from(&req.cwd);
            if !cwd.is_dir() {
                return Err(HrnError::Config(format!(
                    "`cwd` is not a directory: {}",
                    cwd.display()
                )));
            }
            let meta = runtime.create_session(
                req.title,
                &cwd,
                req.model,
                req.thinking,
                req.permission_mode,
            )?;
            Ok(SessionSummaryDto {
                id: meta.id,
                title: meta.title,
                cwd: meta.cwd,
                model: meta.model,
                status: meta.status,
                active_turn: false,
                updated_at_ms: u64::try_from(meta.updated_at_ms).unwrap_or(0),
            })
        })
    }

    fn start_turn(&self, session_id: String, req: StartTurnRequest) -> BoxFuture<Result<()>> {
        let runtime = self.runtime.clone();
        let store = Arc::clone(&self.store);
        let chan = self.channel(&session_id);
        Box::pin(async move {
            if store.get_session(&session_id)?.is_none() {
                return Err(HrnError::Store(format!("unknown session `{session_id}`")));
            }
            let io = TurnIo {
                cancel: CancelFlag::new(),
                events: Arc::new(BroadcastEventSink {
                    session_id: session_id.clone(),
                    chan: Arc::clone(&chan),
                }),
                approval: Some(Arc::new(DaemonApprovalGate {
                    session_id: session_id.clone(),
                    auto_approve: req.auto_approve,
                    chan: Arc::clone(&chan),
                })),
                ask: None,
            };
            tokio::spawn(async move {
                let _ = runtime
                    .run_turn(&session_id, &req.prompt, &chan.control, io)
                    .await;
            });
            Ok(())
        })
    }

    fn steer(&self, session_id: String, text: String) -> BoxFuture<Result<()>> {
        let chan = self.channel(&session_id);
        chan.control.push_steer(text);
        Box::pin(async { Ok(()) })
    }

    fn interrupt(&self, session_id: String) -> BoxFuture<Result<()>> {
        let chan = self.channel(&session_id);
        chan.control.interrupt();
        Box::pin(async { Ok(()) })
    }

    fn approve(&self, session_id: String, req: ApproveRequest) -> BoxFuture<Result<()>> {
        let chan = self.channel(&session_id);
        let sender = chan
            .pending_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&req.call_id);
        match sender {
            Some(tx) => {
                let _ = tx.send(req.approved);
                Box::pin(async { Ok(()) })
            }
            None => Box::pin(async move {
                Err(HrnError::Tool(format!(
                    "no pending approval `{}` for session `{session_id}`",
                    req.call_id
                )))
            }),
        }
    }

    fn restore_session(
        &self,
        session_id: String,
        req: RestoreSessionRequest,
    ) -> BoxFuture<Result<bool>> {
        let runtime = self.runtime.clone();
        let chan = self.channel(&session_id);
        let already_running = self.is_active(&session_id);
        Box::pin(async move {
            if already_running {
                return Ok(false);
            }
            if !runtime.diagnose_session(&session_id)?.is_interrupted() {
                return Ok(false);
            }
            let io = TurnIo {
                cancel: CancelFlag::new(),
                events: Arc::new(BroadcastEventSink {
                    session_id: session_id.clone(),
                    chan: Arc::clone(&chan),
                }),
                approval: Some(Arc::new(DaemonApprovalGate {
                    session_id: session_id.clone(),
                    auto_approve: req.auto_approve,
                    chan: Arc::clone(&chan),
                })),
                ask: None,
            };
            tokio::spawn(async move {
                let _ = runtime.restore_turn(&session_id, &chan.control, io).await;
            });
            Ok(true)
        })
    }

    fn subscribe(&self, session_id: String) -> BoxFuture<Result<broadcast::Receiver<DaemonEvent>>> {
        let store = Arc::clone(&self.store);
        let chan = self.channel(&session_id);
        Box::pin(async move {
            if store.get_session(&session_id)?.is_none() {
                return Err(HrnError::Store(format!("unknown session `{session_id}`")));
            }
            Ok(chan.tx.subscribe())
        })
    }
}

/// Run the local loopback `hrn daemon` until `Ctrl+C`.
pub(crate) async fn run_daemon(
    cfg: &Config,
    db_path: &Path,
    run_file_path: &Path,
    host_key_path: &Path,
    listen: &str,
) -> Result<()> {
    let listen_addr: SocketAddr = listen
        .parse()
        .map_err(|e| HrnError::Config(format!("invalid --listen address `{listen}`: {e}")))?;
    let identity = ensure_host_identity(host_key_path)?;
    let token = generate_token()?;
    let store = open_store(db_path, false)?;
    let cwd = std::env::current_dir()?;
    let mut cfg = cfg.clone();
    let background = Background::new();
    let (runtime, mcp_manager, warnings) =
        build_runtime_with_mcp(&mut cfg, Arc::clone(&store), &cwd, &background).await?;
    for warn in warnings {
        err_line(&format!("warning: {warn}"));
    }

    let backend = Arc::new(RuntimeDaemonBackend::new(runtime, store));
    let handle = DaemonServer::bind(
        listen_addr,
        token.clone(),
        identity.fingerprint.clone(),
        backend,
    )
    .await?;
    let bound = handle.addr();
    let pid = std::process::id();
    let rf = RunFile {
        pid,
        addr: bound.to_string(),
        token,
        host_key: identity.fingerprint.clone(),
        started_at_ms: now_unix_ms(),
    };
    write_run_file(run_file_path, &rf)?;

    out(&format!(
        "hrn daemon listening on {bound}\nhost-key: {}\nrun-file: {}\nPress Ctrl+C to stop.\n",
        identity.fingerprint,
        run_file_path.display()
    ));

    let _ = tokio::signal::ctrl_c().await;
    out("\nshutting down hrn daemon...\n");
    handle.shutdown().await;
    mcp_manager.shutdown().await;
    remove_run_file_if_owned(run_file_path, pid);
    Ok(())
}

/// `hrn attach <session-or-ssh-url>`: stream live events and allow `/steer`, `/approve`, `/deny`,
/// `/interrupt`, or sending a prompt.
pub(crate) async fn attach(
    target: &str,
    run_file_path: &Path,
    trust_store_path: &Path,
) -> Result<()> {
    let (tunnel_guard, client, summary) = if target.starts_with("ssh://") {
        let remote = parse_remote_target(target)?;
        let trust_store = HostTrustStore::load(trust_store_path)?;
        let tunnel = SshTunnel::connect(&remote, &trust_store).await?;
        let client = tunnel.client().clone();
        let summary = resolve_session(&client, remote.session_id.as_deref()).await?;
        (Some(tunnel), client, summary)
    } else {
        let client = IpcClient::from_run_file(run_file_path)?;
        let _ = client.health().await?;
        let summary = resolve_session(&client, Some(target)).await?;
        (None, client, summary)
    };

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let session_id = handle_orphan_attach(&client, &summary, &mut lines).await?;

    out(&format!(
        "attached to session `{session_id}` (type a prompt, `/steer <msg>`, `/approve <call_id>`, `/deny <call_id>`, `/interrupt`, or `/quit`)\n"
    ));

    let mut events = client.subscribe_events(&session_id).await?;
    let event_task = tokio::spawn(async move {
        while let Ok(Some(ev)) = events.next_event().await {
            render_daemon_event(&ev);
        }
    });

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/quit" || trimmed == "/exit" {
            break;
        }
        if trimmed == "/interrupt" {
            if let Err(e) = client.interrupt(&session_id).await {
                err_line(&format!("error: {e}"));
            }
        } else if let Some(msg) = trimmed.strip_prefix("/steer ") {
            if let Err(e) = client.steer(&session_id, msg.trim().to_owned()).await {
                err_line(&format!("error: {e}"));
            }
        } else if let Some(call_id) = trimmed.strip_prefix("/approve ") {
            if let Err(e) = client
                .approve(&session_id, call_id.trim().to_owned(), true)
                .await
            {
                err_line(&format!("error: {e}"));
            }
        } else if let Some(call_id) = trimmed.strip_prefix("/deny ") {
            if let Err(e) = client
                .approve(&session_id, call_id.trim().to_owned(), false)
                .await
            {
                err_line(&format!("error: {e}"));
            }
        } else if let Err(e) = client
            .start_turn(
                &session_id,
                &StartTurnRequest {
                    prompt: trimmed.to_owned(),
                    auto_approve: false,
                },
            )
            .await
        {
            err_line(&format!("error: {e}"));
        }
    }

    event_task.abort();
    if let Some(tunnel) = tunnel_guard {
        tunnel.close().await;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OrphanChoice {
    Restore,
    ContinueViewing,
    NewSession,
}

async fn handle_orphan_attach(
    client: &IpcClient,
    summary: &SessionSummaryDto,
    lines: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
) -> Result<String> {
    let session_id = summary.id.clone();
    if summary.status != AgentStatus::Working || summary.active_turn {
        return Ok(session_id);
    }
    out(&format!(
        "session `{session_id}` is marked working with no live owner · [r]estore · [c]ontinue viewing · [n]ew: "
    ));
    let answer = lines.next_line().await.ok().flatten().unwrap_or_default();
    match parse_orphan_choice(&answer) {
        OrphanChoice::Restore => {
            if client.restore_session(&session_id, false).await? {
                out("restored: going on with the interrupted turn…\n");
            } else {
                out("restored: nothing was left unfinished\n");
            }
            Ok(session_id)
        }
        OrphanChoice::ContinueViewing => {
            out("continuing in view mode\n");
            Ok(session_id)
        }
        OrphanChoice::NewSession => {
            let created = client
                .create_session(&CreateSessionRequest {
                    title: "attached session".to_owned(),
                    cwd: summary.cwd.clone(),
                    model: summary.model.clone(),
                    thinking: ThinkingLevel::default(),
                    permission_mode: PermissionMode::default(),
                })
                .await?;
            out(&format!("started new session `{}`\n", created.id));
            Ok(created.id)
        }
    }
}

fn parse_orphan_choice(raw: &str) -> OrphanChoice {
    match raw.trim().to_ascii_lowercase().as_str() {
        "r" | "restore" => OrphanChoice::Restore,
        "n" | "new" => OrphanChoice::NewSession,
        _ => OrphanChoice::ContinueViewing,
    }
}

async fn resolve_session(client: &IpcClient, wanted: Option<&str>) -> Result<SessionSummaryDto> {
    let sessions = client.list_sessions().await?;
    match wanted.map(str::trim).filter(|s| !s.is_empty()) {
        Some(prefix) => {
            let matches: Vec<&SessionSummaryDto> = sessions
                .iter()
                .filter(|s| s.id == prefix || s.id.starts_with(prefix))
                .collect();
            match matches.as_slice() {
                [only] => Ok((*only).clone()),
                [] => Err(HrnError::Store(format!(
                    "no daemon session matching `{prefix}`"
                ))),
                _ => Err(HrnError::Config(format!(
                    "ambiguous session prefix `{prefix}` ({} matches)",
                    matches.len()
                ))),
            }
        }
        None => sessions
            .into_iter()
            .next()
            .ok_or_else(|| HrnError::Store("remote daemon has no sessions yet".to_owned())),
    }
}

fn render_daemon_event(ev: &DaemonEvent) {
    match &ev.event {
        AgentEvent::TextDelta { delta } => out(delta),
        AgentEvent::ToolStart {
            name, arguments, ..
        } => {
            let preview: String = arguments.chars().take(120).collect();
            out(&format!("\n[tool:{name}] {preview}\n"));
        }
        AgentEvent::ToolDone { name, is_error, .. } => {
            let tag = if *is_error { "ERR" } else { "ok" };
            out(&format!("[tool:{name} {tag}]\n"));
        }
        AgentEvent::ApprovalRequested { request } => {
            out(&format!(
                "\n[approval needed] id={} tool={} — {} (use `/approve {}` or `/deny {}`)\n",
                request.id, request.tool_name, request.summary, request.id, request.id
            ));
        }
        AgentEvent::AdvisorNote { model, note } => {
            out(&format!("\n[advisor · {model}]\n{note}\n"));
        }
        AgentEvent::AssistantNote { text } if !text.is_empty() => {
            out(&format!("\n{text}\n"));
        }
        AgentEvent::TurnDone { steps, .. } => {
            out(&format!("\n[turn done in {steps} steps]\n"));
        }
        AgentEvent::TurnError { code, message, .. } => {
            err_line(&format!("[turn error {code}] {message}"));
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
