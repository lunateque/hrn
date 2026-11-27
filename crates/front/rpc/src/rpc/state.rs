//! The connection's own view of its session: startup defaults, `get_state`, and the model and
//! queue projections a host reads.

use super::Core;
use crate::backend::{Backend, NewSession, SessionPatch};
use crate::events::usage_json;
use crate::history;
use crate::wire::put;
use hrn_kernel::{DagNode, PermissionMode, Result, ThinkingLevel, TodoStatus, Usage};

use serde_json::{Value, json};

/// The context-window default for a model that does not say.
const UNKNOWN_WINDOW: u64 = 200_000;

/// The thinking levels `hrn` accepts, `off` first, as omp's discovery orders them.
pub(crate) fn thinking_levels() -> Vec<&'static str> {
    [
        ThinkingLevel::Off,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
    ]
    .iter()
    .copied()
    .map(ThinkingLevel::as_str)
    .collect()
}

/// One todo's status as omp spells it.
const fn todo_status(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
        TodoStatus::Abandoned => "abandoned",
    }
}

/// The context-usage block, from the session's newest recorded step.
fn context_usage(backend: &dyn Backend, model: &str, last: Option<&Usage>) -> Option<Value> {
    let usage = last?;
    if usage.total_tokens == 0 {
        return None;
    }
    let window = backend.context_window(model).unwrap_or(UNKNOWN_WINDOW);
    let tokens = usage.prompt_tokens + usage.completion_tokens;
    #[expect(clippy::cast_precision_loss, reason = "a percentage is displayed")]
    let percent = tokens as f64 * 100.0 / window as f64;
    Some(json!({
        "tokens": tokens,
        "contextWindow": window,
        "percent": (percent * 100.0).round() / 100.0,
    }))
}

impl Core {
    /// The connection's session, created on demand: the first prompt needs a row, a host that only
    /// inspects state never gets one.
    ///
    /// # Errors
    /// The store refused to create or read the session.
    pub(crate) fn session(&self, create: bool) -> Result<Option<String>> {
        let existing = self.lock().session.clone();
        if existing.is_some() || !create {
            return Ok(existing);
        }
        let (cwd, title, model, thinking, mode) = {
            let state = self.lock();
            (
                state.cwd.clone(),
                state
                    .session_name
                    .clone()
                    .unwrap_or_else(|| "rpc session".to_owned()),
                state.model.clone(),
                state.thinking,
                state.mode,
            )
        };
        let meta = self.backend.create_session(NewSession {
            cwd,
            title,
            model,
            thinking,
            mode,
        })?;
        let id = meta.id.clone();
        self.lock().session = Some(id.clone());
        Ok(Some(id))
    }

    /// Push the connection's settings onto its stored session; the runtime reads them from there.
    ///
    /// # Errors
    /// The session is unknown or the store refused the update.
    pub(crate) fn sync(&self) -> Result {
        let Some(id) = self.session(true)? else {
            return Ok(());
        };
        let state = self.lock();
        self.backend
            .patch_session(
                &id,
                SessionPatch {
                    model: Some(state.model.clone()),
                    thinking: Some(state.thinking),
                    mode: Some(state.mode),
                    title: state.session_name.clone(),
                },
            )
            .map(|_| ())
    }

    /// The branch of the connection's session; empty before it has one.
    ///
    /// # Errors
    /// The session could not be read.
    pub(crate) fn branch(&self) -> Result<Vec<DagNode>> {
        match self.session(false)? {
            Some(id) => self.backend.history(&id),
            None => Ok(Vec::new()),
        }
    }

    /// Slash commands as the host sees them.
    pub(crate) fn command_rows(&self) -> Vec<Value> {
        let cwd = self.lock().cwd.clone();
        self.backend
            .commands(&cwd)
            .into_iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "source": "custom",
                    "description": c.description,
                    "input": { "hint": "[args]" },
                })
            })
            .collect()
    }

    /// The `get_state` payload.
    ///
    /// # Errors
    /// The session or its store could not be read.
    pub(crate) fn state_payload(&self) -> Result<Value> {
        let (snapshot, last_usage) = {
            let state = self.lock();
            let snapshot = StateSnapshot {
                session: state.session.clone(),
                model: state.model.clone(),
                thinking: state.thinking,
                mode: state.mode,
                steering: state.steering.clone(),
                follow_up: state.follow_up.clone(),
                steering_mode: state.steering_mode.clone(),
                follow_up_mode: state.follow_up_mode.clone(),
                interrupt_mode: state.interrupt_mode.clone(),
                auto_compaction: state.auto_compaction,
                session_name: state.session_name.clone(),
                todos: state.todos.clone(),
                streaming: state.run_active,
                compacting: state.compacting,
                speed: state.translator.last_speed(),
                fast_mode_enabled: state.fast_mode_enabled,
                slow_mode_enabled: state.slow_mode_enabled,
            };
            (snapshot, state.translator.last_usage())
        };
        let usage = match snapshot.session.as_deref() {
            Some(id) => self.backend.usage(id)?,
            None => Usage::default(),
        };
        let nodes = self.branch()?;
        let todo_phases: Vec<Value> = snapshot
            .todos
            .iter()
            .map(|phase| {
                json!({
                    "name": phase.name,
                    "tasks": phase.tasks.iter().map(|t| json!({
                        "content": t.content,
                        "status": todo_status(t.status),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let system_prompt = snapshot
            .session
            .as_deref()
            .and_then(|id| self.backend.system_prompt(id).ok());
        let pending = snapshot
            .session
            .as_deref()
            .is_some_and(|id| self.backend.pending_async(id));
        let supports_fast = self.backend.supports_service_tier(&snapshot.model);
        let supports_slow = self.backend.supports_slow_mode(&snapshot.model);
        let fast_active = snapshot.fast_mode_enabled && supports_fast;
        let mut payload = json!({
            "model": self.backend.model_json(&snapshot.model),
            "thinkingLevel": snapshot.thinking.as_str(),
            "configuredThinkingLevel": snapshot.thinking.as_str(),
            "thinkingLevels": thinking_levels(),
            "isStreaming": snapshot.streaming,
            "isCompacting": snapshot.compacting,
            "steeringMode": snapshot.steering_mode,
            "followUpMode": snapshot.follow_up_mode,
            "interruptMode": snapshot.interrupt_mode,
            "permissionMode": snapshot.mode.as_str(),
            "sessionName": snapshot.session_name,
            "autoCompactionEnabled": snapshot.auto_compaction,
            "messageCount": history::messages(&nodes).len(),
            "queuedMessageCount": snapshot.steering.len() + snapshot.follow_up.len(),
            "queuedMessages": {
                "steering": snapshot.steering,
                "followUp": snapshot.follow_up,
            },
            "todoPhases": todo_phases,
            "usage": usage_json(Some(&usage)),
            "fastModeEnabled": snapshot.fast_mode_enabled,
            "fastModeActive": fast_active,
            "slowModeSupported": supports_slow,
            "slowModeEnabled": snapshot.slow_mode_enabled,
            "tokensPerSecond": snapshot.speed,
            "hasPendingAsyncWork": pending,
            "isSettled": self.is_settled(),
            "dumpTools": self.backend.tools(),
            "systemPrompt": system_prompt,
        });
        if let Some(usage_block) =
            context_usage(self.backend.as_ref(), &snapshot.model, last_usage.as_ref())
        {
            put(&mut payload, "contextUsage", usage_block);
        }
        if let Some(id) = snapshot.session {
            put(&mut payload, "sessionId", json!(id));
        }
        Ok(payload)
    }
}

/// What `get_state` reads, collected under one lock.
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors the independent boolean flags in RPC State"
)]
struct StateSnapshot {
    session: Option<String>,
    model: String,
    thinking: ThinkingLevel,
    mode: PermissionMode,
    steering: Vec<String>,
    follow_up: Vec<String>,
    steering_mode: String,
    follow_up_mode: String,
    interrupt_mode: String,
    auto_compaction: bool,
    session_name: Option<String>,
    todos: Vec<hrn_kernel::TodoPhase>,
    streaming: bool,
    compacting: bool,
    speed: Option<f64>,
    fast_mode_enabled: bool,
    slow_mode_enabled: bool,
}
