//! The seam between the wire protocols and the agent runtime: the binary implements [`Backend`]
//! over its `AgentRuntime`; the protocol fronts depend on nothing else of it.

use hrn_kernel::{
    ApprovalGate, AskHandler, BoxFuture, CancelFlag, DagNode, EventSink, HarnessHook,
    ImageAttachment, PermissionMode, Result, SessionMeta, ThinkingLevel, Usage,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One selectable model, as the host sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelEntry {
    /// The runtime's name for it (the preset key); what a session stores.
    pub alias: String,
    /// Provider key.
    pub provider: String,
    /// Upstream model id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Context window in tokens.
    pub context_window: u64,
    /// Whether the model reasons.
    pub reasoning: bool,
    /// USD per million input tokens, when priced.
    pub input_price: Option<f64>,
    /// USD per million output tokens, when priced.
    pub output_price: Option<f64>,
}

/// A model row as a host sees it; `current` marks the row in use.
pub fn model_row_json(entry: &ModelEntry, current: &str) -> Value {
    serde_json::json!({
        "alias": entry.alias,
        "provider": entry.provider,
        "id": entry.id,
        "name": entry.name,
        "contextWindow": entry.context_window,
        "reasoning": entry.reasoning,
        "input": ["text"],
        "compat": [],
        "current": entry.alias == current,
        "cost": {
            "input": entry.input_price,
            "output": entry.output_price,
            "cacheRead": Value::Null,
            "cacheWrite": Value::Null,
        },
    })
}

/// A slash command the host may offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandInfo {
    /// Name without the slash.
    pub name: String,
    /// One-line description.
    pub description: String,
}

/// Everything needed to start a session.
#[derive(Clone, Debug)]
pub struct NewSession {
    /// Working directory of the session.
    pub cwd: PathBuf,
    /// Title (the first prompt, shortened).
    pub title: String,
    /// Model alias.
    pub model: String,
    /// Thinking effort.
    pub thinking: ThinkingLevel,
    /// Permission mode.
    pub mode: PermissionMode,
}

/// Settings changed on a live or pending session; `None` keeps the stored value.
#[derive(Clone, Debug, Default)]
pub struct SessionPatch {
    /// New model alias.
    pub model: Option<String>,
    /// New thinking effort.
    pub thinking: Option<ThinkingLevel>,
    /// New permission mode.
    pub mode: Option<PermissionMode>,
    /// New title.
    pub title: Option<String>,
}

/// One turn to run.
pub struct TurnRequest {
    /// Session to run in.
    pub session_id: String,
    /// The user message.
    pub prompt: String,
    /// Attached images.
    pub images: Vec<ImageAttachment>,
    /// Cancellation of the whole run.
    pub cancel: CancelFlag,
    /// Where runtime events go.
    pub events: Arc<dyn EventSink>,
    /// Who answers tool approvals; `None` refuses them.
    pub approval: Option<Arc<dyn ApprovalGate>>,
    /// Who answers `ask` forms; `None` makes the tool report that nobody can answer.
    pub ask: Option<Arc<dyn AskHandler>>,
    /// Extra hooks for this run only.
    pub hooks: Vec<Arc<dyn HarnessHook>>,
}

/// How a settled run went.
#[derive(Clone, Debug, Default)]
pub struct TurnSummary {
    /// The final assistant text.
    pub text: String,
    /// Usage the run's turns added up to.
    pub usage: Usage,
    /// The run was cancelled before it settled.
    pub aborted: bool,
    /// The run ended on a model or tool failure rather than an answer.
    pub failed: bool,
}

/// What a manual compaction did.
#[derive(Clone, Debug)]
pub struct CompactionReport {
    /// The newest node the summary covers.
    pub first_kept: String,
    /// The summary text.
    pub summary: String,
    /// Characters the summary saved.
    pub saved_chars: u64,
    /// Context tokens before the summary replaced the covered nodes.
    pub tokens_before: u64,
}

/// The agent runtime as the protocol fronts use it.
pub trait Backend: Send + Sync + 'static {
    /// Selectable models, in cycle order.
    fn models(&self) -> Vec<ModelEntry>;

    /// A model from `provider/id`, a preset key or a role.
    ///
    /// # Errors
    /// The selector names no configured model.
    fn find_model(&self, selector: &str) -> Result<ModelEntry>;

    /// The model a fresh session starts with.
    ///
    /// # Errors
    /// No model is configured.
    fn default_model(&self) -> Result<ModelEntry>;

    /// The thinking effort a fresh session starts with.
    fn default_thinking(&self) -> ThinkingLevel;

    /// The permission mode a fresh session starts with.
    fn default_mode(&self) -> PermissionMode;

    /// The directory the process started in.
    fn cwd(&self) -> PathBuf;

    /// Create a session.
    ///
    /// # Errors
    /// The store refused the session.
    fn create_session(&self, spec: NewSession) -> Result<SessionMeta>;

    /// A stored session by exact id or unique id prefix.
    ///
    /// # Errors
    /// No session, or several, match.
    fn find_session(&self, wanted: &str) -> Result<SessionMeta>;

    /// Stored sessions, newest first.
    ///
    /// # Errors
    /// The store could not be read.
    fn list_sessions(&self) -> Result<Vec<SessionMeta>>;

    /// Apply `patch` to a stored session and to its live control.
    ///
    /// # Errors
    /// The session is unknown or the store refused the update.
    fn patch_session(&self, id: &str, patch: SessionPatch) -> Result<SessionMeta>;

    /// The conversation on the session's current branch, oldest first.
    ///
    /// # Errors
    /// The session is unknown or the store could not be read.
    fn history(&self, id: &str) -> Result<Vec<DagNode>>;

    /// Start a new session from `node` (or the head) of `id`.
    ///
    /// # Errors
    /// The session or node is unknown.
    fn fork(&self, id: &str, node: Option<&str>) -> Result<SessionMeta>;

    /// The model behind a stored session, by its runtime name.
    fn model_by_alias(&self, alias: &str) -> Option<ModelEntry>;

    /// The tools the agent can call, as `{name, description, parameters}` rows.
    fn tools(&self) -> Vec<Value>;

    /// Every model as the host sees it, in cycle order; the row for `current` says so.
    fn model_rows(&self, current: &str) -> Vec<Value>;

    /// One model as the host sees it; an unknown alias reports just its id.
    fn model_json(&self, alias: &str) -> Value;

    /// The context window of a stored model alias, when the preset says.
    fn context_window(&self, alias: &str) -> Option<u64>;

    /// Whether outside work (a background job or a running child agent) can still wake
    /// `id` and start another turn.
    fn pending_async(&self, id: &str) -> bool;

    /// Everything the session used so far.
    ///
    /// # Errors
    /// The store could not be read.
    fn usage(&self, id: &str) -> Result<Usage>;

    /// Run `request.prompt` and anything steered in after its last step; resolves when the
    /// session is settled.
    fn run_turn(&self, request: TurnRequest) -> BoxFuture<Result<TurnSummary>>;

    /// Queue `text` for the session's next step boundary.
    fn steer(&self, id: &str, text: String);

    /// Stop the session's running turn at once.
    fn interrupt(&self, id: &str);

    /// Take the steering the run never picked up.
    fn take_steers(&self, id: &str) -> Vec<String>;

    /// Summarize older context; `None` when there was nothing to summarize.
    fn compact(
        &self,
        id: &str,
        cancel: CancelFlag,
        events: Arc<dyn EventSink>,
    ) -> BoxFuture<Result<Option<CompactionReport>>>;

    /// Write an HTML export of the session; returns where it went.
    ///
    /// # Errors
    /// The session is unknown or the file could not be written.
    fn export_html(&self, id: &str, path: Option<&Path>) -> Result<PathBuf>;

    /// Slash commands available in `cwd`.
    fn commands(&self, cwd: &Path) -> Vec<CommandInfo>;

    /// The prompt a slash command expands to; `None` when `name` is not a command.
    fn expand_command(&self, cwd: &Path, name: &str, args: &str) -> Option<String>;

    /// The system prompt the next turn of `id` would send.
    ///
    /// # Errors
    /// The prompt could not be built.
    fn system_prompt(&self, id: &str) -> Result<Vec<String>>;

    /// Release what the runtime holds (MCP servers); called once at exit.
    fn shutdown(&self) -> BoxFuture<()>;

    /// Clear session-scoped and persisted cross-session approval grants (`/forget-approvals`).
    ///
    /// # Errors
    /// Clearing persisted grants failed.
    fn forget_approvals(&self) -> Result {
        Ok(())
    }

    /// Whether `alias` supports service tiers (`set_fast_mode`).
    fn supports_service_tier(&self, _alias: &str) -> bool {
        false
    }

    /// Whether `alias` supports Anthropic subscription slow mode (`set_slow_mode`).
    fn supports_slow_mode(&self, _alias: &str) -> bool {
        false
    }

    /// Toggle session-scoped priority fast mode (non-persisted).
    fn set_fast_mode(&self, _enabled: bool) {}

    /// Toggle session-scoped Anthropic slow mode (non-persisted).
    fn set_slow_mode(&self, _enabled: bool) {}

    /// Whether fast mode is currently enabled on the backend.
    fn fast_mode_enabled(&self) -> bool {
        false
    }

    /// Whether slow mode is currently enabled on the backend.
    fn slow_mode_enabled(&self) -> bool {
        false
    }
}
