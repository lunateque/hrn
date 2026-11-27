//! Test-only `Backend`: scripted turns, recorded calls, no runtime.

use crate::backend::{
    Backend, CommandInfo, CompactionReport, ModelEntry, NewSession, SessionPatch, TurnRequest,
    TurnSummary,
};
use hrn_kernel::{
    AgentEvent, ApprovalGate, ApprovalRequest, AskHandler, AskRequest, BoxFuture, CancelFlag,
    DagNode, EventSink, FinishReason, HrnError, PermissionMode, Result, SessionMeta, ThinkingLevel,
    Usage,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, oneshot};

/// The model the mock serves.
pub(crate) fn entry() -> ModelEntry {
    ModelEntry {
        alias: "fake".to_owned(),
        provider: "mock".to_owned(),
        id: "m1".to_owned(),
        name: "Mock One".to_owned(),
        context_window: 1000,
        reasoning: false,
        input_price: None,
        output_price: None,
    }
}

/// A settled turn's outcome.
pub(crate) fn done(text: &str) -> TurnSummary {
    TurnSummary {
        text: text.to_owned(),
        ..Default::default()
    }
}

/// The events of a one-step answer.
pub(crate) fn answer(text: &str) -> Vec<AgentEvent> {
    vec![
        AgentEvent::TurnStart {
            turn_id: "t".to_owned(),
            user_text: String::new(),
        },
        AgentEvent::TextDelta {
            delta: text.to_owned(),
        },
        AgentEvent::LlmCall {
            model: "m1".to_owned(),
            first_token_ms: Some(1),
            duration_ms: 10,
            finish_reason: FinishReason::Stop,
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            }),
        },
        AgentEvent::TurnDone {
            turn_id: "t".to_owned(),
            model: "m1".to_owned(),
            text: text.to_owned(),
            usage: Some(Usage::default()),
            steps: 1,
        },
    ]
}

/// An approval request for tool `name`.
pub(crate) fn approval(id: &str, name: &str) -> ApprovalRequest {
    ApprovalRequest {
        id: id.to_owned(),
        session_id: "session-1".to_owned(),
        tool_name: name.to_owned(),
        summary: format!("run {name}"),
        diff: None,
        reason: None,
        targets: Vec::new(),
        choices: Vec::new(),
        tier: None,
    }
}

/// What the next turns do.
#[derive(Clone)]
pub(crate) enum Script {
    /// Put `approvals` to the turn's approval gate and `forms` to its ask handler, emit each
    /// event through the sink, then settle.
    Events {
        approvals: Vec<ApprovalRequest>,
        forms: Vec<AskRequest>,
        events: Vec<AgentEvent>,
        summary: TurnSummary,
    },
    /// Put the dialogs, emit the events, then hold the turn until the test releases it.
    Hold {
        approvals: Vec<ApprovalRequest>,
        forms: Vec<AskRequest>,
        events: Vec<AgentEvent>,
    },
    /// Settle without emitting anything.
    Done(TurnSummary),
    /// Fail the turn.
    Fail(String),
}

/// What the runtime and the front's gates recorded.
#[derive(Default)]
pub(crate) struct Calls {
    pub(crate) prompts: Vec<(String, String)>,
    pub(crate) steers: Vec<String>,
    pub(crate) interrupts: usize,
    pub(crate) compactions: usize,
    pub(crate) patches: Vec<(String, SessionPatch)>,
    /// Approval decisions the front's gate returned.
    pub(crate) decisions: Vec<bool>,
    /// Submissions the front's ask handler returned.
    pub(crate) submissions: Vec<Value>,
    /// Held turns that finished (released or cancelled).
    pub(crate) held_done: usize,
    resume: Vec<oneshot::Sender<TurnSummary>>,
}

/// A `Backend` whose behavior tests program through [`Mock::script`].
pub(crate) struct Mock {
    /// What the next turns do, newest first.
    script: Mutex<std::collections::VecDeque<Script>>,
    /// What the turns did.
    calls: Arc<Mutex<Calls>>,
    /// Woken whenever a turn starts, so tests can wait for one.
    turn_started: Notify,
    sessions: AtomicUsize,
    /// Whether background work can still wake a session.
    pending_async: AtomicBool,
    fast_supported: AtomicBool,
    slow_supported: AtomicBool,
    fast_enabled: AtomicBool,
    slow_enabled: AtomicBool,
}

impl Mock {
    pub(crate) fn new(script: Vec<Script>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            calls: Arc::new(Mutex::new(Calls::default())),
            turn_started: Notify::new(),
            sessions: AtomicUsize::new(0),
            pending_async: AtomicBool::new(false),
            fast_supported: AtomicBool::new(true),
            slow_supported: AtomicBool::new(false),
            fast_enabled: AtomicBool::new(false),
            slow_enabled: AtomicBool::new(false),
        }
    }

    pub(crate) fn set_fast_supported(&self, supported: bool) {
        self.fast_supported.store(supported, Ordering::Relaxed);
    }

    pub(crate) fn set_slow_supported(&self, supported: bool) {
        self.slow_supported.store(supported, Ordering::Relaxed);
    }

    /// Wait until `n` turns have started.
    pub(crate) async fn wait_prompts(&self, n: usize) {
        loop {
            let live = self.with_calls(|c| c.prompts.len());
            if live >= n {
                return;
            }
            self.turn_started.notified().await;
        }
    }

    /// Settle the newest held turn and wait for its future to finish.
    pub(crate) fn release_held(&self, summary: TurnSummary) {
        let sender = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resume
            .pop();
        if let Some(sender) = sender {
            let _ = sender.send(summary);
        }
    }

    /// What the front asked the runtime to do.
    pub(crate) fn with_calls<T>(&self, f: impl FnOnce(&Calls) -> T) -> T {
        let calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&calls)
    }

    /// Let background work wake the session (or not).
    pub(crate) fn set_pending_async(&self, pending: bool) {
        self.pending_async.store(pending, Ordering::Relaxed);
    }
}

/// One dialog outcome to record.
enum Note {
    Approved(bool),
    Submitted(Value),
}

impl Calls {
    fn apply(&mut self, note: Note) {
        match note {
            Note::Approved(ok) => self.decisions.push(ok),
            Note::Submitted(values) => self.submissions.push(values),
        }
    }
}

impl Default for Mock {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

fn meta(n: usize, cwd: &Path) -> SessionMeta {
    SessionMeta {
        id: format!("session-{n}"),
        title: String::new(),
        cwd: cwd.to_string_lossy().into_owned(),
        branch: None,
        head_node_id: None,
        model: Some("fake".to_owned()),
        thinking: ThinkingLevel::Off,
        goal: None,
        permission_mode: PermissionMode::Default,
        parent_session_id: None,
        parent_node_id: None,
        status: hrn_kernel::AgentStatus::Idle,
        created_at_ms: 0,
        updated_at_ms: 0,
        pinned: false,
        title_source: None,
        exit_ms: None,
    }
}

/// Put the scripted dialogs to the gates the front attached, recording what they decided.
async fn dialogs(
    events: &Arc<dyn EventSink>,
    approval: Option<&Arc<dyn ApprovalGate>>,
    ask: Option<&Arc<dyn AskHandler>>,
    requests: &[ApprovalRequest],
    forms: &[AskRequest],
    cancel: &CancelFlag,
    log: &Arc<Mutex<Calls>>,
) -> Result {
    for request in requests {
        if let Some(gate) = approval {
            let approved = gate
                .await_decision(request.clone(), cancel.clone())
                .await?
                .is_approved();
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .apply(Note::Approved(approved));
            events
                .emit(AgentEvent::ApprovalResolved {
                    id: request.id.clone(),
                    approved,
                })
                .await;
        }
    }
    for form in forms {
        if let Some(handler) = ask {
            let answer = handler.ask(form.clone(), cancel.clone()).await?;
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .apply(Note::Submitted(answer.values));
        }
    }
    Ok(())
}

impl Backend for Mock {
    fn models(&self) -> Vec<ModelEntry> {
        vec![entry()]
    }

    fn find_model(&self, selector: &str) -> Result<ModelEntry> {
        if selector == "mock/m1" || selector == "fake" {
            Ok(entry())
        } else {
            Err(HrnError::Config(format!("unknown model `{selector}`")))
        }
    }

    fn default_model(&self) -> Result<ModelEntry> {
        Ok(entry())
    }

    fn default_thinking(&self) -> ThinkingLevel {
        ThinkingLevel::Off
    }

    fn default_mode(&self) -> PermissionMode {
        PermissionMode::Default
    }

    fn cwd(&self) -> PathBuf {
        PathBuf::from(".")
    }

    fn create_session(&self, spec: NewSession) -> Result<SessionMeta> {
        let n = self.sessions.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(meta(n, &spec.cwd))
    }

    fn find_session(&self, wanted: &str) -> Result<SessionMeta> {
        wanted
            .rsplit('-')
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .map_or_else(
                || Err(HrnError::Config(format!("unknown session `{wanted}`"))),
                |n| Ok(meta(n, Path::new("."))),
            )
    }

    fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        Ok(vec![meta(1, Path::new("."))])
    }

    fn patch_session(&self, id: &str, patch: SessionPatch) -> Result<SessionMeta> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .patches
            .push((id.to_owned(), patch.clone()));
        let mut base = self.find_session(id)?;
        base.model = patch.model.or(base.model);
        if let Some(thinking) = patch.thinking {
            base.thinking = thinking;
        }
        if let Some(mode) = patch.mode {
            base.permission_mode = mode;
        }
        if let Some(title) = patch.title {
            base.title = title.clone();
        }
        Ok(base)
    }

    fn history(&self, _id: &str) -> Result<Vec<DagNode>> {
        Ok(Vec::new())
    }

    fn fork(&self, id: &str, _node: Option<&str>) -> Result<SessionMeta> {
        let n = id
            .rsplit('-')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        Ok(meta(n + 100, Path::new(".")))
    }

    fn usage(&self, _id: &str) -> Result<Usage> {
        Ok(Usage::default())
    }

    fn model_by_alias(&self, alias: &str) -> Option<ModelEntry> {
        (alias == "fake").then(entry)
    }

    fn tools(&self) -> Vec<Value> {
        vec![json!({ "name": "read", "description": "read", "parameters": {} })]
    }

    fn model_rows(&self, current: &str) -> Vec<Value> {
        vec![crate::backend::model_row_json(&entry(), current)]
    }

    fn model_json(&self, alias: &str) -> Value {
        self.model_by_alias(alias).map_or_else(
            || json!({ "alias": alias, "id": alias, "current": false }),
            |m| crate::backend::model_row_json(&m, &m.alias),
        )
    }

    fn context_window(&self, alias: &str) -> Option<u64> {
        (alias == "fake").then_some(1000)
    }

    fn pending_async(&self, _id: &str) -> bool {
        self.pending_async.load(Ordering::Relaxed)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per script shape; the test mock mirrors the runtime's turn menu"
    )]
    fn run_turn(&self, request: TurnRequest) -> BoxFuture<Result<TurnSummary>> {
        let script = {
            let mut queue = self
                .script
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.pop_front()
        };
        {
            let mut calls = self
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            calls
                .prompts
                .push((request.session_id.clone(), request.prompt.clone()));
        }
        self.turn_started.notify_waiters();
        let events: Arc<dyn EventSink> = request.events;
        let approval = request.approval;
        let ask = request.ask;
        let cancel = request.cancel;
        let log = Arc::clone(&self.calls);
        let holder = log.clone();
        match script {
            None => Box::pin(async move {
                Err(HrnError::Config(
                    "the mock has no scripted turn left".to_owned(),
                ))
            }),
            Some(Script::Done(summary)) => Box::pin(async move { Ok(summary) }),
            Some(Script::Fail(message)) => Box::pin(async move {
                events
                    .emit(AgentEvent::TurnError {
                        turn_id: "t".to_owned(),
                        code: "provider".to_owned(),
                        message: message.clone(),
                    })
                    .await;
                Err(HrnError::Provider {
                    message,
                    retryable: false,
                    retry_after_secs: None,
                })
            }),
            Some(Script::Events {
                approvals,
                forms,
                events: list,
                summary,
            }) => Box::pin(async move {
                dialogs(
                    &events,
                    approval.as_ref(),
                    ask.as_ref(),
                    &approvals,
                    &forms,
                    &cancel,
                    &log,
                )
                .await?;
                for event in list {
                    events.emit(event).await;
                }
                Ok(summary)
            }),
            Some(Script::Hold {
                approvals,
                forms,
                events: list,
            }) => {
                let (tx, rx) = oneshot::channel();
                log.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .resume
                    .push(tx);
                Box::pin(async move {
                    dialogs(
                        &events,
                        approval.as_ref(),
                        ask.as_ref(),
                        &approvals,
                        &forms,
                        &cancel,
                        &log,
                    )
                    .await?;
                    for event in list {
                        events.emit(event).await;
                    }
                    let outcome = tokio::select! {
                        summary = rx => summary.map_err(|_| HrnError::Cancelled),
                        () = cancel.cancelled() => Err(HrnError::Cancelled),
                    };
                    holder
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .held_done += 1;
                    outcome
                })
            }
        }
    }

    fn steer(&self, _id: &str, text: String) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .steers
            .push(text);
    }

    fn interrupt(&self, _id: &str) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .interrupts += 1;
    }

    fn take_steers(&self, _id: &str) -> Vec<String> {
        Vec::new()
    }

    fn compact(
        &self,
        _id: &str,
        cancel: CancelFlag,
        _events: Arc<dyn EventSink>,
    ) -> BoxFuture<Result<Option<CompactionReport>>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .compactions += 1;
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(HrnError::Cancelled);
            }
            Ok(Some(CompactionReport {
                first_kept: "n1".to_owned(),
                summary: "summary".to_owned(),
                saved_chars: 5,
                tokens_before: 900,
            }))
        })
    }

    fn export_html(&self, _id: &str, _path: Option<&Path>) -> Result<PathBuf> {
        Ok(PathBuf::from("export.html"))
    }

    fn commands(&self, _cwd: &Path) -> Vec<CommandInfo> {
        vec![CommandInfo {
            name: "fix".to_owned(),
            description: "fix it".to_owned(),
        }]
    }

    fn expand_command(&self, _cwd: &Path, name: &str, args: &str) -> Option<String> {
        (name == "fix").then(|| format!("fix: {args}"))
    }

    fn system_prompt(&self, _id: &str) -> Result<Vec<String>> {
        Ok(vec!["base".to_owned()])
    }

    fn shutdown(&self) -> BoxFuture<()> {
        Box::pin(async {})
    }

    fn supports_service_tier(&self, _alias: &str) -> bool {
        self.fast_supported.load(Ordering::Relaxed)
    }

    fn supports_slow_mode(&self, _alias: &str) -> bool {
        self.slow_supported.load(Ordering::Relaxed)
    }

    fn set_fast_mode(&self, enabled: bool) {
        self.fast_enabled.store(enabled, Ordering::Relaxed);
    }

    fn set_slow_mode(&self, enabled: bool) {
        self.slow_enabled.store(enabled, Ordering::Relaxed);
    }

    fn fast_mode_enabled(&self) -> bool {
        self.fast_enabled.load(Ordering::Relaxed)
    }

    fn slow_mode_enabled(&self) -> bool {
        self.slow_enabled.load(Ordering::Relaxed)
    }
}
