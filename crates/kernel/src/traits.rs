//! Core port traits implemented by edges, tools, hooks, and persistence backends.

use crate::caller::ToolCaller;
use crate::dag::{
    AgentEvent, ApprovalRequest, AskRequest, DagNode, PermissionMode, SessionId, SessionMeta,
    TurnId,
};
use crate::error::{CancelFlag, Result};
use crate::form::Submission;
use crate::inbox::Inbox;
use crate::ir::{
    LlmChunk, LlmRequest, LlmResponse, ModelInfo, Msg, ProviderCapabilities, RemoteCompaction,
    ToolEffect, ToolOutput, ToolSpec, Usage, UsageRow,
};
use crate::tier::{ApprovalDecision, ApprovalGrant, TitleSource, ToolApproval, TraceRecord};
use crate::urls::UrlRouter;
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Owned boxed `Send` future returned by trait methods.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
/// Borrowed boxed `Send` future for zero-copy streaming calls.
pub type ScopedFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Streaming token sink handed to [`LlmProvider::stream_chat`].
pub trait ChunkSink: Send + Sync {
    /// Deliver one streamed text or reasoning chunk.
    fn on_chunk(&self, chunk: LlmChunk) -> BoxFuture<()>;
}

/// Outbound LLM backend (or router across multiple backends).
pub trait LlmProvider: Send + Sync {
    /// Execute one streaming chat completion call.
    fn stream_chat<'a>(
        &'a self,
        request: &'a LlmRequest,
        cancel: &'a CancelFlag,
        sink: &'a dyn ChunkSink,
    ) -> ScopedFuture<'a, Result<LlmResponse>>;

    /// List configured model aliases/presets.
    fn models(&self) -> Vec<ModelInfo>;

    /// Query capabilities (context window, thinking, prompt cache) for a model alias.
    fn capabilities(&self, _model: Option<&str>) -> ProviderCapabilities {
        ProviderCapabilities {
            tools: true,
            thinking: true,
            prompt_cache: true,
            context_window: 200_000,
            max_context_window: None,
        }
    }

    /// The base-prompt variant for a model alias (`None` = the default variant). The router
    /// resolves it from the preset's declared variant or the model-family table.
    fn prompt_variant(&self, _model: Option<&str>) -> Option<String> {
        None
    }

    /// Whether `err` represents a context-window overflow from the upstream provider.
    fn is_context_overflow(&self, _err: &crate::error::HrnError) -> bool {
        false
    }

    /// Optional larger-context model alias to promote to before compacting on overflow.
    fn context_promotion_target(&self, _model: Option<&str>) -> Option<String> {
        None
    }

    /// Whether the serving model supports explicit service-tier selection (`/fast`).
    fn supports_service_tier(&self, _model: Option<&str>) -> bool {
        false
    }

    /// Whether the serving model accepts inline image inputs (used by `snapcompact`).
    fn supports_vision(&self, _model: Option<&str>) -> bool {
        false
    }

    /// Run provider-native server-side compaction (e.g. OpenAI Responses `/responses/compact`).
    /// Returns `Ok(None)` when the model/provider does not support native compaction.
    fn compact_remote<'a>(
        &'a self,
        _model: Option<&'a str>,
        _messages: &'a [Msg],
        _instructions: Option<&'a str>,
        _cancel: &'a CancelFlag,
    ) -> ScopedFuture<'a, Result<Option<RemoteCompaction>>> {
        Box::pin(async { Ok(None) })
    }
}

/// Execution context passed to every [`Tool::execute`] invocation.
#[derive(Clone)]
pub struct ToolContext {
    /// Owning session identifier.
    pub session_id: SessionId,
    /// Active turn identifier.
    pub turn_id: TurnId,
    /// Tool call identifier (used for streaming `ToolDelta` events).
    pub call_id: String,
    /// Working directory for this agent (repo root or git worktree).
    pub cwd: PathBuf,
    /// Active permission mode for this session.
    pub permission_mode: PermissionMode,
    /// Cooperative cancellation flag.
    pub cancel: CancelFlag,
    /// Event sink for live tool output chunks (`ToolDelta`, `TodosUpdated`, etc.).
    pub events: Arc<dyn EventSink>,
    /// Optional interactive question handler (used by the `ask` tool).
    pub ask: Option<Arc<dyn AskHandler>>,
    /// Approval gate inherited by nested agents (subagents) so they cannot bypass confirmation.
    pub approval: Option<Arc<dyn ApprovalGate>>,
    /// Internal-URL schemes (`local://`, `skill://`, …) shared by every tool of the runtime.
    pub urls: Arc<UrlRouter>,
    /// The session at the root of this agent's tree (`session_id` itself for a top-level
    /// session); `local://` scratch files are shared across the whole tree through it.
    pub root_session_id: SessionId,
    /// Where background work started by this call delivers its result, keyed by `session_id`.
    pub inbox: Arc<Inbox>,
    /// The session's way of running another tool, for tools that call tools (`bash` tool
    /// commands); `None` where the session offers none.
    pub caller: Option<std::sync::Arc<dyn ToolCaller>>,
    /// The runtime's live todo checklists. The `todo` tool applies its op to the checklist of
    /// `root_session_id` and writes the result back; the runtime persists the same list as a
    /// board node through the call's [`crate::SessionEffect::TodoEdit`].
    pub todos: Arc<crate::todo::TodoBoards>,
    /// The structured result a subagent's hidden `yield` tool submitted for this run; the task
    /// executor reads it as the agent's final output. Fresh `Arc` per agent run.
    pub yield_data: Arc<Mutex<Option<String>>>,
    /// Whether this call is running inside a spawned subagent rather than the top-level session.
    pub is_subagent: bool,
}

/// One callable tool registered with the agent harness.
pub trait Tool: Send + Sync {
    /// JSON schema metadata exposed to the LLM.
    fn spec(&self) -> ToolSpec;

    /// Side-effect classification (`ReadOnly` tools run in parallel batches).
    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    /// Approval tier and optional per-call override for `args` (defaults to [`ToolApproval::exec`]).
    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::exec()
    }

    /// Hidden tool (omp's `HIDDEN_TOOLS`: `goal`, `think`, `yield`): callable, but its schema
    /// only joins the request when the runtime activates it for the session's state.
    fn hidden(&self) -> bool {
        false
    }

    /// Human-readable one-line summary for approval prompts (e.g. command or file path).
    fn summarize_call(&self, args: &Value) -> String {
        let s = args.to_string();
        if s.len() > 140 {
            let cut = s.floor_char_boundary(140);
            format!("{}...", s.get(..cut).unwrap_or(""))
        } else {
            s
        }
    }

    /// Execute the tool with parsed JSON arguments.
    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>>;
}

/// A tool whose schema stays out of LLM requests until `tool_search` activates it. It is listed
/// compactly (by `short_name`, under `group`) in the system prompt instead.
#[derive(Clone)]
pub struct DeferredTool {
    /// Listing group, e.g. the MCP server name.
    pub group: String,
    /// Name shown in the compact listing (the qualified name minus the group's shared prefix).
    pub short_name: String,
    /// The tool itself; callable under its normal `spec().name` once activated.
    pub tool: Arc<dyn Tool>,
}

/// Interactive confirmation gate for mutating / dangerous tool calls.
pub trait ApprovalGate: Send + Sync {
    /// Suspend until the user approves or denies `request` (or `cancel` trips).
    fn await_decision(
        &self,
        request: ApprovalRequest,
        cancel: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>>;
}

/// Interactive form handler backing the `ask` tool.
pub trait AskHandler: Send + Sync {
    /// Show the form and return what the user submitted; a cancelled form is an error.
    fn ask(&self, request: AskRequest, cancel: CancelFlag) -> BoxFuture<Result<Submission>>;
}

/// Kind of a filesystem entry as reported by [`WorkspaceFs::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    /// Nothing exists at the path.
    Missing,
    /// Regular file.
    File,
    /// Directory.
    Dir,
}

/// Read-only filesystem port used by the harness to discover `AGENTS.md`, custom commands and
/// subagent profiles without depending on `std::fs` (keeps `hrn-harness` free of I/O).
pub trait WorkspaceFs: Send + Sync {
    /// Read a UTF-8 file; `Ok(None)` when it does not exist, `Err` for any other failure.
    fn read_to_string(&self, path: &Path) -> Result<Option<String>>;

    /// List directory entries (full paths, sorted); `Ok(vec![])` when it does not exist.
    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;

    /// Classify `path`; unreadable entries count as [`PathKind::Missing`].
    fn kind(&self, path: &Path) -> PathKind;

    /// The user's home directory, if known.
    fn home_dir(&self) -> Option<PathBuf>;

    /// Canonicalize `path` when it exists on disk (resolving symlinks/junctions); `None` lets the
    /// caller fall back to lexical normalization.
    fn canonicalize(&self, _path: &Path) -> Option<PathBuf> {
        None
    }
}

/// Subscriber sink receiving live [`AgentEvent`] frames.
pub trait EventSink: Send + Sync {
    /// Emit one event (`false` signals subscriber disconnected).
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool>;
}

/// Verdict returned by [`HarnessHook::before_tool`].
#[derive(Clone, Debug)]
pub enum HookVerdict {
    /// Proceed with execution unchanged.
    Continue,
    /// Proceed, optionally with replacement arguments and/or a context block the next model call
    /// carries (omp's `input` + `additionalContext`).
    Revise {
        /// Replacement tool arguments; `None` keeps the call's own.
        arguments: Option<Value>,
        /// Trusted instruction block injected before the next LLM request.
        context: Option<String>,
    },
    /// Short-circuit tool execution and return this output directly to the model.
    ShortCircuit(ToolOutput),
}

/// One session/turn lifecycle point a [`HarnessHook`] observes.
#[derive(Clone, Debug)]
pub enum HookEvent {
    /// A session began its first turn.
    SessionStart {
        /// Session id.
        session_id: String,
        /// Working directory of the session.
        cwd: PathBuf,
    },
    /// The user submitted the prompt that starts a turn.
    UserPrompt {
        /// Session id.
        session_id: String,
        /// Working directory of the session.
        cwd: PathBuf,
        /// The prompt text.
        prompt: String,
    },
    /// A turn finished: `final_text` on success, `error` when it failed.
    TurnEnd {
        /// Session id.
        session_id: String,
        /// Working directory of the session.
        cwd: PathBuf,
        /// The assistant's final text, when the turn succeeded.
        final_text: Option<String>,
        /// Why the turn failed, if it did.
        error: Option<String>,
    },
}

/// Deterministic lifecycle hook installed in the harness pipeline.
pub trait HarnessHook: Send + Sync {
    /// Inspect or transform messages immediately before an LLM call.
    fn before_llm(&self, _messages: &mut Vec<Msg>) -> BoxFuture<Result> {
        Box::pin(async { Ok(()) })
    }

    /// Inspect a tool call before execution; may modify arguments or short-circuit.
    fn before_tool(
        &self,
        _ctx: &ToolContext,
        _tool_name: &str,
        _arguments: &Value,
    ) -> BoxFuture<Result<HookVerdict>> {
        Box::pin(async { Ok(HookVerdict::Continue) })
    }

    /// A session/turn lifecycle point. A returned string is context the runtime injects into the
    /// conversation; an `Err` reports the hook to the user and (for the events that precede the
    /// turn's work) fails the turn.
    fn on_event(&self, _event: &HookEvent) -> BoxFuture<Result<Option<String>>> {
        Box::pin(async { Ok(None) })
    }

    /// A tool that may change the workspace is about to run: its approval, if it needs one, is
    /// given and `before_tool` let it through. Nothing is run for a read-only tool.
    fn before_mutating_tool(&self, _ctx: &ToolContext, _tool_name: &str) -> BoxFuture<Result> {
        Box::pin(async { Ok(()) })
    }

    /// Transform tool output after execution (e.g. secret redaction, output capping, rules that
    /// the touched files match). `arguments` are the effective ones the tool ran with; a hook
    /// that wants the model to see more appends it to the output.
    fn after_tool(
        &self,
        _ctx: &ToolContext,
        _tool_name: &str,
        _arguments: &Value,
        output: ToolOutput,
    ) -> BoxFuture<Result<ToolOutput>> {
        Box::pin(async move { Ok(output) })
    }
}

/// Append-only DAG persistence store for sessions and nodes.
pub trait SessionStore: Send + Sync {
    /// Create and persist a new session record.
    fn create_session(&self, meta: &SessionMeta) -> Result;

    /// Update mutable session metadata (title, head, model, thinking, permission mode, status).
    fn update_session(&self, meta: &SessionMeta) -> Result;

    /// Load session metadata by id.
    fn get_session(&self, session_id: &str) -> Result<Option<SessionMeta>>;

    /// List top-level sessions (subagent child sessions are excluded): pinned ones first, then
    /// most recently updated first.
    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMeta>>;

    /// Delete a session with its nodes, usage and subagent sessions; `false` when it is unknown.
    fn delete_session(&self, session_id: &str) -> Result<bool>;

    /// Change a session's title and record provenance (`auto` does not overwrite `user`).
    fn rename_session(&self, session_id: &str, title: &str, source: TitleSource) -> Result;

    /// List up to `limit` most recently updated top-level sessions for `cwd`.
    fn list_recent_for(&self, cwd: &str, limit: usize) -> Result<Vec<SessionMeta>>;

    /// List child (subagent) sessions spawned directly under `parent_session_id`.
    fn list_children(&self, parent_session_id: &str) -> Result<Vec<SessionMeta>>;

    /// Insert or update a `/btw` side-question topic for `session_id`.
    fn put_btw_topic(
        &self,
        session_id: &str,
        topic_id: &str,
        json: &str,
        updated_at_ms: i64,
    ) -> Result;

    /// Load all `/btw` topics for `session_id`, newest first: `(topic_id, json, updated_at_ms)`.
    fn get_btw_topics(&self, session_id: &str) -> Result<Vec<(String, String, i64)>>;

    /// Persist a cross-session approval grant (`always-ask-cached`).
    fn put_approval_grant(&self, grant: &ApprovalGrant) -> Result;

    /// Load all persisted approval grants.
    fn list_approval_grants(&self) -> Result<Vec<ApprovalGrant>>;

    /// Delete every persisted approval grant (`/forget-approvals`).
    fn clear_approval_grants(&self) -> Result;

    /// Append a `/trace` or edit-attribution span record.
    fn record_trace(&self, trace: &TraceRecord) -> Result;

    /// Load recorded traces for `session_id`, oldest first.
    fn list_traces(&self, session_id: &str) -> Result<Vec<TraceRecord>>;

    /// Checkpoint WAL and vacuum storage when reclaiming space (`hrn gc --apply`).
    fn vacuum_and_checkpoint(&self) -> Result;

    /// Pin or unpin a session; pinned sessions lead [`SessionStore::list_sessions`].
    fn set_pinned(&self, session_id: &str, pinned: bool) -> Result;

    /// The ids of the pinned sessions.
    fn pinned_sessions(&self) -> Result<Vec<SessionId>>;

    /// Append an immutable DAG node and advance `head_node_id` on the session.
    fn append_node(&self, node: &DagNode) -> Result;

    /// Load the ancestor chain from root to `head_node_id` (inclusive, chronological order).
    fn load_branch(&self, session_id: &str, head_node_id: &str) -> Result<Vec<DagNode>>;

    /// Load all DAG nodes belonging to a session (for tree view / `/fork`).
    fn load_tree(&self, session_id: &str) -> Result<Vec<DagNode>>;

    /// Record the usage of one LLM call of a session.
    fn record_usage(&self, session_id: &str, at_ms: i64, model: &str, usage: &Usage) -> Result;

    /// Everything the session itself has used (its subagents' sessions not included).
    fn session_usage(&self, session_id: &str) -> Result<Usage>;

    /// Usage grouped by UTC day and model, newest day first.
    fn usage_report(&self) -> Result<Vec<UsageRow>>;

    /// [`SessionStore::usage_report`] of one session's own calls.
    fn session_usage_report(&self, session_id: &str) -> Result<Vec<UsageRow>>;
}

/// Wall-clock abstraction for deterministic testing.
pub trait Clock: Send + Sync {
    /// Current Unix timestamp in milliseconds.
    fn now_ms(&self) -> i64;
}

/// Standard system wall-clock implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        let dur = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        i64::try_from(dur.as_millis()).unwrap_or(i64::MAX)
    }
}
