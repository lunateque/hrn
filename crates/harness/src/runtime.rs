//! Core event-sourced DAG agent runtime: session lifecycle, `/fork`, Tier 3 compaction and the
//! `run_turn` entry point. The step loop itself lives in the private `turn` module.

use crate::advisor::AdvisorConfig;
use crate::agents::AgentRegistry;
use crate::approvals::{GrantCache, UserApprovalPolicy};
use crate::compaction::CompactionPolicy;
use crate::context::ContextSources;
use crate::control::SessionControl;
use crate::isolation::{DynamicToolResolver, WorktreeIsolator};
use crate::registry::ToolRegistry;
use crate::task_config::TaskConfig;
use hrn_kernel::{
    AgentEvent, AgentStatus, ApprovalGate, AskHandler, BoxFuture, CancelFlag, ChunkSink, Clock,
    DagNode, EventSink, HarnessHook, HrnError, ImageAttachment, LlmChunk, LlmProvider, LlmResponse,
    NodeId, ProcTable, Result, SessionMeta, SessionStore, SystemClock, UrlRouter, Usage,
    WorkspaceFs,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(1);

/// Configuration for [`AgentRuntime`].
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent runtime feature toggles (subagent, promotion, auto-title, replan)"
)]
pub struct HarnessConfig {
    /// `--system-prompt`: replaces the embedded base variant wholesale.
    pub system_prompt_override: Option<String>,
    /// `--append-system-prompt`: text appended after every `APPEND_SYSTEM.md` layer.
    pub append_system_override: Option<String>,
    /// An explicitly pinned base-prompt variant (`"claude"`, `"gpt5"`); by default the model
    /// router resolves one per family.
    pub prompt_variant: Option<String>,
    /// The paragraphs the modules contributed (`Registry::prompt_section`), appended to the
    /// static block in registration order.
    pub module_sections: Vec<String>,
    /// Whether this runtime serves a child (`task` subagent) session; its prompt gains the
    /// hand-off block.
    pub subagent: bool,
    /// Optional explicit service tier applied to subagent turns (`[llm.tier] subagent`);
    /// `None` inherits the parent session's active tier.
    pub subagent_tier: Option<hrn_kernel::Tier>,
    /// Maximum LLM + tool iterations per user turn (`0` = unlimited, the default for the main
    /// agent; subagents always get an explicit budget from `[task.max_steps]` /
    /// `[task.agent_max_steps]`).
    pub max_steps: usize,
    /// Role alias used for Tier 3 summarization (default `"fast"`); falls back to the session
    /// model when the router does not know the alias.
    pub fast_model_role: String,
    /// Context compaction policy (`context_window_tokens` is overridden by the provider's
    /// reported capabilities for the session model).
    pub compaction: CompactionPolicy,
    /// Promote to a model's larger `context_promotion_target` before compacting (`[llm] context_promotion`).
    pub context_promotion: bool,
    /// Passive post-turn advisor configuration.
    pub advisor: AdvisorConfig,
    /// Which instruction sources the session reads (`--skills` / `--no-skills` / `--no-rules` /
    /// `--no-context-files` land here; subagents inherit the parent's).
    pub context: ContextSources,
    /// Subagent task limits, shape, isolation, and idle-parking TTL (`[task]`).
    pub task: TaskConfig,
    /// User-configured approval overrides (`[tools] restricted_folders` and `[tools.approval]`).
    pub approval_policy: UserApprovalPolicy,
    /// Recover accidental inline edit blocks emitted in assistant prose (`[edit] recover_inline_edits`).
    pub recover_inline_edits: bool,
    /// Automatically title a session after its first substantive turn (`[session] auto_title`).
    pub auto_title: bool,
    /// Refresh an auto-generated title when a todo replan occurs (`[session] title_refresh_on_replan`).
    pub title_refresh_on_replan: bool,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            system_prompt_override: None,
            append_system_override: None,
            prompt_variant: None,
            module_sections: Vec::new(),
            subagent: false,
            subagent_tier: None,
            max_steps: 0,
            fast_model_role: "fast".to_owned(),
            compaction: CompactionPolicy::default(),
            context_promotion: false,
            advisor: AdvisorConfig::default(),
            context: ContextSources::default(),
            task: TaskConfig::default(),
            approval_policy: UserApprovalPolicy::default(),
            recover_inline_edits: true,
            auto_title: false,
            title_refresh_on_replan: false,
        }
    }
}

/// Front-end channels and cancellation for one [`AgentRuntime::run_turn`] call.
#[derive(Clone)]
pub struct TurnIo {
    /// Cooperative cancellation for the whole turn (children derive from it).
    pub cancel: CancelFlag,
    /// Live event subscriber.
    pub events: Arc<dyn EventSink>,
    /// Approval channel; without one, confirmation-requiring tools are refused.
    pub approval: Option<Arc<dyn ApprovalGate>>,
    /// Interactive question handler for the `ask` tool.
    pub ask: Option<Arc<dyn AskHandler>>,
}

/// Summary returned at the end of one turn.
#[derive(Clone, Debug)]
pub struct TurnResult {
    /// Final assistant text for the turn.
    pub final_text: String,
    /// Number of LLM steps executed.
    pub steps: usize,
    /// Updated DAG head node id.
    pub head_node_id: Option<NodeId>,
    /// Token usage accumulated over all LLM calls of the turn.
    pub usage: Option<Usage>,
    /// Model that served the last call.
    pub model: String,
}

/// Core runtime coordinating LLM streaming, DAG persistence, compaction, and tool execution.
#[derive(Clone)]
pub struct AgentRuntime {
    pub(crate) llm: Arc<dyn LlmProvider>,
    pub(crate) store: Arc<dyn SessionStore>,
    pub(crate) tools: ToolRegistry,
    pub(crate) fs: Arc<dyn WorkspaceFs>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) config: HarnessConfig,
    pub(crate) extra_hooks: Vec<Arc<dyn HarnessHook>>,
    /// (session id, rule name) pairs whose glob rule already reached this session.
    pub(crate) rules_seen: Arc<std::sync::Mutex<std::collections::HashSet<(String, String)>>>,
    pub(crate) urls: Arc<UrlRouter>,
    pub(crate) agents: Arc<AgentRegistry>,
    pub(crate) procs: Arc<ProcTable>,
    pub(crate) todos: Arc<hrn_kernel::todo::TodoBoards>,
    pub(crate) grants: Arc<GrantCache>,
    pub(crate) isolator: Option<Arc<dyn WorktreeIsolator>>,
    pub(crate) dynamic_tools: Option<Arc<dyn DynamicToolResolver>>,
    pub(crate) snap_renderer: Option<Arc<dyn hrn_kernel::SnapRenderer>>,
    pub(crate) speculative: crate::compaction::SpeculativeMap,
    pub(crate) title_revs: crate::runtime::TitleRevs,
}

pub(crate) struct NullSink;

impl ChunkSink for NullSink {
    fn on_chunk(&self, _chunk: LlmChunk) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

impl AgentRuntime {
    /// Construct a runtime using the system clock.
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        store: Arc<dyn SessionStore>,
        tools: ToolRegistry,
        fs: Arc<dyn WorkspaceFs>,
        config: HarnessConfig,
    ) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let grants = Arc::new(GrantCache::with_store(
            Arc::clone(&store),
            Arc::clone(&clock),
        ));
        Self {
            llm,
            store,
            tools,
            fs,
            clock,
            config,
            extra_hooks: Vec::new(),
            rules_seen: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            urls: Arc::default(),
            agents: Arc::new(AgentRegistry::detached()),
            procs: Arc::default(),
            todos: Arc::default(),
            grants,
            isolator: None,
            dynamic_tools: None,
            snap_renderer: None,
            speculative: Arc::default(),
            title_revs: Arc::default(),
        }
    }

    /// Clone this runtime with a [`WorktreeIsolator`] backing `task` items that set `isolated: true`.
    #[must_use]
    pub fn with_isolator(&self, isolator: Arc<dyn WorktreeIsolator>) -> Self {
        let rt = Self {
            isolator: Some(isolator),
            ..self.clone()
        };
        rt.agents.attach_host(&rt);
        rt
    }

    /// Configured [`WorktreeIsolator`], if attached.
    #[must_use]
    pub fn isolator(&self) -> Option<&Arc<dyn WorktreeIsolator>> {
        self.isolator.as_ref()
    }

    /// Clone this runtime with a [`DynamicToolResolver`] for eval-defined subagent tools.
    #[must_use]
    pub fn with_dynamic_tools(&self, resolver: Arc<dyn DynamicToolResolver>) -> Self {
        let rt = Self {
            dynamic_tools: Some(resolver),
            ..self.clone()
        };
        rt.agents.attach_host(&rt);
        rt
    }

    /// Configured [`DynamicToolResolver`], if attached.
    #[must_use]
    pub fn dynamic_tools(&self) -> Option<&Arc<dyn DynamicToolResolver>> {
        self.dynamic_tools.as_ref()
    }

    /// Clone this runtime sharing an explicit approval [`GrantCache`] (preserves session grants
    /// across hot-reloads and multiple protocol fronts).
    #[must_use]
    pub fn with_grants(&self, grants: Arc<GrantCache>) -> Self {
        Self {
            grants,
            ..self.clone()
        }
    }

    /// Shared session + persistent approval grant cache.
    #[must_use]
    pub fn grants(&self) -> &Arc<GrantCache> {
        &self.grants
    }

    /// Workspace filesystem port backing scope and approval canonicalization.
    pub(crate) fn fs(&self) -> &Arc<dyn WorkspaceFs> {
        &self.fs
    }

    /// Underlying LLM provider backing this runtime (reused for one-shot commit generation).
    #[must_use]
    pub fn llm(&self) -> &Arc<dyn LlmProvider> {
        &self.llm
    }

    /// Clear in-memory session-scoped approval grants belonging to `session_id`.
    pub fn clear_session_approvals(&self, session_id: &str) {
        self.grants.clear_session(session_id);
    }

    /// Clear all session-scoped and persisted cross-session approval grants (`/forget-approvals`).
    ///
    /// # Errors
    /// Returns a store error if clearing persistent `approval_grants` fails.
    pub fn forget_approvals(&self) -> Result {
        self.grants.clear()
    }

    /// Clone this runtime with the process-wide live todo checklist table (shared with the
    /// `todo` tool module through [`hrn_kernel::Shared`]).
    #[must_use]
    pub fn with_todos(&self, todos: Arc<hrn_kernel::todo::TodoBoards>) -> Self {
        Self {
            todos,
            ..self.clone()
        }
    }

    /// The live todo checklists, keyed by root session id.
    #[must_use]
    pub fn todos(&self) -> &Arc<hrn_kernel::todo::TodoBoards> {
        &self.todos
    }

    /// Replace the clock (tests, deterministic replays).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.grants = Arc::new(GrantCache::with_store(
            Arc::clone(&self.store),
            Arc::clone(&clock),
        ));
        self.clock = clock;
        self
    }

    /// Clone this runtime with the internal-URL schemes every tool and the output-spill hook use.
    #[must_use]
    pub fn with_urls(&self, urls: Arc<UrlRouter>) -> Self {
        Self {
            urls,
            ..self.clone()
        }
    }

    /// Clone this runtime with the registry that tracks (and runs) its background agents. The
    /// process table starts as just those agents; [`AgentRuntime::with_procs`] widens it.
    #[must_use]
    pub fn with_agents(&self, agents: Arc<AgentRegistry>) -> Self {
        let rt = Self {
            procs: Arc::new(ProcTable::new(vec![Arc::clone(&agents) as _])),
            agents,
            ..self.clone()
        };
        rt.agents.attach_host(&rt);
        rt
    }

    /// The registry of background agents (and, through it, the session inbox).
    pub fn agents(&self) -> &Arc<AgentRegistry> {
        &self.agents
    }

    /// The `[task]` configuration applied to subagents spawned by this runtime.
    pub fn task_config(&self) -> &TaskConfig {
        &self.config.task
    }

    /// Clone this runtime with a different `[task]` configuration.
    #[must_use]
    pub fn with_task_config(&self, task: TaskConfig) -> Self {
        let mut config = self.config.clone();
        config.task = task;
        Self {
            config,
            ..self.clone()
        }
    }

    /// Clone this runtime with the table of everything that runs in the background (agents and
    /// whatever else the binary registers, such as bash jobs).
    #[must_use]
    pub fn with_procs(&self, procs: Arc<ProcTable>) -> Self {
        Self {
            procs,
            ..self.clone()
        }
    }

    /// Everything running in the background, as `proc://`, `wait` and the front-ends see it.
    pub fn procs(&self) -> &Arc<ProcTable> {
        &self.procs
    }

    /// Clone this runtime with an additional lifecycle hook prepended ahead of builtin hooks.
    #[must_use]
    pub fn with_hook(&self, hook: Arc<dyn HarnessHook>) -> Self {
        let mut extra_hooks = self.extra_hooks.clone();
        extra_hooks.push(hook);
        Self {
            extra_hooks,
            ..self.clone()
        }
    }

    /// Clone this runtime with a different tool registry (used for subagents).
    #[must_use]
    pub fn with_tools(&self, tools: ToolRegistry) -> Self {
        Self {
            tools,
            ..self.clone()
        }
    }

    /// Clone this runtime with an overridden per-turn step cap (used for subagent profiles).
    #[must_use]
    pub fn with_max_steps(&self, max_steps: usize) -> Self {
        let mut config = self.config.clone();
        config.max_steps = max_steps;
        Self {
            config,
            ..self.clone()
        }
    }

    /// Clone this runtime marked as serving child (`task` subagent) sessions: their system
    /// prompt carries the hand-off block.
    #[must_use]
    pub fn as_subagent(&self) -> Self {
        let mut config = self.config.clone();
        config.subagent = true;
        config.auto_title = false;
        config.title_refresh_on_replan = false;
        Self {
            config,
            ..self.clone()
        }
    }

    /// The registered tools.
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    pub(crate) fn next_id(&self, prefix: &str) -> String {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        format!(
            "{prefix}_{:x}_{:x}_{n}",
            self.clock.now_ms(),
            std::process::id()
        )
    }

    pub(crate) fn load_head_branch(&self, meta: &SessionMeta) -> Result<Vec<DagNode>> {
        match &meta.head_node_id {
            Some(head) => self.store.load_branch(&meta.id, head),
            None => Ok(Vec::new()),
        }
    }

    /// Persist and announce a status change.
    pub(crate) async fn set_status(
        &self,
        meta: &mut SessionMeta,
        status: AgentStatus,
        message: Option<String>,
        events: &Arc<dyn EventSink>,
    ) -> Result {
        meta.status = status;
        meta.updated_at_ms = self.clock.now_ms();
        self.store.update_session(meta)?;
        emit(
            events,
            AgentEvent::StatusChanged {
                session_id: meta.id.clone(),
                status,
                message,
            },
        )
        .await;
        Ok(())
    }

    /// Active `[compaction]` configuration for this runtime.
    #[must_use]
    pub fn compaction_config(&self) -> &crate::compaction::CompactionConfig {
        &self.config.compaction.config
    }

    /// Provider capabilities for `model` (respects session extended-context settings).
    #[must_use]
    pub fn capabilities(&self, model: Option<&str>) -> hrn_kernel::ProviderCapabilities {
        self.llm.capabilities(model)
    }

    /// Whether `model` supports service tiers (`/fast`).
    #[must_use]
    pub fn supports_service_tier(&self, model: Option<&str>) -> bool {
        self.llm.supports_service_tier(model)
    }

    /// Compaction policy with the context window and canonical model key from the provider.
    pub fn policy_for(&self, model: Option<&str>) -> CompactionPolicy {
        let mut policy = self.config.compaction.clone();
        let window = self.llm.capabilities(model).context_window;
        if window > 0 {
            policy.context_window_tokens = window;
        }
        policy.model_key = resolve_model_key(&self.llm.models(), model);
        policy
    }

    /// Persist what one LLM call of `session_id` used, for `hrn stats` and the cost totals.
    pub(crate) fn record_usage(
        &self,
        session_id: &str,
        asked: Option<&str>,
        response: &LlmResponse,
    ) -> Result {
        match &response.usage {
            Some(usage) => self.store.record_usage(
                session_id,
                self.clock.now_ms(),
                &model_name(&self.llm.models(), asked, &response.model_used),
                usage,
            ),
            None => Ok(()),
        }
    }

    /// Run one user turn to completion (LLM/tool loop with steering, compaction and cancellation).
    ///
    /// # Errors
    /// Returns the failure that ended the turn (also reported as `TurnError`). Cancellation yields
    /// [`HrnError::Cancelled`]; the DAG is always left with every tool call answered.
    pub async fn run_turn(
        &self,
        session_id: &str,
        prompt: &str,
        control: &SessionControl,
        io: TurnIo,
    ) -> Result<TurnResult> {
        self.run_turn_with_images(session_id, prompt, Vec::new(), control, io)
            .await
    }

    /// Run one user turn with inline image attachments.
    ///
    /// # Errors
    /// Same as [`Self::run_turn`].
    pub async fn run_turn_with_images(
        &self,
        session_id: &str,
        prompt: &str,
        images: Vec<ImageAttachment>,
        control: &SessionControl,
        io: TurnIo,
    ) -> Result<TurnResult> {
        let guard = control.begin_turn(io.cancel.clone())?;
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let turn_id = self.next_id("turn");
        emit(
            &io.events,
            AgentEvent::TurnStart {
                turn_id: turn_id.clone(),
                user_text: prompt.to_owned(),
            },
        )
        .await;
        self.set_status(&mut meta, AgentStatus::Working, None, &io.events)
            .await?;

        let outcome = self
            .drive_turn(&mut meta, &turn_id, prompt, images, control, &io)
            .await;
        drop(guard);

        self.finish_turn(&mut meta, turn_id, outcome, &io).await
    }
}

/// Deliver an event. `emit` reports `false` once the subscriber has disconnected; a turn must
/// finish and persist regardless, so that signal is deliberately not acted upon.
pub(crate) async fn emit(events: &Arc<dyn EventSink>, event: AgentEvent) {
    let _delivered = events.emit(event).await;
}

// ===== merged from runtime_sessions.rs =====
// Session bookkeeping on the runtime: create, fork, switch head, and the DAG writes every
// other runtime subsystem shares.

use hrn_kernel::{DagPayload, ModelInfo, Msg, PermissionMode, ThinkingLevel};
use std::collections::HashMap;
use std::path::Path;

impl AgentRuntime {
    pub(crate) fn new_meta(&self, title: String, cwd: &Path) -> Result<SessionMeta> {
        let cwd = cwd.to_str().ok_or_else(|| {
            HrnError::Config(format!("cwd is not valid UTF-8: {}", cwd.display()))
        })?;
        let now = self.clock.now_ms();
        Ok(SessionMeta {
            id: self.next_id("sess"),
            title,
            cwd: cwd.to_owned(),
            branch: None,
            head_node_id: None,
            model: None,
            thinking: ThinkingLevel::default(),
            goal: None,
            permission_mode: PermissionMode::default(),
            parent_session_id: None,
            parent_node_id: None,
            status: AgentStatus::Idle,
            pinned: false,
            title_source: None,
            exit_ms: None,
            created_at_ms: now,
            updated_at_ms: now,
        })
    }

    /// Create and persist a new top-level session.
    ///
    /// # Errors
    /// Fails when `cwd` is not valid UTF-8 or the store rejects the session.
    pub fn create_session(
        &self,
        title: impl Into<String>,
        cwd: &Path,
        model: Option<String>,
        thinking: ThinkingLevel,
        permission_mode: PermissionMode,
    ) -> Result<SessionMeta> {
        let mut meta = self.new_meta(title.into(), cwd)?;
        meta.model = model;
        meta.thinking = thinking;
        meta.permission_mode = permission_mode;
        self.store.create_session(&meta)?;
        Ok(meta)
    }

    /// Create a hidden child (subagent) session linked to `parent` at `parent_node_id`.
    ///
    /// # Errors
    /// Fails when the parent cwd is not valid UTF-8 or the store rejects the session.
    pub fn create_child_session(
        &self,
        parent: &SessionMeta,
        parent_node_id: Option<NodeId>,
        title: impl Into<String>,
        model: Option<String>,
        permission_mode: PermissionMode,
    ) -> Result<SessionMeta> {
        self.create_child_session_in(
            parent,
            Path::new(&parent.cwd),
            parent_node_id,
            title,
            model,
            permission_mode,
        )
    }

    /// Create a hidden child (subagent) session rooted at an explicit `cwd` (e.g. an isolated
    /// git worktree) while staying linked to `parent`.
    pub(crate) fn create_child_session_in(
        &self,
        parent: &SessionMeta,
        cwd: &Path,
        parent_node_id: Option<NodeId>,
        title: impl Into<String>,
        model: Option<String>,
        permission_mode: PermissionMode,
    ) -> Result<SessionMeta> {
        let mut meta = self.new_meta(title.into(), cwd)?;
        meta.model = model.or_else(|| parent.model.clone());
        meta.thinking = parent.thinking;
        meta.permission_mode = permission_mode;
        meta.parent_session_id = Some(parent.id.clone());
        meta.parent_node_id = parent_node_id;
        self.store.create_session(&meta)?;
        Ok(meta)
    }

    /// `/fork`: copy the branch ending at `target_node_id` (default: head) into a new session.
    ///
    /// # Errors
    /// Fails when the session or node does not exist or persistence fails.
    pub fn fork_session(
        &self,
        source_session_id: &str,
        target_node_id: Option<&str>,
    ) -> Result<SessionMeta> {
        let source = self
            .store
            .get_session(source_session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{source_session_id}`")))?;
        let target = target_node_id
            .map(str::to_owned)
            .or_else(|| source.head_node_id.clone())
            .ok_or_else(|| HrnError::Store("cannot fork an empty session".into()))?;
        let branch = self.store.load_branch(source_session_id, &target)?;

        let mut forked =
            self.new_meta(format!("{} (fork)", source.title), Path::new(&source.cwd))?;
        forked.model.clone_from(&source.model);
        forked.thinking = source.thinking;
        forked.permission_mode = source.permission_mode;
        self.store.create_session(&forked)?;

        // Summary nodes reference earlier nodes by id; those ids change in the copy.
        let mut remap: HashMap<NodeId, NodeId> = HashMap::with_capacity(branch.len());
        let mut parent: Option<NodeId> = None;
        for node in branch {
            let new_id = self.next_id("node");
            remap.insert(node.id.clone(), new_id.clone());
            let payload = match node.payload {
                DagPayload::Summary {
                    covers_node_id,
                    text,
                    method,
                    preserve,
                } => {
                    let preserve = if method.as_deref() == Some("shake") {
                        preserve.map(|val| remap_shake_preserve(val, &remap))
                    } else {
                        preserve
                    };
                    DagPayload::Summary {
                        covers_node_id: remap
                            .get(&covers_node_id)
                            .cloned()
                            .unwrap_or(covers_node_id),
                        text,
                        method,
                        preserve,
                    }
                }
                DagPayload::Label {
                    target_node_id,
                    label,
                } => DagPayload::Label {
                    target_node_id: remap
                        .get(&target_node_id)
                        .cloned()
                        .unwrap_or(target_node_id),
                    label,
                },
                DagPayload::BranchSummary { from_node_id, text } => DagPayload::BranchSummary {
                    from_node_id: remap.get(&from_node_id).cloned().unwrap_or(from_node_id),
                    text,
                },
                other => other,
            };
            self.store.append_node(&DagNode {
                id: new_id.clone(),
                session_id: forked.id.clone(),
                parent_id: parent.take(),
                created_at_ms: node.created_at_ms,
                payload,
            })?;
            parent = Some(new_id);
        }
        forked.head_node_id = parent;
        self.store.update_session(&forked)?;
        Ok(forked)
    }

    /// Move `session_id`'s active branch head to `target_node_id` (an existing node in the same
    /// session), returning the updated session metadata and the active branch root-first.
    ///
    /// # Errors
    /// Fails when the session or node does not exist or persistence fails.
    pub fn switch_head(
        &self,
        session_id: &str,
        target_node_id: &str,
    ) -> Result<(SessionMeta, Vec<DagNode>)> {
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let branch = self.store.load_branch(session_id, target_node_id)?;
        meta.head_node_id = Some(target_node_id.to_owned());
        meta.updated_at_ms = self.clock.now_ms();
        self.store.update_session(&meta)?;
        self.invalidate_speculation(session_id);
        Ok((meta, branch))
    }

    /// Record a session-only temporary model switch (`/switch`, `Alt+P`, or context promotion) as
    /// a [`DagPayload::ModelChange`] node and update session metadata (never touches `[roles]`).
    ///
    /// # Errors
    /// Fails when the session does not exist or persistence fails.
    pub fn switch_model_temporary(
        &self,
        session_id: &str,
        selector: &str,
        resolved_model: &str,
        thinking: Option<ThinkingLevel>,
        role: Option<&str>,
    ) -> Result<SessionMeta> {
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let switch_role = role.unwrap_or("temporary").to_owned();
        self.append_payload(
            &mut meta,
            DagPayload::ModelChange {
                selector: selector.to_owned(),
                role: Some(switch_role),
            },
        )?;
        meta.model = Some(resolved_model.to_owned());
        if let Some(level) = thinking {
            meta.thinking = level;
        }
        meta.updated_at_ms = self.clock.now_ms();
        self.store.update_session(&meta)?;
        self.invalidate_speculation(session_id);
        Ok(meta)
    }

    /// Relocate an existing session's working directory (and optional git branch) in the store,
    /// used by `/wt` after creating a linked worktree and carrying uncommitted changes.
    ///
    /// # Errors
    /// Fails when `new_cwd` is not valid UTF-8, the session is unknown, or persistence fails.
    pub fn relocate_session(
        &self,
        session_id: &str,
        new_cwd: &Path,
        branch: Option<String>,
    ) -> Result<SessionMeta> {
        let cwd = new_cwd.to_str().ok_or_else(|| {
            HrnError::Config(format!("cwd is not valid UTF-8: {}", new_cwd.display()))
        })?;
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        cwd.clone_into(&mut meta.cwd);
        if branch.is_some() {
            meta.branch = branch;
        }
        meta.updated_at_ms = self.clock.now_ms();
        self.store.update_session(&meta)?;
        self.invalidate_speculation(session_id);
        Ok(meta)
    }

    /// Append `payload` after the session head and advance `meta.head_node_id`.
    pub(crate) fn append_payload(
        &self,
        meta: &mut SessionMeta,
        payload: DagPayload,
    ) -> Result<NodeId> {
        let node = DagNode {
            id: self.next_id("node"),
            session_id: meta.id.clone(),
            parent_id: meta.head_node_id.clone(),
            created_at_ms: self.clock.now_ms(),
            payload,
        };
        self.store.append_node(&node)?;
        meta.head_node_id = Some(node.id.clone());
        Ok(node.id)
    }

    pub(crate) fn append_message(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        msg: Msg,
    ) -> Result<NodeId> {
        self.append_payload(
            meta,
            DagPayload::Message {
                turn_id: turn_id.to_owned(),
                msg,
            },
        )
    }

    /// The system prompt a turn in `cwd` would send (rules and deferred hints included;
    /// pending advisor notes are previewed as the `[system]` message the turn would deliver).
    #[must_use]
    pub fn system_prompt_for(
        &self,
        cwd: &Path,
        control: &crate::control::SessionControl,
    ) -> String {
        let mut prompt = match self.rules_for_preview(cwd) {
            Ok(rules) => self.build_system_prompt(cwd, rules.as_ref(), None),
            Err(err) => {
                // A preview states what it could not show instead of pretending there are no rules.
                let mut prompt = self.build_system_prompt(cwd, None, None);
                let _ = std::fmt::write(
                    &mut prompt,
                    format_args!("\n\n(Rules could not be read: {err})"),
                );
                prompt
            }
        };
        let notes = control.peek_advisor_notes();
        if !notes.is_empty() {
            prompt.push_str("\n\n[system] message the next turn receives:\n");
            let _ = std::fmt::write(&mut prompt, format_args!("{}", notes.join("\n")));
        }
        prompt
    }

    fn rules_for_preview(&self, cwd: &Path) -> Result<Option<crate::rules::Rulebook>> {
        if !self.config.context.rules {
            return Ok(None);
        }
        crate::rules::Rulebook::discover(self.fs.as_ref(), cwd).map(Some)
    }

    /// Replace the session's todo checklist: append a `TodoBoard` node at the head and refresh
    /// the live board, so `/todo edit` survives resume, rewind and fork like a tool edit does.
    pub fn set_todo_board(&self, session_id: &str, phases: Vec<hrn_kernel::TodoPhase>) -> Result {
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        self.append_payload(
            &mut meta,
            DagPayload::TodoBoard {
                phases: phases.clone(),
            },
        )?;
        meta.updated_at_ms = self.clock.now_ms();
        self.store.update_session(&meta)?;
        let root = self.agents.root_of(session_id);
        self.todos.seed(&root, phases);
        Ok(())
    }

    /// The tools whose schemas ride in the next request: the active set plus this branch's
    /// activated deferred names.
    pub fn active_tools(&self, meta: &SessionMeta) -> Result<Vec<String>> {
        self.activated_tools(meta)
    }

    /// The tool schemas a turn with `activated` names would send.
    #[must_use]
    pub fn request_specs(&self, activated: &[String]) -> Vec<hrn_kernel::ToolSpec> {
        self.tools.specs_with(activated)
    }
}

fn remap_shake_preserve(
    mut value: serde_json::Value,
    remap: &HashMap<NodeId, NodeId>,
) -> serde_json::Value {
    if let Some(obj) = value.as_object_mut()
        && let Some(elisions) = obj.get("elisions").and_then(serde_json::Value::as_object)
    {
        let mut updated = serde_json::Map::with_capacity(elisions.len());
        for (old_id, text) in elisions {
            let new_id = remap.get(old_id).cloned().unwrap_or_else(|| old_id.clone());
            updated.insert(new_id, text.clone());
        }
        obj.insert("elisions".to_owned(), serde_json::Value::Object(updated));
    }
    value
}

/// Canonical `<provider>/<id>` key for `[compaction.model_thresholds]` matching.
pub(crate) fn resolve_model_key(models: &[ModelInfo], asked: Option<&str>) -> Option<String> {
    if let Some(m) = asked.map(str::trim).filter(|s| !s.is_empty()) {
        let base = hrn_kernel::split_level_suffix(m).0.trim();
        if let Some(info) = models.iter().find(|info| {
            info.alias == base
                || format!("{}/{}", info.provider, info.id) == base
                || info.id == base
        }) {
            return Some(format!("{}/{}", info.provider, info.id));
        }
        return Some(base.to_owned());
    }
    models
        .iter()
        .find(|m| m.default)
        .or_else(|| models.first())
        .map(|m| format!("{}/{}", m.provider, m.id))
}

/// The name a provider answered with (the bare upstream id) as hrn and omp name that model,
/// `<provider>/<id>`. A name already in that form, or one no configured model answers with (a
/// model that left the config), is kept as reported. When two providers serve the same id, the
/// alias the session asked for decides, else the default model, else the first.
pub(crate) fn model_name(models: &[ModelInfo], asked: Option<&str>, stored: &str) -> String {
    let qualified = |id: &str, provider: &str| format!("{provider}/{id}");
    if models
        .iter()
        .any(|m| qualified(&m.id, &m.provider) == stored)
    {
        return stored.to_owned();
    }
    let named: Vec<&ModelInfo> = models.iter().filter(|m| m.id == stored).collect();
    match named.as_slice() {
        [] => stored.to_owned(),
        [only] => qualified(&only.id, &only.provider),
        several => several
            .iter()
            .find(|m| asked == Some(m.alias.as_str()))
            .or_else(|| several.iter().find(|m| m.default))
            .or_else(|| several.first())
            .map_or_else(|| stored.to_owned(), |m| qualified(&m.id, &m.provider)),
    }
}

// ===== merged from runtime_compact.rs =====
// Compaction entry points of [`AgentRuntime`]: method walk (`handoff` / `shake` / `summary`),
// no-progress guard, custom `/compact [instructions]`, `/shake`, and idle compaction.

use crate::compaction::normalize_method_name;
use crate::compaction::plan_shake;
use crate::compaction::project_branch;
use crate::summary::{
    build_handoff_prompt, build_summary_prompt_with_instructions, extract_modified_files,
    extract_todos, has_goal_section, plan_summary_for_tokens,
};
use hrn_kernel::{LlmRequest, ToolContext};
use std::path::PathBuf;
use std::sync::Mutex;

/// Why a compaction walk was initiated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionReason {
    /// Automatic threshold maintenance (pre-step or post-turn).
    Threshold,
    /// Context-window overflow error recovery.
    Overflow,
    /// Model stopped with `FinishReason::Length`.
    IncompleteOutput,
    /// Manual `/compact [instructions]`.
    Manual,
    /// Manual `/shake` (aggressive zero-LLM tool-output elision).
    ManualShake,
    /// Automatic idle compaction (`[compaction.idle]`; never auto-continues).
    Idle,
}

/// Persisted result committed by one compaction method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionCommit {
    /// Covered DAG node id.
    pub covers_node_id: NodeId,
    /// Canonical method name (`"handoff"`, `"shake"`, `"summary"`).
    pub method: String,
    /// Projected tokens before compaction.
    pub tokens_before: u64,
    /// Projected tokens after compaction.
    pub tokens_after: u64,
    /// Approximate characters reclaimed.
    pub saved_chars: usize,
    /// Summary or shake description text stored in the DAG node.
    pub summary_text: String,
    /// True when projected tokens failed to decrease (blocks `auto_continue`).
    pub no_progress: bool,
}

/// Outcome of attempting a single compaction method in `method_order`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MethodOutcome {
    /// Method produced a compaction commit (check `commit.no_progress`).
    Committed(CompactionCommit),
    /// Method did not apply or could not meet its threshold; walk advances.
    Skipped(String),
}

/// Aggregate result of walking `method_order`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionWalkOutcome {
    /// Last committed compaction result with real token reduction, if any.
    pub committed: Option<CompactionCommit>,
    /// True when a method ran but did not reduce projected tokens.
    pub no_progress: bool,
    /// Whether `auto_continue` is permitted after this compaction (`false` on idle or no-progress).
    pub allow_auto_continue: bool,
    /// Diagnostic notes for skipped methods or no-progress warnings.
    pub notes: Vec<String>,
}

impl AgentRuntime {
    /// Walk `policy.config.method_order` until a method commits and brings context under control,
    /// enforcing the no-progress guard (blocks `auto_continue` with a warning when reduction <= 0).
    pub(crate) async fn compact_branch_walk(
        &self,
        meta: &mut SessionMeta,
        policy: &CompactionPolicy,
        reason: CompactionReason,
        instructions: Option<&str>,
        cancel: &CancelFlag,
        events: &Arc<dyn EventSink>,
    ) -> Result<CompactionWalkOutcome> {
        let methods = effective_methods(policy, reason);
        let trigger = policy.trigger_tokens();
        let mut last_commit: Option<CompactionCommit> = None;
        let mut notes = Vec::new();
        self.invalidate_speculation(&meta.id);

        for method in methods {
            let outcome = self
                .run_one_method(meta, policy, method, reason, instructions, cancel, events)
                .await?;
            match outcome {
                MethodOutcome::Skipped(why) => notes.push(format!("{method}: {why}")),
                MethodOutcome::Committed(commit) if commit.no_progress => {
                    let warning = format!(
                        "compaction ({}) made no progress ({} -> {} tokens); auto-continue blocked",
                        commit.method, commit.tokens_before, commit.tokens_after
                    );
                    emit(
                        events,
                        AgentEvent::StatusChanged {
                            session_id: meta.id.clone(),
                            status: AgentStatus::Working,
                            message: Some(warning.clone()),
                        },
                    )
                    .await;
                    notes.push(warning);
                    return Ok(CompactionWalkOutcome {
                        committed: None,
                        no_progress: true,
                        allow_auto_continue: false,
                        notes,
                    });
                }
                MethodOutcome::Committed(commit) => {
                    let below_trigger = trigger == 0 || commit.tokens_after < trigger;
                    let stop = matches!(
                        reason,
                        CompactionReason::Manual
                            | CompactionReason::ManualShake
                            | CompactionReason::Idle
                    ) || below_trigger
                        || commit.method != "shake";
                    last_commit = Some(commit);
                    if stop {
                        break;
                    }
                }
            }
        }

        let allow_auto_continue = last_commit.is_some()
            && policy.config.auto_continue
            && reason != CompactionReason::Idle;
        Ok(CompactionWalkOutcome {
            committed: last_commit,
            no_progress: false,
            allow_auto_continue,
            notes,
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "method dispatch needs session, policy, method, reason, instructions, cancel, events"
    )]
    async fn run_one_method(
        &self,
        meta: &mut SessionMeta,
        policy: &CompactionPolicy,
        method: &str,
        reason: CompactionReason,
        instructions: Option<&str>,
        cancel: &CancelFlag,
        events: &Arc<dyn EventSink>,
    ) -> Result<MethodOutcome> {
        let branch = self.load_head_branch(meta)?;
        match method {
            "remote" => {
                self.try_remote_method(meta, &branch, policy, instructions, cancel, events)
                    .await
            }
            "snapcompact" => {
                self.try_snapcompact_method(meta, &branch, policy, events)
                    .await
            }
            "shake" => {
                if instructions.is_some_and(|s| !s.trim().is_empty()) {
                    Ok(MethodOutcome::Skipped(
                        "custom instructions require an LLM summary".into(),
                    ))
                } else {
                    self.try_shake_method(
                        meta,
                        &branch,
                        policy,
                        reason == CompactionReason::ManualShake,
                        events,
                    )
                    .await
                }
            }
            "handoff" => {
                if reason == CompactionReason::Overflow {
                    Ok(MethodOutcome::Skipped(
                        "overflow recovery skips handoff generation".into(),
                    ))
                } else {
                    self.try_llm_method(
                        meta,
                        &branch,
                        policy,
                        "handoff",
                        instructions,
                        cancel,
                        events,
                    )
                    .await
                }
            }
            "summary" => {
                self.try_llm_method(
                    meta,
                    &branch,
                    policy,
                    "summary",
                    instructions,
                    cancel,
                    events,
                )
                .await
            }
            other => Ok(MethodOutcome::Skipped(format!("unknown method `{other}`"))),
        }
    }

    async fn try_shake_method(
        &self,
        meta: &mut SessionMeta,
        branch: &[DagNode],
        policy: &CompactionPolicy,
        aggressive: bool,
        events: &Arc<dyn EventSink>,
    ) -> Result<MethodOutcome> {
        let spill_ctx = self.build_spill_context(meta, events);
        let urls = Arc::clone(&self.urls);
        let plan = match plan_shake(branch, policy, aggressive, &|content| {
            urls.spill(&spill_ctx, content).ok().flatten()
        }) {
            Ok(plan) => plan,
            Err(why) => return Ok(MethodOutcome::Skipped(why)),
        };

        self.append_payload(
            meta,
            DagPayload::Summary {
                covers_node_id: plan.covers_node_id.clone(),
                text: plan.summary_text.clone(),
                method: Some("shake".to_owned()),
                preserve: Some(plan.preserve_value()),
            },
        )?;
        emit(
            events,
            AgentEvent::Compacted {
                covers_node_id: plan.covers_node_id.clone(),
                method: "shake".to_owned(),
                tokens_before: plan.tokens_before,
                tokens_after: plan.tokens_after,
                saved_chars: plan.saved_chars,
            },
        )
        .await;
        Ok(MethodOutcome::Committed(CompactionCommit {
            covers_node_id: plan.covers_node_id,
            method: "shake".to_owned(),
            tokens_before: plan.tokens_before,
            tokens_after: plan.tokens_after,
            saved_chars: plan.saved_chars,
            summary_text: plan.summary_text,
            no_progress: false,
        }))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "method attempt needs session, branch, policy, method name, instructions, cancel, events"
    )]
    async fn try_llm_method(
        &self,
        meta: &mut SessionMeta,
        branch: &[DagNode],
        policy: &CompactionPolicy,
        method: &str,
        instructions: Option<&str>,
        cancel: &CancelFlag,
        events: &Arc<dyn EventSink>,
    ) -> Result<MethodOutcome> {
        let keep_tok = policy.effective_keep_recent_tokens();
        let Some(plan) = plan_summary_for_tokens(branch, keep_tok, policy.keep_recent_turns, true)
        else {
            return Ok(MethodOutcome::Skipped("not enough history to fold".into()));
        };
        let before = project_branch(branch, policy);
        let folded_nodes = branch.get(..=plan.covers_idx).unwrap_or_default();
        let folded = project_branch(folded_nodes, policy);
        let todos = extract_todos(branch);
        let files = extract_modified_files(folded_nodes);
        let prompt = if method == "handoff" {
            build_handoff_prompt(&folded.messages, &todos, &files, instructions)
        } else {
            build_summary_prompt_with_instructions(&folded.messages, &todos, &files, instructions)
        };
        let text = self.summarize(meta, prompt, cancel).await?;
        if method == "handoff" && !has_goal_section(&text) {
            return Ok(MethodOutcome::Skipped(
                "handoff output missing required `## Goal` section".into(),
            ));
        }

        let after = projected_after_summary(branch, policy, &plan.covers_node_id, &text, method);
        if after.estimated_tokens >= before.estimated_tokens {
            return Ok(MethodOutcome::Committed(CompactionCommit {
                covers_node_id: plan.covers_node_id,
                method: method.to_owned(),
                tokens_before: before.estimated_tokens,
                tokens_after: after.estimated_tokens,
                saved_chars: 0,
                summary_text: text,
                no_progress: true,
            }));
        }

        let folded_chars: usize = folded.messages.iter().map(|m| m.content.len()).sum();
        let saved_chars = folded_chars.saturating_sub(text.len());
        self.append_payload(
            meta,
            DagPayload::Summary {
                covers_node_id: plan.covers_node_id.clone(),
                text: text.clone(),
                method: Some(method.to_owned()),
                preserve: None,
            },
        )?;
        emit(
            events,
            AgentEvent::Compacted {
                covers_node_id: plan.covers_node_id.clone(),
                method: method.to_owned(),
                tokens_before: before.estimated_tokens,
                tokens_after: after.estimated_tokens,
                saved_chars,
            },
        )
        .await;
        Ok(MethodOutcome::Committed(CompactionCommit {
            covers_node_id: plan.covers_node_id,
            method: method.to_owned(),
            tokens_before: before.estimated_tokens,
            tokens_after: after.estimated_tokens,
            saved_chars,
            summary_text: text,
            no_progress: false,
        }))
    }

    fn build_spill_context(&self, meta: &SessionMeta, events: &Arc<dyn EventSink>) -> ToolContext {
        ToolContext {
            session_id: meta.id.clone(),
            turn_id: "compact_shake".to_owned(),
            call_id: "shake".to_owned(),
            cwd: PathBuf::from(&meta.cwd),
            permission_mode: meta.permission_mode,
            cancel: CancelFlag::new(),
            events: Arc::clone(events),
            ask: None,
            approval: None,
            urls: Arc::clone(&self.urls),
            root_session_id: self.agents.root_of(&meta.id),
            inbox: Arc::clone(self.agents.inbox()),
            caller: None,
            todos: Arc::clone(&self.todos),
            yield_data: Arc::new(Mutex::new(None)),
            is_subagent: meta.parent_session_id.is_some(),
        }
    }

    pub(crate) async fn summarize(
        &self,
        meta: &SessionMeta,
        messages: Vec<Msg>,
        cancel: &CancelFlag,
    ) -> Result<String> {
        let mut request = LlmRequest {
            model: Some(self.config.fast_model_role.clone()),
            messages,
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            thinking_budgets: None,
            temperature: None,
            max_tokens: Some(4096),
            session_id: None,
            service_tier: None,
        };
        let response = match self.llm.stream_chat(&request, cancel, &NullSink).await {
            Err(HrnError::Config(_)) if meta.model != request.model => {
                request.model.clone_from(&meta.model);
                self.llm.stream_chat(&request, cancel, &NullSink).await?
            }
            other => other?,
        };
        self.record_usage(&meta.id, request.model.as_deref(), &response)?;
        let text = response.content.trim();
        if text.is_empty() {
            return Err(HrnError::Provider {
                message: "summarizer returned no text".into(),
                retryable: false,
                retry_after_secs: None,
            });
        }
        Ok(text.to_owned())
    }

    /// Run a full compaction walk for `session_id` with an explicit [`CompactionReason`] and
    /// optional user `instructions`.
    ///
    /// # Errors
    /// Fails when the session is unknown, cancelled, the summarizer fails, or persistence fails.
    pub async fn compact_session(
        &self,
        session_id: &str,
        reason: CompactionReason,
        instructions: Option<&str>,
        cancel: &CancelFlag,
        events: &Arc<dyn EventSink>,
    ) -> Result<CompactionWalkOutcome> {
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let policy = self.policy_for(meta.model.as_deref());
        let outcome = self
            .compact_branch_walk(&mut meta, &policy, reason, instructions, cancel, events)
            .await?;
        if outcome.committed.is_some() {
            meta.updated_at_ms = self.clock.now_ms();
            self.store.update_session(&meta)?;
        }
        Ok(outcome)
    }

    /// `/compact [instructions]`: run compaction now. Returns the covered node id, or `None` when
    /// nothing could be compacted.
    ///
    /// # Errors
    /// Fails when the session is unknown, the summarizer fails, or persistence fails.
    pub async fn compact_now(
        &self,
        session_id: &str,
        instructions: Option<&str>,
        cancel: &CancelFlag,
        events: &Arc<dyn EventSink>,
    ) -> Result<Option<NodeId>> {
        let outcome = self
            .compact_session(
                session_id,
                CompactionReason::Manual,
                instructions,
                cancel,
                events,
            )
            .await?;
        Ok(outcome.committed.map(|c| c.covers_node_id))
    }

    /// `/shake`: run aggressive zero-LLM tool-output elision without the 20k savings floor.
    ///
    /// # Errors
    /// Fails when the session is unknown or persistence fails.
    pub async fn shake_now(
        &self,
        session_id: &str,
        cancel: &CancelFlag,
        events: &Arc<dyn EventSink>,
    ) -> Result<Option<NodeId>> {
        let outcome = self
            .compact_session(
                session_id,
                CompactionReason::ManualShake,
                None,
                cancel,
                events,
            )
            .await?;
        Ok(outcome.committed.map(|c| c.covers_node_id))
    }

    /// Trigger idle-session compaction when `[compaction.idle]` is enabled, `idle_secs` meets the
    /// clamped timeout (>= 60 s), and projected tokens meet `idle.threshold_tokens`.
    /// Always returns `allow_auto_continue == false`.
    ///
    /// # Errors
    /// Fails when the session is unknown, cancelled, the summarizer fails, or persistence fails.
    pub async fn compact_idle(
        &self,
        session_id: &str,
        idle_secs: u64,
        cancel: &CancelFlag,
        events: &Arc<dyn EventSink>,
    ) -> Result<CompactionWalkOutcome> {
        let cfg = &self.config.compaction.config;
        if !cfg.enabled || !cfg.idle.enabled || idle_secs < cfg.idle.effective_timeout_secs() {
            return Ok(CompactionWalkOutcome {
                committed: None,
                no_progress: false,
                allow_auto_continue: false,
                notes: vec!["idle compaction criteria not met".into()],
            });
        }
        let meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let branch = self.load_head_branch(&meta)?;
        let policy = self.policy_for(meta.model.as_deref());
        let projected = project_branch(&branch, &policy);
        if projected.estimated_tokens < cfg.idle.threshold_tokens {
            return Ok(CompactionWalkOutcome {
                committed: None,
                no_progress: false,
                allow_auto_continue: false,
                notes: vec![format!(
                    "idle tokens ({}) below threshold ({})",
                    projected.estimated_tokens, cfg.idle.threshold_tokens
                )],
            });
        }
        self.compact_session(session_id, CompactionReason::Idle, None, cancel, events)
            .await
    }
}

pub(crate) fn effective_methods(
    policy: &CompactionPolicy,
    reason: CompactionReason,
) -> Vec<&'static str> {
    if reason == CompactionReason::ManualShake {
        return vec!["shake"];
    }
    let list: Vec<&'static str> = policy
        .config
        .method_order
        .iter()
        .filter_map(|m| normalize_method_name(m))
        .collect();
    if list.is_empty() {
        vec!["summary"]
    } else {
        list
    }
}

fn projected_after_summary(
    branch: &[DagNode],
    policy: &CompactionPolicy,
    covers_node_id: &str,
    text: &str,
    method: &str,
) -> crate::compaction::ProjectedContext {
    projected_after_summary_with_preserve(branch, policy, covers_node_id, text, method, None)
}

pub(crate) fn projected_after_summary_with_preserve(
    branch: &[DagNode],
    policy: &CompactionPolicy,
    covers_node_id: &str,
    text: &str,
    method: &str,
    preserve: Option<serde_json::Value>,
) -> crate::compaction::ProjectedContext {
    let mut trial = branch.to_vec();
    trial.push(DagNode {
        id: "__summary_trial__".to_owned(),
        session_id: branch
            .first()
            .map(|n| n.session_id.clone())
            .unwrap_or_default(),
        parent_id: branch.last().map(|n| n.id.clone()),
        created_at_ms: 0,
        payload: DagPayload::Summary {
            covers_node_id: covers_node_id.to_owned(),
            text: text.to_owned(),
            method: Some(method.to_owned()),
            preserve,
        },
    });
    project_branch(&trial, policy)
}

// ===== merged from runtime_restore.rs =====
// Crash-interrupt diagnosis, pairing repair, fatal exit recording, and `--restore` turn replay.
//
// Normal `--resume` repairs any unanswered tool-call tail without calling the LLM so transcripts
// and providers always see valid `tool_use` / `tool_result` pairing. Only `--restore` calls
// [`AgentRuntime::restore_turn`] to continue the interrupted turn.

use crate::turn::StepEnv;
use hrn_kernel::{ExitReason, PendingToolCall, Role, ToolCall};
use std::collections::HashSet;

/// Synthetic error text appended for each unanswered tool call during interrupt repair.
pub const INTERRUPTED_TOOL_RESULT: &str =
    "[outcome unknown — the process was interrupted before this call completed]";

/// Synthetic assistant message appended after repaired tool results on normal resume.
pub const INTERRUPTED_ASSISTANT_TEXT: &str = "[session interrupted before turn completed]";

/// Lifecycle summary shown per row in the session picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionLifecycle {
    /// Last turn finished cleanly (or session has no unfinished turn).
    Complete,
    /// Process exited mid-turn without a normal or user-interrupt exit record.
    Interrupted,
    /// User interrupted the last turn (Esc; final and never auto-restored).
    Aborted,
    /// Last turn ended with a runtime or provider error.
    Error,
}

impl SessionLifecycle {
    /// Compact lowercase label rendered in picker rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Interrupted => "interrupted",
            Self::Aborted => "aborted",
            Self::Error => "error",
        }
    }
}

/// Diagnosis of a session's active branch tail after an unexpected process termination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterruptDiagnosis {
    /// No unfinished turn tail (clean exit, user Esc interrupt, or empty session).
    Clean,
    /// Last assistant turn emitted tool calls whose results were cut off before completion.
    UnansweredToolCalls {
        /// Turn id of the interrupted assistant message.
        turn_id: String,
        /// Latest user prompt on the branch (for thinking/keyword context).
        prompt: String,
        /// Tool calls that never received a `Role::Tool` result, in original order.
        unanswered: Vec<ToolCall>,
        /// `true` when zero calls in the batch completed before interruption (safe to re-drive).
        all_cut_off: bool,
    },
    /// A user message (or completed tool batch) was left pending while `status == Working`.
    UnansweredUser {
        /// Turn id of the pending message.
        turn_id: String,
        /// Prompt text of the latest user message on the branch.
        prompt: String,
    },
}

impl InterruptDiagnosis {
    /// Whether this diagnosis represents an unfinished interrupted turn.
    #[must_use]
    pub const fn is_interrupted(&self) -> bool {
        !matches!(self, Self::Clean)
    }
}

/// Latest user message content on `branch`, if any.
fn latest_user_prompt(branch: &[DagNode]) -> String {
    branch
        .iter()
        .rev()
        .find_map(|n| match &n.payload {
            DagPayload::Message { msg, .. } if msg.role == Role::User => Some(msg.content.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Inspect `meta` and its chronological `branch` to determine whether the last turn crashed mid-way.
///
/// A trailing [`DagPayload::SessionExit`] with [`ExitReason::Normal`] or
/// [`ExitReason::Interrupted`] (user Esc) suppresses diagnosis; [`ExitReason::Fatal`] does not.
#[must_use]
pub fn diagnose_interrupt(meta: &SessionMeta, branch: &[DagNode]) -> InterruptDiagnosis {
    let mut answered_ids: HashSet<&str> = HashSet::new();
    let mut saw_message = false;
    let mut trailing_tool_turn: Option<String> = None;

    for node in branch.iter().rev() {
        match &node.payload {
            DagPayload::SessionExit {
                reason: ExitReason::Normal | ExitReason::Interrupted,
                ..
            } if !saw_message => return InterruptDiagnosis::Clean,
            DagPayload::Message { turn_id, msg } => {
                saw_message = true;
                match msg.role {
                    Role::Tool => {
                        if let Some(id) = msg.tool_call_id.as_deref() {
                            answered_ids.insert(id);
                        }
                        if trailing_tool_turn.is_none() {
                            trailing_tool_turn = Some(turn_id.clone());
                        }
                    }
                    Role::Assistant => {
                        if !msg.tool_calls.is_empty() {
                            let unanswered: Vec<ToolCall> = msg
                                .tool_calls
                                .iter()
                                .filter(|c| !answered_ids.contains(c.id.as_str()))
                                .cloned()
                                .collect();
                            if !unanswered.is_empty() {
                                let all_cut_off = unanswered.len() == msg.tool_calls.len();
                                return InterruptDiagnosis::UnansweredToolCalls {
                                    turn_id: turn_id.clone(),
                                    prompt: latest_user_prompt(branch),
                                    unanswered,
                                    all_cut_off,
                                };
                            }
                            if meta.status == AgentStatus::Working
                                && let Some(tool_turn) = trailing_tool_turn
                            {
                                return InterruptDiagnosis::UnansweredUser {
                                    turn_id: tool_turn,
                                    prompt: latest_user_prompt(branch),
                                };
                            }
                        }
                        return InterruptDiagnosis::Clean;
                    }
                    Role::User => {
                        if meta.status == AgentStatus::Working {
                            return InterruptDiagnosis::UnansweredUser {
                                turn_id: turn_id.clone(),
                                prompt: msg.content.clone(),
                            };
                        }
                        return InterruptDiagnosis::Clean;
                    }
                    Role::System => return InterruptDiagnosis::Clean,
                }
            }
            _ => {}
        }
    }
    InterruptDiagnosis::Clean
}

/// Classify a session's picker lifecycle (`complete | interrupted | aborted | error`).
#[must_use]
pub fn session_lifecycle(meta: &SessionMeta, branch: &[DagNode]) -> SessionLifecycle {
    if meta.status == AgentStatus::Error {
        return SessionLifecycle::Error;
    }
    if diagnose_interrupt(meta, branch).is_interrupted() {
        return SessionLifecycle::Interrupted;
    }
    for node in branch.iter().rev() {
        match &node.payload {
            DagPayload::SessionExit {
                reason: ExitReason::Interrupted,
                ..
            } => return SessionLifecycle::Aborted,
            DagPayload::SessionExit {
                reason: ExitReason::Fatal,
                ..
            } => return SessionLifecycle::Interrupted,
            DagPayload::SessionExit {
                reason: ExitReason::Normal,
                ..
            }
            | DagPayload::Message { .. } => break,
            _ => {}
        }
    }
    if matches!(meta.status, AgentStatus::Working | AgentStatus::Blocked) {
        SessionLifecycle::Interrupted
    } else {
        SessionLifecycle::Complete
    }
}

/// Unanswered tool calls currently at the tail of `branch`, serialized for `SessionExit.pending`.
#[must_use]
pub fn pending_tool_calls(branch: &[DagNode]) -> Vec<PendingToolCall> {
    let mut answered: HashSet<&str> = HashSet::new();
    for node in branch.iter().rev() {
        if let DagPayload::Message { msg, .. } = &node.payload {
            match msg.role {
                Role::Tool => {
                    if let Some(id) = msg.tool_call_id.as_deref() {
                        answered.insert(id);
                    }
                }
                Role::Assistant => {
                    return msg
                        .tool_calls
                        .iter()
                        .filter(|c| !answered.contains(c.id.as_str()))
                        .map(|c| PendingToolCall {
                            id: c.id.clone(),
                            name: c.name.clone(),
                            arguments: c.arguments.clone(),
                        })
                        .collect();
                }
                Role::User | Role::System => return Vec::new(),
            }
        }
    }
    Vec::new()
}

/// Best-effort fatal exit record written by panic / fatal hooks when the process still survives.
///
/// # Errors
/// Returns a store error if loading the session or appending the exit node fails.
pub fn record_fatal_exit(store: &dyn SessionStore, session_id: &str, now_ms: i64) -> Result<bool> {
    let Some(mut meta) = store.get_session(session_id)? else {
        return Ok(false);
    };
    let branch = match &meta.head_node_id {
        Some(head) => store.load_branch(session_id, head)?,
        None => Vec::new(),
    };
    let pending = pending_tool_calls(&branch);
    let node = DagNode {
        id: format!("node_fatal_{now_ms:x}_{:x}", std::process::id()),
        session_id: meta.id.clone(),
        parent_id: meta.head_node_id.clone(),
        created_at_ms: now_ms,
        payload: DagPayload::SessionExit {
            reason: ExitReason::Fatal,
            pending,
        },
    };
    store.append_node(&node)?;
    meta.head_node_id = Some(node.id);
    meta.exit_ms = Some(now_ms);
    meta.updated_at_ms = now_ms;
    store.update_session(&meta)?;
    Ok(true)
}

impl AgentRuntime {
    /// Append a [`DagPayload::SessionExit`] node and stamp `meta.exit_ms`.
    pub(crate) fn record_session_exit(
        &self,
        meta: &mut SessionMeta,
        reason: ExitReason,
        pending: Vec<PendingToolCall>,
    ) -> Result {
        let now = self.clock.now_ms();
        self.append_payload(meta, DagPayload::SessionExit { reason, pending })?;
        meta.exit_ms = Some(now);
        Ok(())
    }

    /// Finalize either a normal turn or a restored turn: run `turn_end` hooks, append the
    /// appropriate [`DagPayload::SessionExit`], update session status, and emit `TurnDone`/`TurnError`.
    pub(crate) async fn finish_turn(
        &self,
        meta: &mut SessionMeta,
        turn_id: String,
        outcome: Result<crate::turn::DrivenTurn>,
        io: &TurnIo,
    ) -> Result<TurnResult> {
        match outcome {
            Ok(crate::turn::DrivenTurn {
                result: done,
                todo_replanned,
            }) => {
                self.fire_turn_end(meta, &turn_id, &done, io).await?;
                self.record_session_exit(meta, ExitReason::Normal, Vec::new())?;
                self.set_status(meta, AgentStatus::Idle, None, &io.events)
                    .await?;
                emit(
                    &io.events,
                    AgentEvent::TurnDone {
                        turn_id: turn_id.clone(),
                        model: done.model.clone(),
                        text: done.final_text.clone(),
                        usage: done.usage,
                        steps: done.steps,
                    },
                )
                .await;
                self.maybe_auto_title(meta, &turn_id, &done.final_text, todo_replanned, io)
                    .await;
                Ok(TurnResult {
                    head_node_id: meta.head_node_id.clone(),
                    ..done
                })
            }
            Err(err) => {
                let (status, reason) = if matches!(err, HrnError::Cancelled) {
                    (AgentStatus::Idle, ExitReason::Interrupted)
                } else {
                    (AgentStatus::Error, ExitReason::Normal)
                };
                let pending = if reason == ExitReason::Interrupted {
                    Vec::new()
                } else {
                    self.load_head_branch(meta)
                        .map(|branch| pending_tool_calls(&branch))
                        .unwrap_or_default()
                };
                let mut message = err.to_string();
                if let Err(exit_err) = self.record_session_exit(meta, reason, pending) {
                    message = format!("{message} (also failed to record exit: {exit_err})");
                }
                if let Err(status_err) = self.set_status(meta, status, None, &io.events).await {
                    message = format!("{message} (also failed to persist status: {status_err})");
                }
                emit(
                    &io.events,
                    AgentEvent::TurnError {
                        turn_id,
                        code: err.code().to_owned(),
                        message,
                    },
                )
                .await;
                Err(err)
            }
        }
    }

    /// Diagnose whether `session_id` has an interrupted turn tail.
    ///
    /// # Errors
    /// Fails if the session does not exist or its branch cannot be loaded.
    pub fn diagnose_session(&self, session_id: &str) -> Result<InterruptDiagnosis> {
        let meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let branch = self.load_head_branch(&meta)?;
        Ok(diagnose_interrupt(&meta, &branch))
    }

    /// Repair an interrupted session tail for display and provider pairing validity without
    /// sending any LLM request. Returns `Ok(true)` when a repair was written.
    ///
    /// # Errors
    /// Fails if the session does not exist or persistence fails.
    pub fn repair_interrupted(&self, session_id: &str) -> Result<bool> {
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let branch = self.load_head_branch(&meta)?;
        match diagnose_interrupt(&meta, &branch) {
            InterruptDiagnosis::Clean => Ok(false),
            InterruptDiagnosis::UnansweredToolCalls {
                turn_id,
                unanswered,
                ..
            } => {
                self.append_interrupted_tool_results(&mut meta, &turn_id, &unanswered)?;
                self.append_message(
                    &mut meta,
                    &turn_id,
                    Msg::assistant(INTERRUPTED_ASSISTANT_TEXT),
                )?;
                meta.status = AgentStatus::Idle;
                meta.updated_at_ms = self.clock.now_ms();
                self.store.update_session(&meta)?;
                Ok(true)
            }
            InterruptDiagnosis::UnansweredUser { .. } => {
                meta.status = AgentStatus::Idle;
                meta.updated_at_ms = self.clock.now_ms();
                self.store.update_session(&meta)?;
                Ok(true)
            }
        }
    }

    /// Continue an interrupted session turn (`hrn chat --restore`):
    /// - Re-drives the tool batch when every call of the batch was cut off, then runs the LLM loop.
    /// - Repairs missing tool results when only part of a batch was cut off, then runs the LLM loop.
    /// - Continues directly from an unanswered user prompt while `status == Working`.
    /// - Returns `Ok(None)` without calling the LLM when nothing was left unfinished.
    ///
    /// # Errors
    /// Returns any turn failure or [`HrnError::Cancelled`].
    pub async fn restore_turn(
        &self,
        session_id: &str,
        control: &SessionControl,
        io: TurnIo,
    ) -> Result<Option<TurnResult>> {
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let branch = self.load_head_branch(&meta)?;
        let diagnosis = diagnose_interrupt(&meta, &branch);
        if !diagnosis.is_interrupted() {
            return Ok(None);
        }

        let guard = control.begin_turn(io.cancel.clone())?;
        let (turn_id, prompt) = match &diagnosis {
            InterruptDiagnosis::UnansweredToolCalls {
                turn_id, prompt, ..
            }
            | InterruptDiagnosis::UnansweredUser { turn_id, prompt } => {
                (turn_id.clone(), prompt.clone())
            }
            InterruptDiagnosis::Clean => unreachable!("checked above"),
        };

        self.set_status(&mut meta, AgentStatus::Working, None, &io.events)
            .await?;

        let outcome = self
            .drive_restore(&mut meta, &turn_id, &prompt, diagnosis, control, &io)
            .await;
        drop(guard);

        self.finish_turn(&mut meta, turn_id, outcome, &io)
            .await
            .map(Some)
    }

    async fn drive_restore(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        prompt: &str,
        diagnosis: InterruptDiagnosis,
        control: &SessionControl,
        io: &TurnIo,
    ) -> Result<crate::turn::DrivenTurn> {
        let mut ctx = self.prepare_turn_context(meta, prompt, io).await?;
        if let InterruptDiagnosis::UnansweredToolCalls {
            unanswered,
            all_cut_off,
            ..
        } = diagnosis
        {
            if all_cut_off {
                let step = StepEnv {
                    hooks: &ctx.hooks,
                    control,
                    io,
                };
                self.run_tool_calls(
                    meta,
                    turn_id,
                    &step,
                    &mut ctx.st.activated,
                    &unanswered,
                    false,
                )
                .await?;
            } else {
                // Repair only the missing tool results so the provider sees valid tool_use/tool_result
                // pairing before the follow-up LLM step continues the turn.
                self.append_interrupted_tool_results(meta, turn_id, &unanswered)?;
            }
        }
        self.drive_steps(meta, turn_id, prompt, &mut ctx, control, io)
            .await
    }

    fn append_interrupted_tool_results(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        unanswered: &[ToolCall],
    ) -> Result {
        for call in unanswered {
            let mut msg = Msg::tool_result(
                call.id.clone(),
                call.name.clone(),
                INTERRUPTED_TOOL_RESULT.to_owned(),
            );
            msg.is_error = true;
            self.append_message(meta, turn_id, msg)?;
        }
        Ok(())
    }
}

// ===== merged from runtime_handoff.rs =====
// `/handoff [focus]` generation and deferred commit on [`AgentRuntime`].

use crate::compaction::is_full_summary;

/// A generated `/handoff` summary waiting to be committed at a tool boundary or when idle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffDraft {
    /// Owning session id.
    pub session_id: String,
    /// Head node observed when `/handoff` started (used to detect branch changes / newer summaries).
    pub expected_head: NodeId,
    /// Last node covered by the handoff summary.
    pub covers_node_id: NodeId,
    /// Validated handoff markdown (guaranteed to contain `## Goal`).
    pub text: String,
    /// Estimated tokens before handoff.
    pub tokens_before: u64,
    /// Estimated tokens after handoff.
    pub tokens_after: u64,
    /// Characters folded by the handoff.
    pub saved_chars: usize,
}

impl AgentRuntime {
    /// Generate a `/handoff [focus]` brief without committing it yet. Fails if the generated text
    /// omits the required `## Goal` section. Returns `Ok(None)` for an empty session.
    ///
    /// # Errors
    /// Fails when the session is unknown, cancelled, the LLM errors, or `## Goal` is missing.
    pub async fn generate_handoff(
        &self,
        session_id: &str,
        focus: Option<&str>,
        cancel: &CancelFlag,
    ) -> Result<Option<HandoffDraft>> {
        let meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let branch = self.load_head_branch(&meta)?;
        let has_messages = branch
            .iter()
            .any(|n| matches!(n.payload, DagPayload::Message { .. }));
        let Some(head_id) = meta.head_node_id.clone().filter(|_| has_messages) else {
            return Ok(None);
        };
        let policy = self.policy_for(meta.model.as_deref());
        let projected = project_branch(&branch, &policy);
        let prompt = build_handoff_prompt(
            &projected.messages,
            &extract_todos(&branch),
            &extract_modified_files(&branch),
            focus,
        );
        let text = self.summarize(&meta, prompt, cancel).await?;
        if !has_goal_section(&text) {
            return Err(HrnError::Provider {
                message: "handoff output missing required `## Goal` section".into(),
                retryable: false,
                retry_after_secs: None,
            });
        }
        let folded_chars: usize = projected.messages.iter().map(|m| m.content.len()).sum();
        let saved_chars = folded_chars.saturating_sub(text.len());
        let tokens_before = u64::try_from(folded_chars / 4).unwrap_or(u64::MAX);
        let tokens_after = u64::try_from(text.len() / 4).unwrap_or(u64::MAX);
        Ok(Some(HandoffDraft {
            session_id: meta.id,
            expected_head: head_id.clone(),
            covers_node_id: head_id,
            text,
            tokens_before,
            tokens_after,
            saved_chars,
        }))
    }

    /// Commit a previously generated [`HandoffDraft`] if the branch still descends from
    /// `draft.expected_head` and no newer compaction summary was appended after it. Returns
    /// `Ok(false)` (discarding the draft) if the branch changed or a newer summary landed.
    ///
    /// # Errors
    /// Fails if the session cannot be loaded or updated.
    pub async fn commit_handoff(
        &self,
        draft: &HandoffDraft,
        events: &Arc<dyn EventSink>,
    ) -> Result<bool> {
        let Some(mut meta) = self.store.get_session(&draft.session_id)? else {
            return Ok(false);
        };
        let branch = self.load_head_branch(&meta)?;
        let Some(anchor) = branch.iter().position(|n| n.id == draft.expected_head) else {
            return Ok(false);
        };
        let newer_summary = branch
            .get(anchor + 1..)
            .unwrap_or_default()
            .iter()
            .any(|n| is_full_summary(&n.payload));
        if newer_summary {
            return Ok(false);
        }
        self.append_payload(
            &mut meta,
            DagPayload::Summary {
                covers_node_id: draft.covers_node_id.clone(),
                text: draft.text.clone(),
                method: Some("handoff".into()),
                preserve: None,
            },
        )?;
        meta.updated_at_ms = self.clock.now_ms();
        self.store.update_session(&meta)?;
        self.invalidate_speculation(&draft.session_id);
        emit(
            events,
            AgentEvent::Compacted {
                covers_node_id: draft.covers_node_id.clone(),
                method: "handoff".into(),
                tokens_before: draft.tokens_before,
                tokens_after: draft.tokens_after,
                saved_chars: draft.saved_chars,
            },
        )
        .await;
        Ok(true)
    }

    /// `/handoff [focus]`: generate and immediately commit a handoff summary when idle.
    ///
    /// # Errors
    /// Fails when generation fails (including missing `## Goal`), cancellation, or persistence fails.
    pub async fn handoff_now(
        &self,
        session_id: &str,
        focus: Option<&str>,
        cancel: &CancelFlag,
        events: &Arc<dyn EventSink>,
    ) -> Result<Option<NodeId>> {
        let Some(draft) = self.generate_handoff(session_id, focus, cancel).await? else {
            return Ok(None);
        };
        if self.commit_handoff(&draft, events).await? {
            Ok(Some(draft.covers_node_id))
        } else {
            Ok(None)
        }
    }
}

// ===== merged from runtime_goal_api.rs =====
// The goal-mode session API of the runtime: `--goal` / `/goal <objective>` create, read and
// clear the session's tracked objective; the turn loop does the accounting and transitions.

impl AgentRuntime {
    /// Start (or replace) the session's goal-mode objective and persist it (omp's `--goal` /
    /// `/goal <objective>`).
    ///
    /// # Errors
    /// The session is unknown or the store rejects the update.
    pub fn start_goal(
        &self,
        session_id: &str,
        objective: &str,
        token_budget: Option<u64>,
        time_budget_secs: Option<u64>,
    ) -> Result<hrn_kernel::Goal> {
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let now = self.clock.now_ms();
        let (text, _) = crate::goal::apply_op(
            &mut meta.goal,
            hrn_kernel::GoalOp::Create,
            Some(objective),
            token_budget,
            time_budget_secs,
            now,
        )
        .map_err(HrnError::Tool)?;
        let _ = text;
        self.store.update_session(&meta)?;
        meta.goal
            .clone()
            .ok_or_else(|| HrnError::Internal("goal vanished after create".to_owned()))
    }

    /// Clear the session's goal (omp's `/goal drop`), persisting the change.
    ///
    /// # Errors
    /// The session is unknown or the store rejects the update.
    pub fn drop_goal(&self, session_id: &str) -> Result {
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        meta.goal = None;
        self.store.update_session(&meta)
    }

    /// Read the session's current goal.
    ///
    /// # Errors
    /// The session is unknown.
    pub fn goal_of(&self, session_id: &str) -> Result<Option<hrn_kernel::Goal>> {
        Ok(self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?
            .goal)
    }
}

// ===== merged from runtime_title.rs =====
// Model-generated session titles (`title -> tiny -> smol -> session model`), revision-guarded
// against concurrent user `/rename` and session switches, plus bare `/rename` regeneration and
// todo-replan title refresh.

use crate::prompt_data::TITLE as TITLE_PROMPT;
use crate::summary::latest_todo_board;
use hrn_kernel::{SessionId, TitleSource, TodoPhase};

/// Maximum characters kept in a generated session title.
const MAX_TITLE_CHARS: usize = 80;
/// Maximum output tokens requested from the title model.
const TITLE_MAX_TOKENS: u32 = 64;
/// Maximum prompt/reply snippet characters sent to the title model.
const DIGEST_SNIPPET_CHARS: usize = 1200;

/// Per-session monotonic revision counters preventing stale auto-title writes.
pub(crate) type TitleRevs = Arc<Mutex<HashMap<SessionId, u64>>>;

/// Parse `<title>...</title>` from a title-model reply.
///
/// Returns `None` for `<title/>`, `none` (case-insensitive), empty titles, or responses missing
/// the `<title>` tag.
#[must_use]
pub fn parse_title_response(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "none" || lower.contains("<title/>") || lower.contains("<title />") {
        return None;
    }
    let open = lower.find("<title>")?;
    let after_open = open + "<title>".len();
    let close_rel = lower.get(after_open..)?.find("</title>")?;
    let inner = trimmed.get(after_open..after_open + close_rel)?;
    let cleaned = clean_title_text(inner);
    if cleaned.is_empty()
        || cleaned.eq_ignore_ascii_case("none")
        || cleaned.eq_ignore_ascii_case("untitled")
        || cleaned.eq_ignore_ascii_case("n/a")
    {
        return None;
    }
    Some(cleaned)
}

fn clean_title_text(raw: &str) -> String {
    let stripped = raw
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*'))
        .trim();
    let filtered: String = stripped
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = filtered.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_end_matches(['.', ':', ';']).trim();
    match trimmed.char_indices().nth(MAX_TITLE_CHARS) {
        Some((cut, _)) => trimmed.get(..cut).unwrap_or(trimmed).trim().to_owned(),
        None => trimmed.to_owned(),
    }
}

/// Structural signature of a todo board (phase names + task texts, ignoring status transitions)
/// so status progress (`pending -> in_progress -> completed`) does NOT count as a replan while
/// adding, removing, or replacing tasks DOES.
pub(crate) fn todo_plan_signature(phases: &[TodoPhase]) -> Vec<(String, Vec<String>)> {
    phases
        .iter()
        .filter(|p| !p.tasks.is_empty())
        .map(|p| {
            (
                p.name.clone(),
                p.tasks.iter().map(|t| t.content.clone()).collect(),
            )
        })
        .collect()
}

/// Hard wall-clock cap for background post-turn auto-title generation.
const AUTO_TITLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
/// Hard wall-clock cap for explicit `/rename` title regeneration.
const REGEN_TITLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Snapshot prepared synchronously right after `TurnDone` so background title generation cannot
/// race with `record_session_exit` or stall turn completion.
struct AutoTitleJob {
    meta: SessionMeta,
    digest: String,
    start_rev: u64,
    start_title: String,
    start_source: Option<TitleSource>,
}

/// Whether the turn contains a substantive prompt and reply worth auto-titling.
#[must_use]
pub(crate) fn is_substantive_turn(prompt: &str, reply: &str, used_tools: bool) -> bool {
    let p = prompt.trim();
    let r = reply.trim();
    if p.is_empty() || r.is_empty() {
        return false;
    }
    if used_tools {
        return true;
    }
    let compact: String = p
        .to_lowercase()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_owned();
    if compact.chars().count() < 4 {
        return false;
    }
    !matches!(
        compact.as_str(),
        "hi" | "hey"
            | "hello"
            | "yo"
            | "ok"
            | "okay"
            | "thanks"
            | "thank you"
            | "thx"
            | "ty"
            | "yes"
            | "no"
            | "yep"
            | "nope"
            | "ping"
            | "test"
            | "bye"
            | "good morning"
            | "good night"
            | "привет"
            | "прив"
            | "приветик"
            | "здравствуй"
            | "здравствуйте"
            | "хай"
            | "хелло"
            | "хэллоу"
            | "салам"
            | "салют"
            | "йоу"
            | "окей"
            | "ладно"
            | "понятно"
            | "ясно"
            | "спасибо"
            | "благодарю"
            | "пока"
            | "до свидания"
            | "доброе утро"
            | "добрый день"
            | "добрый вечер"
            | "спокойной ночи"
            | "тест"
            | "пинг"
            | "проверка"
    )
}

impl AgentRuntime {
    /// Clone this runtime with automatic post-turn session titling enabled or disabled.
    #[must_use]
    pub fn with_auto_title(&self, enabled: bool) -> Self {
        let mut config = self.config.clone();
        config.auto_title = enabled;
        Self {
            config,
            ..self.clone()
        }
    }

    /// Clone this runtime with automatic title refresh on todo replan enabled or disabled.
    #[must_use]
    pub fn with_title_refresh_on_replan(&self, enabled: bool) -> Self {
        let mut config = self.config.clone();
        config.title_refresh_on_replan = enabled;
        Self {
            config,
            ..self.clone()
        }
    }

    /// Current title revision counter for `session_id` (0 before any title mutation).
    #[must_use]
    pub fn title_revision(&self, session_id: &str) -> u64 {
        self.title_revs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }

    /// Increment the revision guard for `session_id` so any in-flight auto-title discards its write.
    pub fn bump_title_revision(&self, session_id: &str) -> u64 {
        let mut map = self
            .title_revs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = map.entry(session_id.to_owned()).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }

    /// Rename `session_id` in the store and bump its revision guard so in-flight auto-titles never
    /// overwrite concurrent user renames.
    ///
    /// # Errors
    /// Returns a store error if the session does not exist or cannot be updated.
    pub fn rename_session(&self, session_id: &str, title: &str, source: TitleSource) -> Result {
        self.bump_title_revision(session_id);
        self.store.rename_session(session_id, title, source)
    }

    /// Dispatch model auto-titling after `TurnDone` when enabled and eligible.
    ///
    /// - Fires on the first substantive user prompt + assistant reply unless a `User` title exists.
    /// - Refreshes an existing `Auto` title when `title_refresh_on_replan = true` and the todo
    ///   plan changed during the turn.
    /// - Non-blocking when an executor is attached: runs on a background task with a hard timeout
    ///   and emits [`AgentEvent::SessionTitle`] on completion so slow or broken title models never
    ///   stall turn completion. Falls back inline when no executor is attached (unit tests).
    /// - Revision-guarded: discards results if a concurrent `/rename` or store change occurred.
    /// - Best-effort: provider or store failures never fail the user's completed turn.
    pub(crate) async fn maybe_auto_title(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        final_text: &str,
        todo_replanned: bool,
        io: &TurnIo,
    ) {
        if io.cancel.is_cancelled() || !self.config.auto_title || self.config.subagent {
            return;
        }
        // Check live store state first so an external/user rename blocks any LLM call.
        let Ok(Some(fresh_before)) = self.store.get_session(&meta.id) else {
            return;
        };
        if fresh_before.title_source == Some(TitleSource::User) {
            meta.title = fresh_before.title;
            meta.title_source = fresh_before.title_source;
            return;
        }
        let is_refresh = fresh_before.title_source == Some(TitleSource::Auto)
            && self.config.title_refresh_on_replan
            && todo_replanned;
        let is_initial = fresh_before.title_source.is_none();
        if !is_initial && !is_refresh {
            return;
        }

        let Ok(branch) = self.load_head_branch(meta) else {
            return;
        };
        let (prompt, used_tools) = extract_turn_prompt(&branch, turn_id);
        if is_initial && !is_substantive_turn(&prompt, final_text, used_tools) {
            return;
        }

        let todos = if is_refresh {
            latest_todo_board(&branch)
        } else {
            None
        };
        let digest = build_title_digest(&prompt, final_text, todos.as_deref());
        let job = AutoTitleJob {
            meta: meta.clone(),
            digest,
            start_rev: self.title_revision(&meta.id),
            start_title: fresh_before.title,
            start_source: fresh_before.title_source,
        };

        let slot = Arc::new(Mutex::new(Some(job)));
        let bg_slot = Arc::clone(&slot);
        let bg_runtime = self.clone();
        let bg_events = Arc::clone(&io.events);
        if self
            .agents
            .spawn(Box::pin(async move {
                let job = bg_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(job) = job {
                    bg_runtime.run_auto_title_job(job, &bg_events, None).await;
                }
            }))
            .is_ok()
        {
            return;
        }

        // Synchronous fallback when no background executor is attached (e.g. unit tests).
        let job = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(job) = job {
            self.run_auto_title_job(job, &io.events, Some(meta)).await;
        }
    }

    async fn run_auto_title_job(
        &self,
        job: AutoTitleJob,
        events: &Arc<dyn EventSink>,
        sync_meta: Option<&mut SessionMeta>,
    ) {
        let title_cancel = CancelFlag::new();
        self.agents
            .cancel_after(title_cancel.clone(), AUTO_TITLE_TIMEOUT);

        let Some(generated) = self
            .request_title(&job.meta, &job.digest, &title_cancel)
            .await
        else {
            return;
        };

        // Revision guard + store double-check before committing.
        if self.title_revision(&job.meta.id) != job.start_rev {
            return;
        }
        let Ok(Some(fresh_after)) = self.store.get_session(&job.meta.id) else {
            return;
        };
        if fresh_after.title_source == Some(TitleSource::User)
            || fresh_after.title != job.start_title
            || fresh_after.title_source != job.start_source
        {
            return;
        }
        self.bump_title_revision(&job.meta.id);
        if self
            .store
            .rename_session(&job.meta.id, &generated, TitleSource::Auto)
            .is_ok()
        {
            if let Some(meta) = sync_meta {
                meta.title.clone_from(&generated);
                meta.title_source = Some(TitleSource::Auto);
            }
            emit(
                events,
                AgentEvent::SessionTitle {
                    session_id: job.meta.id,
                    title: generated,
                },
            )
            .await;
        }
    }

    /// Bare `/rename` (no argument): regenerate a title for `session_id` via the title model chain
    /// and persist it with [`TitleSource::User`].
    ///
    /// Returns `Ok(Some(title))` when updated, or `Ok(None)` when the model returned `<title/>` /
    /// `none` (or a concurrent rename bumped the revision).
    pub async fn regenerate_title(
        &self,
        session_id: &str,
        cancel: &CancelFlag,
    ) -> Result<Option<String>> {
        let meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let branch = self.load_head_branch(&meta)?;
        let (prompt, reply) = extract_session_pair(&branch);
        if prompt.trim().is_empty() && reply.trim().is_empty() {
            return Err(HrnError::Store(
                "cannot regenerate title for an empty session".to_owned(),
            ));
        }
        let todos = latest_todo_board(&branch);
        let digest = build_title_digest(&prompt, &reply, todos.as_deref());
        let start_rev = self.title_revision(session_id);
        self.agents
            .cancel_after(cancel.clone(), REGEN_TITLE_TIMEOUT);
        let Some(generated) = self.request_title(&meta, &digest, cancel).await else {
            return Ok(None);
        };
        if self.title_revision(session_id) != start_rev {
            return Ok(None);
        }
        self.rename_session(session_id, &generated, TitleSource::User)?;
        Ok(Some(generated))
    }

    /// Walk the role chain `title -> tiny -> smol -> session model` and parse `<title>...</title>`.
    async fn request_title(
        &self,
        meta: &SessionMeta,
        digest: &str,
        cancel: &CancelFlag,
    ) -> Option<String> {
        let mut candidates: Vec<Option<String>> = vec![
            Some("title".to_owned()),
            Some("tiny".to_owned()),
            Some("smol".to_owned()),
        ];
        if !candidates.contains(&meta.model) {
            candidates.push(meta.model.clone());
        }

        let mut tried_title_role = false;
        for (idx, candidate) in candidates.into_iter().enumerate() {
            if cancel.is_cancelled() {
                return None;
            }
            let is_title_role = idx < 3;
            if is_title_role && tried_title_role {
                continue;
            }
            let req = LlmRequest {
                model: candidate.clone(),
                messages: vec![Msg::system(TITLE_PROMPT), Msg::user(digest.to_owned())],
                tools: Vec::new(),
                thinking: ThinkingLevel::Off,
                thinking_budgets: None,
                temperature: None,
                max_tokens: Some(TITLE_MAX_TOKENS),
                session_id: None,
                service_tier: None,
            };
            match self.llm.stream_chat(&req, cancel, &NullSink).await {
                Ok(resp) => {
                    self.record_usage(&meta.id, candidate.as_deref(), &resp)
                        .ok();
                    return parse_title_response(&resp.content);
                }
                Err(HrnError::Config(_)) => {
                    // Role alias not configured in this runtime: try the next role in the chain.
                }
                Err(_) => {
                    // Provider/network/timeout error already ran the router's fallback chain for
                    // `title -> tiny -> smol`; skip redundant retries of those aliased roles and
                    // fall back only to the session's own model if distinct.
                    if is_title_role {
                        tried_title_role = true;
                    }
                }
            }
        }
        None
    }
}

fn extract_turn_prompt(branch: &[DagNode], turn_id: &str) -> (String, bool) {
    let mut prompt = String::new();
    let mut used_tools = false;
    for node in branch {
        let DagPayload::Message {
            turn_id: node_turn,
            msg,
        } = &node.payload
        else {
            continue;
        };
        if node_turn != turn_id {
            continue;
        }
        match msg.role {
            Role::User if prompt.is_empty() && !msg.content.starts_with("[steer]") => {
                prompt.clone_from(&msg.content);
            }
            Role::Assistant if !msg.tool_calls.is_empty() => used_tools = true,
            Role::Tool => used_tools = true,
            _ => {}
        }
    }
    (prompt, used_tools)
}

fn extract_session_pair(branch: &[DagNode]) -> (String, String) {
    let mut first_user = String::new();
    let mut last_assistant = String::new();
    for node in branch {
        if let DagPayload::Message { msg, .. } = &node.payload {
            match msg.role {
                Role::User if first_user.is_empty() && !msg.content.starts_with("[steer]") => {
                    first_user.clone_from(&msg.content);
                }
                Role::Assistant if !msg.content.trim().is_empty() => {
                    last_assistant.clone_from(&msg.content);
                }
                _ => {}
            }
        }
    }
    (first_user, last_assistant)
}

fn build_title_digest(prompt: &str, reply: &str, todos: Option<&[TodoPhase]>) -> String {
    let mut out = String::new();
    if !prompt.trim().is_empty() {
        out.push_str("## User request\n");
        out.push_str(&truncate_chars(prompt.trim(), DIGEST_SNIPPET_CHARS));
        out.push_str("\n\n");
    }
    if !reply.trim().is_empty() {
        out.push_str("## Assistant reply\n");
        out.push_str(&truncate_chars(reply.trim(), DIGEST_SNIPPET_CHARS));
        out.push_str("\n\n");
    }
    if let Some(phases) = todos
        && !phases.is_empty()
    {
        out.push_str("## Updated plan\n");
        out.push_str(&truncate_chars(
            &hrn_kernel::todo::to_markdown(phases),
            DIGEST_SNIPPET_CHARS,
        ));
    }
    out
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}...", text.get(..cut).unwrap_or(text)),
        None => text.to_owned(),
    }
}
