//! Event-sourced DAG session types, agent commands, and streaming events.

use crate::form::Form;
use crate::goal::Goal;
use crate::ir::{Msg, Usage};
use crate::thinking::ThinkingLevel;
pub use crate::tier::PermissionMode;
use crate::tier::TitleSource;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Unique identifier for a session (primary agent or child subagent).
pub type SessionId = String;
/// Unique identifier for a node inside a session's append-only DAG.
pub type NodeId = String;
/// Unique identifier for one user-initiated agent turn.
pub type TurnId = String;

/// Why a session turn or process terminated (recorded in [`DagPayload::SessionExit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// Turn completed cleanly.
    Normal,
    /// User interrupted the turn (final; never auto-restored).
    Interrupted,
    /// Process crashed, panicked, or encountered a fatal abort mid-turn.
    Fatal,
}

/// One in-flight tool call recorded when a turn exited abnormally.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingToolCall {
    /// Tool call identifier.
    pub id: String,
    /// Registered tool name.
    pub name: String,
    /// Raw JSON arguments string.
    pub arguments: String,
}

/// Live operational status of a primary agent session (also mapped to Herdr states).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Waiting for user input.
    #[default]
    Idle,
    /// Streaming LLM tokens or executing tools.
    Working,
    /// Blocked waiting for approval or user answer to `ask`.
    Blocked,
    /// Last turn ended with an error.
    Error,
}

/// Payload stored inside an immutable DAG node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DagPayload {
    /// Canonical conversation message (user, assistant, tool result).
    Message {
        /// Turn identifier linking user + assistant + tool nodes.
        turn_id: TurnId,
        /// The canonical IR message.
        msg: Msg,
    },
    /// Tier 3 incremental compaction summary covering ancestors through `covers_node_id`.
    Summary {
        /// Node ID covered up to (inclusive).
        covers_node_id: NodeId,
        /// Compact summary text (never overwrites raw DAG history).
        text: String,
        /// Compaction method that produced this node (e.g. `"summary"`, `"handoff"`, `"remote"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        /// Optional opaque provider replay state (e.g. remote compaction preserve payload).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preserve: Option<Value>,
    },
    /// Explicit branch checkpoint / fork marker.
    ForkMarker {
        /// Optional label for the branch.
        label: String,
    },
    /// Deferred tools made callable from here on (`tool_search`); the branch's union of these
    /// is the session's activated set, so `--resume` and `/fork` keep it.
    ToolsActivated {
        /// Qualified tool names, in activation order.
        names: Vec<String>,
    },
    /// The session's todo checklist, rewritten by one successful `todo` mutation (or a `/todo`
    /// edit). The newest such node on the active branch is the live checklist; a rewind past it
    /// takes the list back with the conversation.
    TodoBoard {
        /// The full phase list after the edit.
        phases: Vec<TodoPhase>,
    },
    /// Terminal exit record written at the end of a turn or fatal interruption for crash restore.
    SessionExit {
        /// Whether the turn ended normally, was interrupted by the user, or aborted fatally.
        reason: ExitReason,
        /// Tool calls that were in flight when the turn exited.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pending: Vec<PendingToolCall>,
    },
    /// User-assigned node label (`Shift+L` in tree view; last write wins per target node).
    Label {
        /// Node id being labeled.
        target_node_id: NodeId,
        /// Label text (`None` clears the label).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Summary of an abandoned branch attached at the new leaf during tree navigation.
    BranchSummary {
        /// Leaf node id of the branch that was left behind.
        from_node_id: NodeId,
        /// Summarized work from the abandoned branch.
        text: String,
    },
    /// Recorded model switch (`role = "temporary"` for `/switch`, `"fallback"` for promotion).
    ModelChange {
        /// Target model selector (`provider/id[:level]` or alias).
        selector: String,
        /// Switch role tag (`"temporary"`, `"fallback"`, etc.).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
    },
}

/// One immutable append-only node in a session's conversation DAG.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DagNode {
    /// Unique node identifier.
    pub id: NodeId,
    /// Owning session identifier.
    pub session_id: SessionId,
    /// Parent node identifier (`None` for the root node).
    pub parent_id: Option<NodeId>,
    /// Unix timestamp in milliseconds when the node was created.
    pub created_at_ms: i64,
    /// Node payload.
    pub payload: DagPayload,
}

/// Metadata describing a primary or child agent session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Session identifier.
    pub id: SessionId,
    /// Human-readable title.
    pub title: String,
    /// Working directory (repo root or git worktree path).
    pub cwd: String,
    /// Active git branch or worktree slug, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Active head node of the current branch in the DAG.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_node_id: Option<NodeId>,
    /// Active model alias/preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Active thinking effort.
    pub thinking: ThinkingLevel,
    /// Active goal-mode objective (`None`: goal mode off), omp's `GoalModeState.goal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<Goal>,
    /// Active permission mode.
    pub permission_mode: PermissionMode,
    /// Optional parent link when this session is a spawned subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    /// Optional parent node that spawned this child session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_node_id: Option<NodeId>,
    /// Live runtime status.
    pub status: AgentStatus,
    /// Whether the user pinned the session; pinned sessions lead `list_sessions`. Read-only
    /// outside the store (the `session_pins` table is the truth).
    #[serde(default)]
    pub pinned: bool,
    /// Provenance of `title` (`auto` never overwrites `user`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_source: Option<TitleSource>,
    /// Timestamp (Unix ms) of the last clean or fatal exit record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_ms: Option<i64>,
    /// Creation timestamp (Unix ms).
    pub created_at_ms: i64,
    /// Last update timestamp (Unix ms).
    pub updated_at_ms: i64,
}

/// Request sent to an [`crate::ApprovalGate`] before executing a gated tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Unique approval prompt identifier.
    pub id: String,
    /// Session requesting approval.
    pub session_id: SessionId,
    /// Tool name.
    pub tool_name: String,
    /// Human-readable summary of the action (command or path).
    pub summary: String,
    /// Optional unified diff preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Optional reason why confirmation is required (safety rule, restricted folder, user policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Normalized paths or URLs touched by this call (for cached file/dir grants and folders).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// Permitted approval scopes for this prompt (`[Once]` = plain Approve/Deny only, e.g.
    /// restricted folder or critical safety override; empty = front-end default for active mode).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<crate::tier::ApprovalGrantScope>,
    /// Risk tier of the tool call requesting approval (used by front-ends to avoid tier-blind grants).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<crate::tier::ToolTier>,
}

/// The form the `ask` tool shows the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskRequest {
    /// Unique prompt identifier.
    pub id: String,
    /// Owning session identifier.
    pub session_id: SessionId,
    /// The parsed form to answer.
    pub form: Form,
}

/// Status of one tracked agent todo item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started yet.
    #[default]
    Pending,
    /// Currently in progress.
    InProgress,
    /// Finished.
    Completed,
    /// Given up on.
    Abandoned,
}

impl TodoStatus {
    /// Parse a model-supplied status; `done` and `completed` are aliases.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "done" | "completed" => Some(Self::Completed),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }

    /// The status name as the model writes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Abandoned => "abandoned",
        }
    }

    /// The checklist box glyph for this status.
    #[must_use]
    pub const fn marker(self) -> char {
        match self {
            Self::Pending => '☐',
            Self::InProgress => '◐',
            Self::Completed => '☑',
            Self::Abandoned => '⊘',
        }
    }

    /// Whether the task needs no further work.
    #[must_use]
    pub const fn closed(self) -> bool {
        matches!(self, Self::Completed | Self::Abandoned)
    }
}

/// One tracked task item; its verbatim content is its identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// The task description, kept exactly as written.
    pub content: String,
    /// Current status.
    pub status: TodoStatus,
}

/// The phase a flat task list gets when none is named.
pub const DEFAULT_PHASE: &str = "Tasks";

/// A named group of tasks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoPhase {
    /// The phase name.
    pub name: String,
    /// Its tasks, in order.
    pub tasks: Vec<TodoItem>,
}

impl TodoPhase {
    /// Whether no task of this phase is open work.
    #[must_use]
    pub fn all_closed(&self) -> bool {
        self.tasks.iter().all(|t| t.status.closed())
    }
}

/// Commands accepted by a `SessionActor` (from TUI, CLI, IPC, or subagent supervisor).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum SessionCommand {
    /// Start a new turn with user input (or queue as follow-up if busy).
    Prompt {
        /// Prompt text.
        text: String,
    },
    /// Inject mid-turn steering text between `drive_loop` steps without losing current progress.
    Steer {
        /// Steering instruction.
        text: String,
    },
    /// Queue a follow-up turn to execute automatically when the current turn settles.
    FollowUp {
        /// Follow-up prompt text.
        text: String,
    },
    /// Cancel the active turn immediately.
    Interrupt,
    /// Fork the DAG from `from_node_id` (or current head if `None`).
    Fork {
        /// Optional ancestor node to branch from.
        from_node_id: Option<NodeId>,
        /// Optional label for the new branch.
        label: Option<String>,
    },
    /// Switch active model alias/preset.
    SetModel {
        /// Model alias or preset name.
        model: String,
    },
    /// Switch thinking / reasoning effort level.
    SetThinking {
        /// New thinking level.
        thinking: ThinkingLevel,
    },
    /// Switch permission mode.
    SetPermissionMode {
        /// New permission mode.
        mode: PermissionMode,
    },
    /// Trigger explicit Tier 3 compaction.
    Compact,
}

/// Streaming events emitted by `SessionActor` to all attached subscribers (TUI, IPC, Herdr).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Agent status changed (`idle`, `working`, `blocked`, `error`).
    StatusChanged {
        /// Owning session identifier.
        session_id: SessionId,
        /// New live status.
        status: AgentStatus,
        /// Optional detail (e.g. pending approval summary or error reason).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// New turn began.
    TurnStart {
        /// Turn identifier.
        turn_id: TurnId,
        /// User text that started the turn.
        user_text: String,
    },
    /// Mid-turn steer message was absorbed into the active loop.
    SteerInjected {
        /// Turn identifier.
        turn_id: TurnId,
        /// Injected steer text.
        text: String,
    },
    /// Streamed reasoning/thinking chunk.
    ThinkingDelta {
        /// Incremental reasoning text.
        delta: String,
    },
    /// Streamed visible assistant text chunk.
    TextDelta {
        /// Incremental visible text.
        delta: String,
    },
    /// Tool execution started.
    ToolStart {
        /// Tool call id.
        call_id: String,
        /// Tool name.
        name: String,
        /// Raw JSON arguments.
        arguments: String,
    },
    /// Incremental live output chunk from a running tool (e.g. `bash`).
    ToolDelta {
        /// Tool call id.
        call_id: String,
        /// Output chunk.
        delta: String,
    },
    /// Tool execution completed.
    ToolDone {
        /// Tool call id.
        call_id: String,
        /// Tool name.
        name: String,
        /// Tool result text.
        output: String,
        /// Whether execution failed.
        is_error: bool,
        /// Optional unified diff preview.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
        /// Optional inline image attachments returned by the tool.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<crate::ir::ImageAttachment>,
    },
    /// Approval requested from user.
    ApprovalRequested {
        /// Approval prompt payload.
        request: ApprovalRequest,
    },
    /// Approval resolved.
    ApprovalResolved {
        /// Approval prompt id.
        id: String,
        /// Whether user approved.
        approved: bool,
    },
    /// Structured question requested via `ask` tool.
    AskRequested {
        /// Question prompt payload.
        request: AskRequest,
    },
    /// Todo list updated.
    TodosUpdated {
        /// The full phase list, in order.
        phases: Vec<TodoPhase>,
    },
    /// One LLM step completed with telemetry.
    LlmCall {
        /// Model used.
        model: String,
        /// Time to first token in ms.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_token_ms: Option<u64>,
        /// Total LLM call duration in ms.
        duration_ms: u64,
        /// Normalized stop reason.
        finish_reason: crate::ir::FinishReason,
        /// Token usage.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// Compaction summary node created.
    Compacted {
        /// Covered node id.
        covers_node_id: NodeId,
        /// Compaction method that ran (`summary`, `handoff`, `shake`, `remote`, `snapcompact`).
        #[serde(default)]
        method: String,
        /// Estimated tokens before compaction.
        #[serde(default)]
        tokens_before: u64,
        /// Estimated tokens after compaction.
        #[serde(default)]
        tokens_after: u64,
        /// Approximate character reduction.
        saved_chars: usize,
    },
    /// Background speculative compaction started (`active = true`) or finished (`active = false`).
    CompactionSpeculating {
        /// Whether a speculative background compaction pass is currently running.
        active: bool,
    },
    /// User-visible assistant prose note emitted mid-turn via `out://`.
    AssistantNote {
        /// Markdown text rendered to the user without ending the turn.
        text: String,
    },
    /// Passive advisor reviewed the completed turn and emitted an actionable note.
    AdvisorNote {
        /// Model that generated the review.
        model: String,
        /// Concise actionable advice injected into the next turn.
        note: String,
    },
    /// Goal mode changed: a new/updated goal, or `None` when it ended (omp's `goal_updated`).
    GoalUpdated {
        /// The live goal after the change.
        goal: Option<crate::goal::Goal>,
    },
    /// Session title was generated or refreshed asynchronously.
    SessionTitle {
        /// Session whose title changed.
        session_id: SessionId,
        /// Updated display title.
        title: String,
    },
    /// Turn completed cleanly.
    TurnDone {
        /// Turn identifier.
        turn_id: TurnId,
        /// Final model used.
        model: String,
        /// Final assistant text.
        text: String,
        /// Accumulated turn usage.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Number of LLM steps executed.
        steps: usize,
    },
    /// Turn failed with an error.
    TurnError {
        /// Turn identifier.
        turn_id: TurnId,
        /// Stable error code.
        code: String,
        /// Human-readable error message.
        message: String,
    },
}

#[cfg(test)]
#[path = "dag_tests.rs"]
mod tests;
