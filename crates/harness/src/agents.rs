//! The registry of background agents of one process: slot reservation, TTL parking, killing,
//! revival (`agent_revive`), and inbox delivery.

use crate::artifacts::{ArtifactStore, allocate_id};
use crate::control::SessionControl;
use crate::runtime::AgentRuntime;
use crate::task_config::TaskConfig;
use hrn_kernel::{BoxFuture, CancelFlag, Clock, Expectation, HrnError, Inbox, Result, SystemClock};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Longest agent name kept in metadata.
const NAME_MAX: usize = 32;

/// Starts a future on an executor. The harness itself has no async runtime; the binary supplies
/// one.
pub trait Spawn: Send + Sync {
    /// Run `task` in the background.
    ///
    /// # Errors
    /// No executor is available.
    fn spawn(&self, task: BoxFuture<()>) -> Result;

    /// Cancel `flag` after `delay` on the underlying async runtime, if supported.
    ///
    /// Runtimes without an executor (such as synchronous unit tests) leave this as a no-op.
    fn cancel_after(&self, _flag: CancelFlag, _delay: std::time::Duration) {}
}

struct NoSpawn;

impl Spawn for NoSpawn {
    fn spawn(&self, _task: BoxFuture<()>) -> Result {
        Err(HrnError::Config(
            "background agents need an executor, and none is attached to this runtime".to_owned(),
        ))
    }
}

/// Reservation of one running generation, held by the task executing the agent. Settling it
/// records the result and delivers it to the owner; dropping it unsettled delivers a failure.
pub(crate) struct JobGuard {
    record: Arc<AgentRecord>,
    registry: Option<Arc<AgentRegistry>>,
    clock: Arc<dyn Clock>,
    expectation: Option<Expectation>,
    generation: u32,
}

impl JobGuard {
    pub(crate) fn record(&self) -> &Arc<AgentRecord> {
        &self.record
    }

    pub(crate) fn registry(&self) -> Option<&Arc<AgentRegistry>> {
        self.registry.as_ref()
    }

    /// Record the outcome, persist metadata if configured, and deliver it to the owner.
    pub(crate) fn finish(
        mut self,
        status: JobStatus,
        output: String,
        yield_json: Option<Value>,
        artifact_path: Option<PathBuf>,
    ) {
        self.settle(status, output, yield_json, artifact_path);
    }

    /// Record a startup failure that the caller reports itself: nothing is delivered.
    pub(crate) fn discard(mut self, reason: String) {
        let now = self.clock.now_ms();
        let _ = self
            .record
            .settle(self.generation, JobStatus::Failed, reason, None, None, now);
        self.expectation = None;
    }

    /// Release the inbox reservation without mutating the record (used when a revive spawn fails
    /// after state was rolled back).
    pub(crate) fn cancel_reservation(mut self) {
        self.expectation = None;
    }

    /// Suppress async inbox delivery for synchronous (`!async_mode` or `blocking: true`) jobs
    /// whose settled summary is returned directly by `SubagentTool::execute`.
    pub(crate) fn suppress_delivery(&mut self) {
        self.expectation = None;
    }

    fn settle(
        &mut self,
        status: JobStatus,
        output: String,
        yield_json: Option<Value>,
        artifact_path: Option<PathBuf>,
    ) {
        let now = self.clock.now_ms();
        let applied = self.record.settle(
            self.generation,
            status,
            output,
            yield_json,
            artifact_path,
            now,
        );
        if applied && let Some(reg) = &self.registry {
            reg.persist_meta(&self.record);
        }
        if let Some(expectation) = self.expectation.take()
            && applied
        {
            expectation.deliver(self.record.delivery());
        }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if self.expectation.is_some() {
            self.settle(
                JobStatus::Failed,
                "the agent task ended without a result".to_owned(),
                None,
                None,
            );
        }
    }
}

/// Where a spawned agent task finds its guard; a failed spawn takes it back.
pub(crate) type GuardSlot = Arc<Mutex<Option<JobGuard>>>;

/// What [`AgentRegistry::register`] needs to know.
pub(crate) struct NewAgent {
    pub name: Option<String>,
    pub profile: String,
    pub owner: String,
    pub root: String,
    pub parent: Option<String>,
    pub task: String,
    pub cancel: CancelFlag,
}

/// Background agents of a process, shared by `task`, `wait`, `agent://`, `proc://`, and UIs.
pub struct AgentRegistry {
    inbox: Arc<Inbox>,
    spawner: Arc<dyn Spawn>,
    clock: Arc<dyn Clock>,
    idle_ttl_ms: AtomicU64,
    artifacts: Mutex<Option<Arc<ArtifactStore>>>,
    host: Mutex<Option<ReviveHost>>,
    agents: Mutex<Vec<Arc<AgentRecord>>>,
}

impl AgentRegistry {
    /// A registry whose agents run on `spawner` with the system clock and default idle TTL.
    pub fn new(spawner: Arc<dyn Spawn>) -> Self {
        Self {
            inbox: Arc::default(),
            spawner,
            clock: Arc::new(SystemClock),
            idle_ttl_ms: AtomicU64::new(TaskConfig::default().agent_idle_ttl_ms),
            artifacts: Mutex::default(),
            host: Mutex::default(),
            agents: Mutex::default(),
        }
    }

    /// A registry with no executor: spawning fails with a clear error.
    pub fn detached() -> Self {
        Self::new(Arc::new(NoSpawn))
    }

    /// Replace the wall clock (deterministic TTL / lifecycle tests).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Configure the idle TTL before `Idle` agents transition to `Parked` (`0` disables parking).
    #[must_use]
    pub fn with_idle_ttl_ms(self, ttl_ms: u64) -> Self {
        self.idle_ttl_ms.store(ttl_ms, Ordering::Relaxed);
        self
    }

    /// Attach an on-disk [`ArtifactStore`] (`<data>/sessions/<root>/artifacts/`).
    #[must_use]
    pub fn with_artifacts(self, store: Arc<ArtifactStore>) -> Self {
        self.set_artifacts(store);
        self
    }

    /// Update the idle TTL in place (used by runtime wiring / hot-reload).
    pub fn set_idle_ttl_ms(&self, ttl_ms: u64) {
        self.idle_ttl_ms.store(ttl_ms, Ordering::Relaxed);
    }

    /// Configured idle TTL in milliseconds (`0` disables auto-parking).
    pub fn idle_ttl_ms(&self) -> u64 {
        self.idle_ttl_ms.load(Ordering::Relaxed)
    }

    /// Attach or replace the on-disk [`ArtifactStore`] in place.
    pub fn set_artifacts(&self, store: Arc<ArtifactStore>) {
        *self
            .artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(store);
    }

    /// Current [`ArtifactStore`], if configured.
    pub fn artifacts(&self) -> Option<Arc<ArtifactStore>> {
        self.artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Wall clock used for timestamps and TTL checks.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The inbox this registry delivers into (shared with bash jobs and the front-ends).
    pub fn inbox(&self) -> &Arc<Inbox> {
        &self.inbox
    }

    pub(crate) fn spawn(&self, task: BoxFuture<()>) -> Result {
        self.spawner.spawn(task)
    }

    pub(crate) fn cancel_after(&self, flag: CancelFlag, delay: std::time::Duration) {
        self.spawner.cancel_after(flag, delay);
    }

    pub(crate) fn list(&self) -> MutexGuard<'_, Vec<Arc<AgentRecord>>> {
        self.agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn host(&self) -> Option<ReviveHost> {
        self.host
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Capture non-cyclic runtime pieces so parked/aborted/resurrected agents can be revived.
    pub(crate) fn attach_host(&self, runtime: &AgentRuntime) {
        let mut base_tools = runtime.tools.clone();
        base_tools.remove("task");
        let host = ReviveHost {
            llm: Arc::clone(&runtime.llm),
            store: Arc::clone(&runtime.store),
            base_tools,
            fs: Arc::clone(&runtime.fs),
            clock: Arc::clone(&runtime.clock),
            config: runtime.config.clone(),
            extra_hooks: runtime.extra_hooks.clone(),
            rules_seen: Arc::clone(&runtime.rules_seen),
            todos: Arc::clone(&runtime.todos),
            grants: Arc::clone(&runtime.grants),
            isolator: runtime.isolator.clone(),
            dynamic_tools: runtime.dynamic_tools.clone(),
            snap_renderer: runtime.snap_renderer.clone(),
            speculative: Arc::clone(&runtime.speculative),
            urls: Arc::downgrade(&runtime.urls),
            procs: Arc::downgrade(&runtime.procs),
        };
        *self
            .host
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(host);
    }

    pub(crate) fn persist_meta(&self, record: &AgentRecord) {
        if let Some(store) = self.artifacts() {
            let _ = store.write_meta(&record.root, &record.to_artifact_record());
        }
    }

    /// Register a `Main` (primary) agent entry for `root_session_id`. `Main` is never parked by
    /// [`Self::sweep_idle`] and does not count against subagent concurrency.
    pub fn register_main(&self, root_session_id: &str, task: &str) -> Arc<AgentRecord> {
        let mut agents = self.list();
        if let Some(existing) = agents
            .iter()
            .find(|a| a.is_main && a.root == root_session_id)
            .cloned()
        {
            return existing;
        }
        let now = self.clock.now_ms();
        let artifact_dir = self.artifacts().map(|s| s.artifacts_dir(root_session_id));
        let record = Arc::new(AgentRecord {
            id: "main".to_owned(),
            name: "main".to_owned(),
            profile: "main".to_owned(),
            owner: root_session_id.to_owned(),
            root: root_session_id.to_owned(),
            parent: None,
            task: task.to_owned(),
            is_main: true,
            control: SessionControl::default(),
            started_ms: now,
            artifact_dir,
            state: Mutex::new(AgentState {
                status: JobStatus::Idle,
                session_id: Some(root_session_id.to_owned()),
                steps: 0,
                usage: AgentUsage::default(),
                activity: String::new(),
                output: String::new(),
                yield_json: None,
                progress_notes: Vec::new(),
                finished_ms: now,
                idle_since_ms: now,
                generation: 1,
                artifact_path: None,
                patch_path: None,
                isolated_branch: None,
                worktree: None,
                cancel: CancelFlag::new(),
                live: None,
                revive_spec: None,
                telemetry: AgentTelemetry::default(),
                focus_sink: None,
            }),
        });
        agents.push(Arc::clone(&record));
        record
    }

    /// Reserve a running slot for a new child agent. `Err(n)`: `limit` agents already run in the
    /// tree (`n` of them).
    pub(crate) fn register(
        self: &Arc<Self>,
        new: NewAgent,
        limit: usize,
    ) -> std::result::Result<JobGuard, usize> {
        let mut agents = self.list();
        let running = agents
            .iter()
            .filter(|a| !a.is_main && a.root == new.root && a.is_running())
            .count();
        if running >= limit {
            return Err(running);
        }
        let name = new.name.filter(|n| !n.trim().is_empty()).map_or_else(
            || new.profile.clone(),
            |n| n.trim().chars().take(NAME_MAX).collect(),
        );
        let artifacts = self.artifacts();
        let id = allocate_id(&name, new.parent.as_deref(), |cand| {
            agents.iter().any(|a| a.root == new.root && a.id == cand)
                || artifacts
                    .as_ref()
                    .is_some_and(|store| store.exists_id(&new.root, cand))
        });
        let artifact_dir = artifacts.as_ref().map(|s| s.artifacts_dir(&new.root));
        let now = self.clock.now_ms();
        let record = Arc::new(AgentRecord {
            id,
            name,
            profile: new.profile,
            task: new.task,
            owner: new.owner.clone(),
            root: new.root,
            parent: new.parent,
            is_main: false,
            control: SessionControl::default(),
            started_ms: now,
            artifact_dir,
            state: Mutex::new(AgentState {
                status: JobStatus::Running,
                session_id: None,
                steps: 0,
                usage: AgentUsage::default(),
                activity: String::new(),
                output: String::new(),
                yield_json: None,
                progress_notes: Vec::new(),
                finished_ms: 0,
                idle_since_ms: 0,
                generation: 1,
                artifact_path: None,
                patch_path: None,
                isolated_branch: None,
                worktree: None,
                cancel: new.cancel,
                live: None,
                revive_spec: None,
                telemetry: AgentTelemetry::default(),
                focus_sink: None,
            }),
        });
        agents.push(Arc::clone(&record));
        drop(agents);
        self.persist_meta(&record);
        Ok(JobGuard {
            record,
            registry: Some(Arc::clone(self)),
            clock: Arc::clone(&self.clock),
            expectation: Some(self.inbox.expect(&new.owner)),
            generation: 1,
        })
    }

    pub(crate) fn make_guard(
        self: &Arc<Self>,
        record: Arc<AgentRecord>,
        generation: u32,
    ) -> JobGuard {
        let expectation = self.inbox.expect(&record.owner);
        JobGuard {
            record,
            registry: Some(Arc::clone(self)),
            clock: Arc::clone(&self.clock),
            expectation: Some(expectation),
            generation,
        }
    }

    /// The agent with this exact id (prefer [`Self::get_in_tree`] when scoped to a root session).
    pub fn get(&self, id: &str) -> Option<Arc<AgentRecord>> {
        self.list().iter().find(|a| a.id == id).cloned()
    }

    /// The agent with `id` belonging to the tree rooted at `root`.
    pub fn get_in_tree(&self, root: &str, id: &str) -> Option<Arc<AgentRecord>> {
        self.list()
            .iter()
            .find(|a| a.root == root && a.id == id)
            .cloned()
    }

    /// Every agent of the tree rooted at `root`, oldest first.
    pub fn in_tree(&self, root: &str) -> Vec<Arc<AgentRecord>> {
        self.list()
            .iter()
            .filter(|a| a.root == root)
            .cloned()
            .collect()
    }

    /// The agent ID owning child session `session_id`, if any.
    pub fn agent_id_for_session(&self, session_id: &str) -> Option<String> {
        self.list()
            .iter()
            .find(|a| !a.is_main && a.state().session_id.as_deref() == Some(session_id))
            .map(|a| a.id.clone())
    }

    /// The root of the tree `session_id` belongs to (itself unless it is an agent's session).
    pub fn root_of(&self, session_id: &str) -> String {
        self.list()
            .iter()
            .find(|a| a.state().session_id.as_deref() == Some(session_id))
            .map_or_else(|| session_id.to_owned(), |a| a.root.clone())
    }

    /// Subagent nesting depth of `session_id` (`0` for root, `1` for direct child, etc.).
    pub fn session_depth(&self, session_id: &str) -> usize {
        let agents = self.list();
        let mut cur_id = agents
            .iter()
            .find(|a| !a.is_main && a.state().session_id.as_deref() == Some(session_id))
            .map(|a| a.id.clone());
        let mut depth = 0usize;
        while let Some(id) = cur_id {
            depth += 1;
            if depth >= 64 {
                break;
            }
            cur_id = agents
                .iter()
                .find(|a| !a.is_main && a.id == id)
                .and_then(|a| a.parent.clone());
        }
        depth
    }

    /// Live scoped [`AgentRuntime`] of child session `session_id`, if currently active.
    pub(crate) fn live_runtime_for_session(&self, session_id: &str) -> Option<AgentRuntime> {
        self.list()
            .iter()
            .find(|a| !a.is_main && a.state().session_id.as_deref() == Some(session_id))
            .and_then(|a| a.state().live.as_ref().map(|h| h.runtime.clone()))
    }

    /// Every agent of the registry, oldest first.
    pub(crate) fn records(&self) -> Vec<Arc<AgentRecord>> {
        self.list().clone()
    }

    /// Ask every agent of the tree rooted at `root` to stop.
    pub fn cancel_tree(&self, root: &str) {
        for agent in self.in_tree(root) {
            if !agent.is_main {
                agent.cancel();
            }
        }
    }

    /// Kill an agent: cancels any running generation, drops its live runtime, and marks it
    /// `Aborted` (tombstone). Returns an error if already `Aborted` or unknown.
    pub fn kill(&self, id: &str) -> Result {
        let agent = self
            .get(id)
            .ok_or_else(|| HrnError::Tool(format!("unknown agent `{id}`")))?;
        if agent.is_main {
            return Err(HrnError::Tool("cannot kill the main agent".to_owned()));
        }
        let now = self.clock.now_ms();
        {
            let mut s = agent.state();
            if s.status == JobStatus::Aborted {
                return Err(HrnError::Tool(format!("job `{id}` already aborted")));
            }
            s.cancel.cancel();
            s.status = JobStatus::Aborted;
            s.finished_ms = now;
            s.idle_since_ms = 0;
            s.live = None;
            s.telemetry.current_tool = None;
            s.telemetry.current_tool_args = None;
            s.telemetry.current_tool_started_ms = None;
            if s.output.is_empty() {
                "aborted".clone_into(&mut s.output);
            }
        }
        agent.note_lifecycle("aborted by user".to_owned(), now);
        if let Some(wt) = agent.take_worktree()
            && let Some(isolator) = self.host().and_then(|h| h.isolator)
        {
            let _ = self.spawn(Box::pin(async move {
                let _ = isolator.cleanup(&wt).await;
            }));
        }
        self.persist_meta(&agent);
        self.inbox.wake();
        Ok(())
    }

    /// Park every non-main `Idle` agent whose idle age has reached `agent_idle_ttl_ms`.
    /// Drops their live runtime handles while keeping records, transcripts, and artifacts.
    /// Returns the IDs of agents transitioned to [`JobStatus::Parked`].
    pub fn sweep_idle(&self) -> Vec<String> {
        let ttl = i64::try_from(self.idle_ttl_ms()).unwrap_or(i64::MAX);
        if ttl <= 0 {
            return Vec::new();
        }
        let now = self.clock.now_ms();
        let records = self.records();
        let mut parked = Vec::new();
        for agent in records {
            if agent.is_main {
                continue;
            }
            let transitioned = {
                let mut s = agent.state();
                if s.status == JobStatus::Idle
                    && s.idle_since_ms > 0
                    && now.saturating_sub(s.idle_since_ms) >= ttl
                {
                    s.status = JobStatus::Parked;
                    s.live = None;
                    true
                } else {
                    false
                }
            };
            if transitioned {
                self.persist_meta(&agent);
                parked.push(agent.id.clone());
            }
        }
        parked
    }
}

#[cfg(test)]
#[path = "agents_tests.rs"]
mod tests;

// ===== merged from agent_activity.rs =====
// Structured per-agent activity log events and live inspector telemetry for Agent Hub.

use hrn_kernel::{EventSink, Usage};
use serde::{Deserialize, Serialize};

/// Maximum number of recent activity events retained per agent in memory.
pub const MAX_AGENT_EVENTS: usize = 128;

/// Category of one activity event shown in the Agent Hub Activity tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivityKind {
    /// Tool invocation started or completed cleanly.
    Tool,
    /// Assistant response text produced.
    Response,
    /// Tool failure or turn error.
    Error,
    /// Steer, follow-up, broadcast, or `?progress` inter-agent message.
    Irc,
    /// Lifecycle transition (spawned, revived, parked, aborted, retry/status).
    Lifecycle,
}

impl AgentActivityKind {
    /// Lower-case tag rendered in UI rows and filters.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Response => "response",
            Self::Error => "error",
            Self::Irc => "irc",
            Self::Lifecycle => "lifecycle",
        }
    }
}

/// One timestamped event recorded for an agent in the live session tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivityEvent {
    /// Timestamp in milliseconds since epoch.
    pub ts_ms: i64,
    /// Agent identifier (`main`, `scout`, `lead.worker`, ...).
    pub agent_id: String,
    /// Event category for Activity tab filtering.
    pub kind: AgentActivityKind,
    /// Concise single-line summary rendered in the Activity log.
    pub summary: String,
    /// Optional fuller detail (tool arguments, error body, progress text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Owning session id for transcript jump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional DAG node id for transcript jump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

/// Live inspector telemetry held alongside [`crate::agents::AgentState`].
#[derive(Clone, Debug, Default)]
pub(crate) struct AgentTelemetry {
    /// Resolved model serving the agent's latest LLM call.
    pub model: Option<String>,
    /// Name of the tool currently executing (`None` between tool calls).
    pub current_tool: Option<String>,
    /// Compact preview of the active tool's arguments.
    pub current_tool_args: Option<String>,
    /// Start timestamp (ms since epoch) of the active tool call.
    pub current_tool_started_ms: Option<i64>,
    /// Latest intent/thought/progress line for the inspector.
    pub last_intent: Option<String>,
    /// Active retry or fallback status message, if any.
    pub retry_state: Option<String>,
    /// Latest prompt token count reported by the provider (live context window fill).
    pub context_tokens: u64,
    /// Unread inter-agent / response notifications since the user last focused this agent.
    pub unread: usize,
    /// Bounded chronological log of recent activity events.
    pub events: Vec<AgentActivityEvent>,
}

/// Collapse whitespace and truncate `text` to at most `max_chars` Unicode scalar values.
pub(crate) fn clip_one_line(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut prev_ws = true;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_ws && count < max_chars {
                out.push(' ');
                count += 1;
            }
            prev_ws = true;
            continue;
        }
        prev_ws = false;
        if count >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
        count += 1;
    }
    out.trim().to_owned()
}

/// Append `ev` to `events`, dropping the oldest entry when exceeding [`MAX_AGENT_EVENTS`].
pub(crate) fn push_bounded(events: &mut Vec<AgentActivityEvent>, ev: AgentActivityEvent) {
    if events.len() >= MAX_AGENT_EVENTS {
        events.remove(0);
    }
    events.push(ev);
}

impl AgentRecord {
    /// Attach or detach a live event sink while a front-end focuses this agent's session.
    pub fn set_focus_sink(&self, sink: Option<Arc<dyn EventSink>>) {
        self.state().focus_sink = sink;
    }

    pub(crate) fn focus_sink(&self) -> Option<Arc<dyn EventSink>> {
        self.state().focus_sink.clone()
    }

    /// Reset this agent's unread counter (called when the user focuses it in Agent Hub).
    pub fn mark_read(&self) {
        self.state().telemetry.unread = 0;
    }

    /// Record one LLM call and its optional token/cost usage. Returns total requests so far.
    #[expect(
        clippy::cast_precision_loss,
        reason = "cost in microdollars stays far below 2^52"
    )]
    pub(crate) fn note_llm_call(&self, model: &str, usage: Option<Usage>) -> u64 {
        let mut s = self.state();
        s.steps += 1;
        s.usage.requests = s.usage.requests.saturating_add(1);
        if !model.trim().is_empty() {
            s.telemetry.model = Some(model.to_owned());
        }
        s.telemetry.retry_state = None;
        if let Some(u) = usage {
            s.usage.input_tokens = s.usage.input_tokens.saturating_add(u.prompt_tokens);
            s.usage.output_tokens = s.usage.output_tokens.saturating_add(u.completion_tokens);
            if u.prompt_tokens > 0 {
                s.telemetry.context_tokens = u.prompt_tokens;
            }
            if let Some(micros) = u.cost_micros {
                s.usage.cost_usd += (micros as f64) / 1_000_000.0;
            }
        }
        s.usage.requests
    }

    pub(crate) fn note_tool_start(&self, name: &str, args: &str, now_ms: i64) {
        let mut s = self.state();
        s.usage.tool_calls = s.usage.tool_calls.saturating_add(1);
        s.activity = format!("running `{name}`");
        let clipped_args = clip_one_line(args, 160);
        s.telemetry.current_tool = Some(name.to_owned());
        s.telemetry.current_tool_args = (!clipped_args.is_empty()).then_some(clipped_args.clone());
        s.telemetry.current_tool_started_ms = Some(now_ms);
        let session_id = s.session_id.clone();
        push_bounded(
            &mut s.telemetry.events,
            AgentActivityEvent {
                ts_ms: now_ms,
                agent_id: self.id.clone(),
                kind: AgentActivityKind::Tool,
                summary: format!("tool `{name}`"),
                detail: (!clipped_args.is_empty()).then_some(clipped_args),
                session_id,
                node_id: None,
            },
        );
    }

    pub(crate) fn note_tool_done(&self, name: &str, output: &str, is_error: bool, now_ms: i64) {
        let mut s = self.state();
        if s.telemetry.current_tool.as_deref() == Some(name) {
            s.telemetry.current_tool = None;
            s.telemetry.current_tool_args = None;
            s.telemetry.current_tool_started_ms = None;
        }
        let session_id = s.session_id.clone();
        let detail = clip_one_line(output, 160);
        let (kind, summary) = if is_error {
            if s.focus_sink.is_none() {
                s.telemetry.unread = s.telemetry.unread.saturating_add(1);
            }
            (AgentActivityKind::Error, format!("tool `{name}` failed"))
        } else {
            (AgentActivityKind::Tool, format!("tool `{name}` ok"))
        };
        push_bounded(
            &mut s.telemetry.events,
            AgentActivityEvent {
                ts_ms: now_ms,
                agent_id: self.id.clone(),
                kind,
                summary,
                detail: (!detail.is_empty()).then_some(detail),
                session_id,
                node_id: None,
            },
        );
    }

    pub(crate) fn note_response(&self, text: &str, now_ms: i64) {
        let summary = clip_one_line(text, 140);
        if summary.is_empty() {
            return;
        }
        let mut s = self.state();
        s.telemetry.last_intent = Some(summary.clone());
        if s.focus_sink.is_none() {
            s.telemetry.unread = s.telemetry.unread.saturating_add(1);
        }
        let session_id = s.session_id.clone();
        push_bounded(
            &mut s.telemetry.events,
            AgentActivityEvent {
                ts_ms: now_ms,
                agent_id: self.id.clone(),
                kind: AgentActivityKind::Response,
                summary,
                detail: None,
                session_id,
                node_id: None,
            },
        );
    }

    pub(crate) fn note_error(&self, message: &str, now_ms: i64) {
        let summary = clip_one_line(message, 160);
        let mut s = self.state();
        if s.focus_sink.is_none() {
            s.telemetry.unread = s.telemetry.unread.saturating_add(1);
        }
        let session_id = s.session_id.clone();
        push_bounded(
            &mut s.telemetry.events,
            AgentActivityEvent {
                ts_ms: now_ms,
                agent_id: self.id.clone(),
                kind: AgentActivityKind::Error,
                summary,
                detail: None,
                session_id,
                node_id: None,
            },
        );
    }

    pub(crate) fn note_status(&self, message: &str, now_ms: i64) {
        let summary = clip_one_line(message, 140);
        if summary.is_empty() {
            return;
        }
        let mut s = self.state();
        let low = summary.to_ascii_lowercase();
        if low.contains("retry")
            || low.contains("backoff")
            || low.contains("fallback")
            || low.contains("rate")
        {
            s.telemetry.retry_state = Some(summary.clone());
        } else {
            s.telemetry.last_intent = Some(summary.clone());
        }
        let session_id = s.session_id.clone();
        push_bounded(
            &mut s.telemetry.events,
            AgentActivityEvent {
                ts_ms: now_ms,
                agent_id: self.id.clone(),
                kind: AgentActivityKind::Lifecycle,
                summary,
                detail: None,
                session_id,
                node_id: None,
            },
        );
    }

    /// Record an inter-agent steer, follow-up, or progress message on this agent's activity log.
    pub fn note_irc(&self, text: &str, now_ms: i64) {
        let summary = clip_one_line(text, 140);
        if summary.is_empty() {
            return;
        }
        let mut s = self.state();
        if s.focus_sink.is_none() {
            s.telemetry.unread = s.telemetry.unread.saturating_add(1);
        }
        let session_id = s.session_id.clone();
        push_bounded(
            &mut s.telemetry.events,
            AgentActivityEvent {
                ts_ms: now_ms,
                agent_id: self.id.clone(),
                kind: AgentActivityKind::Irc,
                summary,
                detail: None,
                session_id,
                node_id: None,
            },
        );
    }

    pub(crate) fn note_lifecycle(&self, summary: String, now_ms: i64) {
        let mut s = self.state();
        let session_id = s.session_id.clone();
        push_bounded(
            &mut s.telemetry.events,
            AgentActivityEvent {
                ts_ms: now_ms,
                agent_id: self.id.clone(),
                kind: AgentActivityKind::Lifecycle,
                summary,
                detail: None,
                session_id,
                node_id: None,
            },
        );
    }
}

// ===== merged from agent_record.rs =====
// Agent lifecycle state, records, snapshots, usage counters, and inbox delivery formatting.

use crate::artifacts::ArtifactRecord;
use crate::isolation::{DynamicToolResolver, IsolatedWorktree, WorktreeIsolator};
use crate::profiles::SubagentProfile;
use crate::registry::ToolRegistry;
use crate::runtime::HarnessConfig;
use crate::scope::ScopePolicy;
use hrn_kernel::{
    ApprovalGate, HarnessHook, InboxItem, LlmProvider, ProcTable, SessionMeta, SessionStore,
    UrlRouter, WorkspaceFs,
};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Weak;

/// Longest agent output carried whole in an inbox delivery (Unicode scalar values).
pub const DELIVERY_FULL_CHARS: usize = 11_500;
/// Preview length carried in an inbox delivery when output exceeds [`DELIVERY_FULL_CHARS`].
pub const DELIVERY_PREVIEW_CHARS: usize = 5_000;

/// Where an agent stands in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Actively executing a turn.
    Running,
    /// Finished its latest turn; runtime and session remain alive for follow-up messages.
    Idle,
    /// Idle TTL elapsed (or resurrected from disk); runtime dropped, transcript & artifact kept.
    Parked,
    /// Stopped by kill/cancel (tombstone); revivable as a new generation.
    Aborted,
    /// Stopped by an error.
    Failed,
}

impl JobStatus {
    /// Lower-case status label, as shown to the model and UI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Parked => "parked",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }

    /// Whether a follow-up message may revive an agent in this state.
    pub const fn can_revive(self) -> bool {
        matches!(self, Self::Idle | Self::Parked | Self::Aborted)
    }
}

/// Accumulated LLM and tool usage counters for one agent across all generations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentUsage {
    /// Total cost in US dollars (`0.0` when unpriced).
    pub cost_usd: f64,
    /// Total prompt/input tokens.
    pub input_tokens: u64,
    /// Total completion/output tokens.
    pub output_tokens: u64,
    /// Total LLM requests issued.
    pub requests: u64,
    /// Total tool calls executed.
    pub tool_calls: u64,
}

/// Live runtime and session retained while an agent is `Running` or `Idle`, dropped on `Parked`
/// or `Aborted`.
#[derive(Clone)]
pub(crate) struct LiveHandle {
    pub runtime: AgentRuntime,
    pub session: SessionMeta,
    pub parent_model: Option<String>,
    pub model_from_profile: bool,
    pub approval: Option<Arc<dyn ApprovalGate>>,
}

/// Non-cyclic recipe for reconstructing a child runtime when reviving from `Parked` or `Aborted`.
#[derive(Clone)]
pub(crate) struct ReviveSpec {
    pub profile: SubagentProfile,
    pub scope: ScopePolicy,
    pub depth: usize,
    pub parent_model: Option<String>,
    pub model_from_profile: bool,
    pub approval: Option<Arc<dyn ApprovalGate>>,
    pub cwd: PathBuf,
}

/// Non-cyclic runtime pieces captured from the host so parked/aborted/resurrected agents can be
/// rebuilt without creating an `Arc` cycle back to [`crate::AgentRegistry`].
#[derive(Clone)]
pub(crate) struct ReviveHost {
    pub llm: Arc<dyn LlmProvider>,
    pub store: Arc<dyn SessionStore>,
    pub base_tools: ToolRegistry,
    pub fs: Arc<dyn WorkspaceFs>,
    pub clock: Arc<dyn Clock>,
    pub config: HarnessConfig,
    pub extra_hooks: Vec<Arc<dyn HarnessHook>>,
    pub rules_seen: Arc<Mutex<HashSet<(String, String)>>>,
    pub todos: Arc<hrn_kernel::todo::TodoBoards>,
    pub grants: Arc<crate::approvals::GrantCache>,
    pub isolator: Option<Arc<dyn WorktreeIsolator>>,
    pub dynamic_tools: Option<Arc<dyn DynamicToolResolver>>,
    pub snap_renderer: Option<Arc<dyn hrn_kernel::SnapRenderer>>,
    pub speculative: crate::compaction::SpeculativeMap,
    pub urls: Weak<UrlRouter>,
    pub procs: Weak<ProcTable>,
}

pub(crate) struct AgentState {
    pub status: JobStatus,
    pub session_id: Option<String>,
    pub steps: usize,
    pub usage: AgentUsage,
    pub activity: String,
    pub output: String,
    pub yield_json: Option<Value>,
    pub progress_notes: Vec<String>,
    pub finished_ms: i64,
    pub idle_since_ms: i64,
    pub generation: u32,
    pub artifact_path: Option<PathBuf>,
    pub patch_path: Option<PathBuf>,
    pub isolated_branch: Option<String>,
    pub worktree: Option<IsolatedWorktree>,
    pub cancel: CancelFlag,
    pub live: Option<LiveHandle>,
    pub revive_spec: Option<ReviveSpec>,
    pub telemetry: AgentTelemetry,
    pub focus_sink: Option<Arc<dyn EventSink>>,
}

/// What a reader sees of an agent at one moment.
#[derive(Clone, Debug)]
pub struct AgentSnapshot {
    /// Current lifecycle status.
    pub status: JobStatus,
    /// Parent agent id if spawned by another subagent (`None` for top-level agents).
    pub parent: Option<String>,
    /// The agent's own session (`history://<session>`), once created.
    pub session_id: Option<String>,
    /// LLM steps taken across generations.
    pub steps: usize,
    /// Total cost in US dollars.
    pub cost_usd: f64,
    /// Total input/prompt tokens.
    pub input_tokens: u64,
    /// Total output/completion tokens.
    pub output_tokens: u64,
    /// Total LLM requests issued.
    pub requests: u64,
    /// Total tool calls executed.
    pub tool_calls: u64,
    /// What it is doing now (last tool), empty before the first step.
    pub activity: String,
    /// Final output, or error; empty while running its first turn.
    pub output: String,
    /// Structured JSON yield value, when produced by `yield`.
    pub yield_json: Option<Value>,
    /// Progress notes written via `agent://<id>?progress`.
    pub progress_notes: Vec<String>,
    /// When it last settled (ms since epoch); `0` while running.
    pub finished_ms: i64,
    /// When it entered `Idle` (ms since epoch); `0` when not idle.
    pub idle_since_ms: i64,
    /// Generation number (starts at `1`; incremented on every revive).
    pub generation: u32,
    /// Directory containing this tree's artifacts (`<data>/sessions/<root>/artifacts`), if configured.
    pub artifact_dir: Option<PathBuf>,
    /// Path to this agent's `<id>.md` artifact file, once written.
    pub artifact_path: Option<PathBuf>,
    /// Path to this agent's `<id>.patch` worktree diff, once captured.
    pub patch_path: Option<PathBuf>,
    /// Committed branch name (`hrn/task/<id>`) when `isolation.merge == "branch"`.
    pub isolated_branch: Option<String>,
    /// Resolved model serving this agent's latest call, if known.
    pub model: Option<String>,
    /// Currently executing tool name, if any.
    pub current_tool: Option<String>,
    /// Compact preview of the active tool's arguments, if any.
    pub current_tool_args: Option<String>,
    /// Start timestamp (ms since epoch) of the active tool call, if running.
    pub current_tool_started_ms: Option<i64>,
    /// Latest assistant/reasoning/progress intent summary.
    pub last_intent: Option<String>,
    /// Active retry or fallback status message, if any.
    pub retry_state: Option<String>,
    /// Prompt tokens of the latest LLM request (live context usage).
    pub context_tokens: u64,
    /// Unread response/IRC/status count since the user last focused this agent.
    pub unread: usize,
    /// Bounded recent activity events for this agent.
    pub events: Vec<AgentActivityEvent>,
}

/// One background (or main) agent tracked by [`crate::AgentRegistry`].
pub struct AgentRecord {
    /// Unique handle in the registry (slug + uniquify; `<parent>.<child>` for nested children).
    pub id: String,
    /// Caller-chosen or profile name.
    pub name: String,
    /// Profile it runs.
    pub profile: String,
    /// Session that spawned it; its results are delivered there.
    pub owner: String,
    /// Root session of the tree it belongs to.
    pub root: String,
    /// Parent agent id if nested under another subagent (`None` for children of Main).
    pub parent: Option<String>,
    /// Start of the task text, for listings.
    pub task: String,
    /// Whether this record represents the primary (`Main`) agent, which is never parked.
    pub is_main: bool,
    /// Live handle: messages written to a running agent steer it here.
    pub control: SessionControl,
    /// When it started (ms since epoch).
    pub started_ms: i64,
    /// Directory containing this tree's artifacts, if configured.
    pub artifact_dir: Option<PathBuf>,
    pub(crate) state: Mutex<AgentState>,
}

impl AgentRecord {
    pub(crate) fn state(&self) -> MutexGuard<'_, AgentState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Current snapshot of mutable state.
    pub fn snapshot(&self) -> AgentSnapshot {
        let s = self.state();
        AgentSnapshot {
            status: s.status,
            parent: self.parent.clone(),
            session_id: s.session_id.clone(),
            steps: s.steps,
            cost_usd: s.usage.cost_usd,
            input_tokens: s.usage.input_tokens,
            output_tokens: s.usage.output_tokens,
            requests: s.usage.requests,
            tool_calls: s.usage.tool_calls,
            activity: s.activity.clone(),
            output: s.output.clone(),
            yield_json: s.yield_json.clone(),
            progress_notes: s.progress_notes.clone(),
            finished_ms: s.finished_ms,
            idle_since_ms: s.idle_since_ms,
            generation: s.generation,
            artifact_dir: self.artifact_dir.clone(),
            artifact_path: s.artifact_path.clone(),
            patch_path: s.patch_path.clone(),
            isolated_branch: s.isolated_branch.clone(),
            model: s.telemetry.model.clone(),
            current_tool: s.telemetry.current_tool.clone(),
            current_tool_args: s.telemetry.current_tool_args.clone(),
            current_tool_started_ms: s.telemetry.current_tool_started_ms,
            last_intent: s.telemetry.last_intent.clone(),
            retry_state: s.telemetry.retry_state.clone(),
            context_tokens: s.telemetry.context_tokens,
            unread: s.telemetry.unread,
            events: s.telemetry.events.clone(),
        }
    }

    /// Whether it is currently executing a turn.
    pub fn is_running(&self) -> bool {
        self.state().status == JobStatus::Running
    }

    /// Whether its live runtime handle is currently resident in memory (`false` once parked/aborted).
    pub fn has_live_runtime(&self) -> bool {
        self.state().live.is_some()
    }

    /// The cancellation flag for the currently active generation.
    pub fn cancel_flag(&self) -> CancelFlag {
        self.state().cancel.clone()
    }

    /// Signal cooperative cancellation on the currently active generation.
    pub fn cancel(&self) {
        self.state().cancel.cancel();
    }

    pub(crate) fn set_session(&self, id: &str) {
        self.state().session_id = Some(id.to_owned());
    }

    pub(crate) fn set_live(&self, live: LiveHandle, spec: Option<ReviveSpec>) {
        let mut s = self.state();
        s.live = Some(live);
        if spec.is_some() {
            s.revive_spec = spec;
        }
    }

    pub(crate) fn set_worktree(&self, wt: IsolatedWorktree) {
        self.state().worktree = Some(wt);
    }

    pub(crate) fn take_worktree(&self) -> Option<IsolatedWorktree> {
        self.state().worktree.take()
    }

    pub(crate) fn set_isolation_result(&self, patch_path: Option<PathBuf>, branch: Option<String>) {
        let mut s = self.state();
        if patch_path.is_some() {
            s.patch_path = patch_path;
        }
        if branch.is_some() {
            s.isolated_branch = branch;
        }
    }

    pub(crate) fn add_progress_note(&self, note: String) {
        let mut s = self.state();
        if s.status == JobStatus::Running {
            s.activity.clone_from(&note);
        }
        let clipped = clip_one_line(&note, 140);
        if !clipped.is_empty() {
            s.telemetry.last_intent = Some(clipped);
        }
        if s.focus_sink.is_none() {
            s.telemetry.unread = s.telemetry.unread.saturating_add(1);
        }
        s.progress_notes.push(note);
    }

    pub(crate) fn to_artifact_record(&self) -> ArtifactRecord {
        let s = self.state();
        ArtifactRecord {
            id: self.id.clone(),
            name: self.name.clone(),
            profile: self.profile.clone(),
            owner: self.owner.clone(),
            root: self.root.clone(),
            parent: self.parent.clone(),
            task: self.task.clone(),
            session_id: s.session_id.clone(),
            status: s.status,
            started_ms: self.started_ms,
            finished_ms: s.finished_ms,
            generation: s.generation,
            usage: s.usage,
            progress_notes: s.progress_notes.clone(),
            patch_path: s.patch_path.clone(),
            isolated_branch: s.isolated_branch.clone(),
        }
    }

    pub(crate) fn settle(
        &self,
        expected_gen: u32,
        status: JobStatus,
        output: String,
        yield_json: Option<Value>,
        artifact_path: Option<PathBuf>,
        now_ms: i64,
    ) -> bool {
        let mut s = self.state();
        if s.generation != expected_gen {
            return false;
        }
        s.status = status;
        s.output = output;
        if yield_json.is_some() {
            s.yield_json = yield_json;
        }
        if artifact_path.is_some() {
            s.artifact_path = artifact_path;
        }
        s.finished_ms = now_ms;
        s.telemetry.current_tool = None;
        s.telemetry.current_tool_args = None;
        s.telemetry.current_tool_started_ms = None;
        s.telemetry.retry_state = None;
        if s.focus_sink.is_none() {
            s.telemetry.unread = s.telemetry.unread.saturating_add(1);
        }
        let session_id = s.session_id.clone();
        push_bounded(
            &mut s.telemetry.events,
            AgentActivityEvent {
                ts_ms: now_ms,
                agent_id: self.id.clone(),
                kind: AgentActivityKind::Lifecycle,
                summary: format!("settled ({})", status.as_str()),
                detail: None,
                session_id,
                node_id: None,
            },
        );
        if status == JobStatus::Idle {
            s.idle_since_ms = now_ms;
        } else {
            s.idle_since_ms = 0;
            s.live = None;
        }
        true
    }

    /// Format the settled summary text returned synchronously (`!async_mode` or `blocking: true`)
    /// or wrapped inside an async inbox delivery.
    pub(crate) fn format_summary(&self) -> String {
        let s = self.snapshot();
        let status = s.status.as_str();
        let mut text = format!(
            "<agent id=\"{}\" name=\"{}\" profile=\"{}\" status=\"{status}\" steps=\"{}\" generation=\"{}\">\n",
            self.id, self.name, self.profile, s.steps, s.generation
        );
        if s.output.chars().count() <= DELIVERY_FULL_CHARS {
            text.push_str(&s.output);
        } else {
            let cut = s
                .output
                .char_indices()
                .nth(DELIVERY_PREVIEW_CHARS)
                .map_or(s.output.len(), |(idx, _)| idx);
            text.push_str(s.output.get(..cut).unwrap_or_default());
            let _ = write!(
                text,
                "\n… (output truncated to {DELIVERY_PREVIEW_CHARS} chars; full output at agent://{})",
                self.id
            );
        }
        text.push_str("\n</agent>");
        text
    }

    /// Build the inbox delivery for this (settled) agent.
    /// Carries the full output if `<= 11_500` chars, otherwise a `5_000`-char preview + `agent://<id>`.
    pub(crate) fn delivery(&self) -> InboxItem {
        let status = self.snapshot().status.as_str();
        InboxItem {
            source: self.id.clone(),
            text: self.format_summary(),
            summary: format!("{} {status}", self.id),
        }
    }
}

// ===== merged from agent_job.rs =====
// The task that runs one background (or synchronous) agent turn to completion, drives sequential
// `steps[]` chains, enforces structured `outputSchema` via hidden `yield`, tracks request/runtime
// budgets, finalizes worktree isolation, writes artifacts (`<id>.md`, `<id>.json`, `<id>.patch`),
// and settles its registry record to `Idle`.
use crate::runtime::{TurnIo, TurnResult};
use crate::subagent::SchemaMode;
use crate::task_config::TaskIsolationConfig;
use hrn_kernel::AgentEvent;
use std::sync::atomic::AtomicBool;

/// Active worktree isolation handle and policy for one isolated subagent job.
pub(crate) struct JobIsolation {
    pub isolator: Arc<dyn WorktreeIsolator>,
    pub worktree: IsolatedWorktree,
    pub allowed_paths: Vec<String>,
    pub denied_paths: Vec<String>,
    pub config: TaskIsolationConfig,
}

/// Everything an agent needs to run, prepared by `SubagentTool` or `AgentRegistry::revive`.
pub(crate) struct Job {
    /// The child runtime (filtered tools, scope, advisor settings).
    pub runtime: AgentRuntime,
    pub session: SessionMeta,
    /// The spawning session's model, used when the profile's role alias is not configured.
    pub parent_model: Option<String>,
    pub model_from_profile: bool,
    pub prompt: String,
    /// Subsequent prompts for multi-step `steps[]` chains (executed sequentially on the same session).
    pub extra_steps: Vec<String>,
    /// Optional JSON Schema enforced on the child's hidden `yield` result.
    pub output_schema: Option<Value>,
    /// How schema validation mismatches are handled once reminders are exhausted.
    pub schema_mode: SchemaMode,
    /// Active git worktree isolation for this job, if `isolated: true`.
    pub isolation: Option<JobIsolation>,
    pub approval: Option<Arc<dyn ApprovalGate>>,
    pub slot: GuardSlot,
}

/// Records what the agent does (steps, usage, tool calls) and enforces `[task]` soft/hard request
/// budgets and `max_runtime_ms`.
struct ProgressSink {
    record: Arc<AgentRecord>,
    cancel: CancelFlag,
    clock: Arc<dyn Clock>,
    started_ms: i64,
    soft_budget: usize,
    hard_budget: usize,
    max_runtime_ms: u64,
    soft_warned: Arc<AtomicBool>,
    budget_stopped: Arc<AtomicBool>,
    runtime_stopped: Arc<AtomicBool>,
}

impl ProgressSink {
    fn check_runtime(&self) {
        if self.max_runtime_ms == 0 {
            return;
        }
        let elapsed = self.clock.now_ms().saturating_sub(self.started_ms);
        let limit = i64::try_from(self.max_runtime_ms).unwrap_or(i64::MAX);
        if elapsed >= limit {
            self.runtime_stopped.store(true, Ordering::SeqCst);
            self.cancel.cancel();
        }
    }
}

impl EventSink for ProgressSink {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        let now = self.clock.now_ms();
        match &event {
            AgentEvent::ToolStart {
                name, arguments, ..
            } => {
                self.record.note_tool_start(name, arguments, now);
                self.check_runtime();
            }
            AgentEvent::ToolDone {
                name,
                output,
                is_error,
                ..
            } => {
                self.record.note_tool_done(name, output, *is_error, now);
            }
            AgentEvent::LlmCall { model, usage, .. } => {
                let requests = self.record.note_llm_call(model, *usage);
                let req_usize = usize::try_from(requests).unwrap_or(usize::MAX);
                if self.soft_budget > 0
                    && req_usize >= self.soft_budget
                    && !self.soft_warned.swap(true, Ordering::SeqCst)
                {
                    self.record.control.push_steer(format!(
                        "[system] Soft request budget ({}) reached; wrap up and deliver your \
                         final answer now. Hard stop triggers at {} requests.",
                        self.soft_budget, self.hard_budget
                    ));
                }
                if self.hard_budget > 0 && req_usize >= self.hard_budget {
                    self.budget_stopped.store(true, Ordering::SeqCst);
                    self.cancel.cancel();
                }
                self.check_runtime();
            }
            AgentEvent::TurnDone { text, .. } => self.record.note_response(text, now),
            AgentEvent::TurnError { message, .. } => self.record.note_error(message, now),
            AgentEvent::StatusChanged {
                message: Some(msg), ..
            } => self.record.note_status(msg, now),
            AgentEvent::SteerInjected { text, .. } => self.record.note_irc(text, now),
            _ => {}
        }
        if let Some(focus) = self.record.focus_sink() {
            return Box::pin(async move {
                let _ = focus.emit(event).await;
                true
            });
        }
        Box::pin(async { true })
    }
}

pub(crate) struct BudgetTracker {
    record: Arc<AgentRecord>,
    cancel: CancelFlag,
    clock: Arc<dyn Clock>,
    started_ms: i64,
    soft_budget: usize,
    hard_budget: usize,
    max_runtime_ms: u64,
    soft_warned: Arc<AtomicBool>,
    budget_stopped: Arc<AtomicBool>,
    runtime_stopped: Arc<AtomicBool>,
    approval: Option<Arc<dyn ApprovalGate>>,
}

impl BudgetTracker {
    pub(crate) fn new(
        record: &Arc<AgentRecord>,
        runtime: &AgentRuntime,
        approval: Option<Arc<dyn ApprovalGate>>,
    ) -> Self {
        Self {
            record: Arc::clone(record),
            cancel: record.cancel_flag(),
            clock: Arc::clone(&runtime.clock),
            started_ms: runtime.clock.now_ms(),
            soft_budget: runtime.task_config().soft_request_budget,
            hard_budget: runtime.task_config().hard_request_budget(),
            max_runtime_ms: runtime.task_config().max_runtime_ms,
            soft_warned: Arc::new(AtomicBool::new(false)),
            budget_stopped: Arc::new(AtomicBool::new(false)),
            runtime_stopped: Arc::new(AtomicBool::new(false)),
            approval,
        }
    }

    pub(crate) fn make_io(&self) -> TurnIo {
        TurnIo {
            cancel: self.cancel.clone(),
            events: Arc::new(ProgressSink {
                record: Arc::clone(&self.record),
                cancel: self.cancel.clone(),
                clock: Arc::clone(&self.clock),
                started_ms: self.started_ms,
                soft_budget: self.soft_budget,
                hard_budget: self.hard_budget,
                max_runtime_ms: self.max_runtime_ms,
                soft_warned: Arc::clone(&self.soft_warned),
                budget_stopped: Arc::clone(&self.budget_stopped),
                runtime_stopped: Arc::clone(&self.runtime_stopped),
            }),
            approval: self.approval.clone(),
            ask: None,
        }
    }

    pub(crate) fn stop_reason(&self) -> StopReason {
        StopReason {
            budget_stopped: self.budget_stopped.load(Ordering::SeqCst),
            hard_budget: self.hard_budget,
            runtime_stopped: self.runtime_stopped.load(Ordering::SeqCst),
            max_runtime_ms: self.max_runtime_ms,
        }
    }
}

/// Run the job; the result is persisted to artifacts and delivered to the spawning session.
pub(crate) async fn run(job: Job) {
    let Job {
        runtime,
        mut session,
        parent_model,
        model_from_profile,
        prompt,
        extra_steps,
        output_schema,
        schema_mode,
        isolation,
        approval,
        slot,
    } = job;
    let guard = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(guard) = guard else { return };
    let record = Arc::clone(guard.record());
    let tracker = BudgetTracker::new(&record, &runtime, approval.clone());

    let mut result = run_initial_turn(
        &runtime,
        &mut session,
        &prompt,
        parent_model.as_deref(),
        model_from_profile,
        &record,
        &tracker,
    )
    .await;

    if !extra_steps.is_empty() {
        result = run_extra_steps(
            &runtime,
            &session.id,
            result,
            extra_steps,
            &guard,
            &record,
            &tracker,
        )
        .await;
    }

    let (schema_warning, recovered_json) = apply_output_schema(
        &runtime,
        &session.id,
        output_schema.as_ref(),
        schema_mode,
        &mut result,
        &record,
        &tracker,
    )
    .await;

    let isolation_notes = if let Some(iso) = isolation {
        finalize_isolation(&guard, &record, iso, result.is_ok()).await
    } else {
        Vec::new()
    };

    if result.is_ok() {
        refresh_live_handle(
            &record,
            runtime,
            session,
            parent_model,
            model_from_profile,
            approval,
        );
    }

    let stop = tracker.stop_reason();
    settle(
        guard,
        result,
        &record,
        stop,
        recovered_json,
        schema_warning.as_deref(),
        &isolation_notes,
    );
}

pub(crate) fn refresh_live_handle(
    record: &AgentRecord,
    runtime: AgentRuntime,
    session: SessionMeta,
    parent_model: Option<String>,
    model_from_profile: bool,
    approval: Option<Arc<dyn ApprovalGate>>,
) {
    let updated_session = runtime
        .store
        .get_session(&session.id)
        .ok()
        .flatten()
        .unwrap_or(session);
    record.set_live(
        LiveHandle {
            runtime,
            session: updated_session,
            parent_model,
            model_from_profile,
            approval,
        },
        None,
    );
}

async fn run_extra_steps(
    runtime: &AgentRuntime,
    session_id: &str,
    mut result: Result<TurnResult>,
    extra_steps: Vec<String>,
    guard: &JobGuard,
    record: &AgentRecord,
    tracker: &BudgetTracker,
) -> Result<TurnResult> {
    let total_steps = extra_steps.len() + 1;
    for (idx, step_prompt) in extra_steps.into_iter().enumerate() {
        let Ok(prev) = &result else { break };
        let step_num = idx + 1;
        record.add_progress_note(format!(
            "step {step_num}/{total_steps}: {}",
            clip_progress(&prev.final_text)
        ));
        if let Some(reg) = guard.registry() {
            reg.persist_meta(record);
        }
        clear_yield_slot(record);
        let formatted = format!("Step {}/{total_steps}:\n{step_prompt}", step_num + 1);
        result = runtime
            .run_turn_settled(session_id, &formatted, &record.control, tracker.make_io())
            .await;
    }
    result
}

async fn run_initial_turn(
    runtime: &AgentRuntime,
    session: &mut SessionMeta,
    prompt: &str,
    parent_model: Option<&str>,
    model_from_profile: bool,
    record: &AgentRecord,
    tracker: &BudgetTracker,
) -> Result<TurnResult> {
    let res = runtime
        .run_turn_settled(&session.id, prompt, &record.control, tracker.make_io())
        .await;
    if matches!(res, Err(HrnError::Config(_)))
        && model_from_profile
        && session.model.as_deref() != parent_model
    {
        session.model = parent_model.map(str::to_owned);
        return match runtime.store.update_session(session) {
            Ok(()) => {
                runtime
                    .run_turn_settled(&session.id, prompt, &record.control, tracker.make_io())
                    .await
            }
            Err(err) => Err(err),
        };
    }
    res
}

#[derive(Clone, Copy)]
pub(crate) struct StopReason {
    budget_stopped: bool,
    hard_budget: usize,
    runtime_stopped: bool,
    max_runtime_ms: u64,
}

pub(crate) fn settle(
    guard: JobGuard,
    result: Result<TurnResult>,
    record: &AgentRecord,
    stop: StopReason,
    recovered_json: Option<Value>,
    schema_warning: Option<&str>,
    isolation_notes: &[String],
) {
    let append_notes = |base: &str| -> String {
        let mut out = base.to_owned();
        if let Some(warn) = schema_warning {
            let _ = write!(out, "\n\n{warn}");
        }
        if !isolation_notes.is_empty() {
            let _ = write!(out, "\n\n[isolation]\n{}", isolation_notes.join("\n"));
        }
        out
    };
    match result {
        Ok(done) => {
            let yielded = take_yield_slot(record);
            let yield_json: Option<Value> = recovered_json.or_else(|| {
                yielded
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            });
            let final_text = yielded
                .or_else(|| {
                    yield_json
                        .as_ref()
                        .and_then(|v| serde_json::to_string_pretty(v).ok())
                })
                .unwrap_or(done.final_text);
            let trimmed = final_text.trim();
            let base = if trimmed.is_empty() {
                "(the subagent produced no final message)"
            } else {
                trimmed
            };
            let full = append_notes(base);
            let artifact_path = write_artifacts(&guard, record, &full, yield_json.as_ref());
            guard.finish(JobStatus::Idle, full, yield_json, artifact_path);
        }
        Err(HrnError::Cancelled) if stop.budget_stopped => guard.finish(
            JobStatus::Failed,
            append_notes(&format!(
                "error: hard request budget ({}) exceeded",
                stop.hard_budget
            )),
            None,
            None,
        ),
        Err(HrnError::Cancelled) if stop.runtime_stopped => guard.finish(
            JobStatus::Failed,
            append_notes(&format!(
                "error: max runtime ({}ms) exceeded",
                stop.max_runtime_ms
            )),
            None,
            None,
        ),
        Err(HrnError::Cancelled) => {
            guard.finish(JobStatus::Aborted, append_notes("aborted"), None, None);
        }
        Err(err) => guard.finish(
            JobStatus::Failed,
            append_notes(&format!("error: {err}")),
            None,
            None,
        ),
    }
}

fn clear_yield_slot(record: &AgentRecord) {
    *record
        .control
        .yield_data()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

pub(crate) fn take_yield_slot(record: &AgentRecord) -> Option<String> {
    record
        .control
        .yield_data()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

pub(crate) fn set_yield_slot(record: &AgentRecord, raw: String) {
    *record
        .control
        .yield_data()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(raw);
}

fn clip_progress(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() > 240 {
        format!(
            "{}...",
            trimmed
                .get(..trimmed.floor_char_boundary(240))
                .unwrap_or_default()
        )
    } else if trimmed.is_empty() {
        "(no output)".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn write_artifacts(
    guard: &JobGuard,
    record: &AgentRecord,
    text: &str,
    yield_json: Option<&Value>,
) -> Option<PathBuf> {
    let store = guard.registry()?.artifacts()?;
    store
        .write_output(&record.root, &record.id, text, yield_json)
        .ok()
}

// ===== merged from agent_job_post.rs =====
// Post-turn structured `yield` schema enforcement (up to 3 reminders + missing-yield JSON
// recovery) and worktree isolation finalization helpers for [`crate::agents`].

use crate::isolation::IsolationFinalize;
use crate::json_schema::validate_json;

/// Maximum reminder turns sent when a child fails to `yield` schema-valid JSON.
const MAX_YIELD_REMINDERS: usize = 3;

pub(crate) async fn apply_output_schema(
    runtime: &AgentRuntime,
    session_id: &str,
    output_schema: Option<&Value>,
    schema_mode: SchemaMode,
    result: &mut Result<TurnResult>,
    record: &AgentRecord,
    tracker: &BudgetTracker,
) -> (Option<String>, Option<Value>) {
    let (Ok(done), Some(schema)) = (result.as_mut(), output_schema) else {
        return (None, None);
    };
    match enforce_yield_schema(
        runtime,
        session_id,
        schema,
        schema_mode,
        done,
        record,
        tracker,
    )
    .await
    {
        Ok(SchemaCheck::Valid(val)) => (None, val),
        Ok(SchemaCheck::Warn(warn, val)) => (Some(warn), val),
        Err(err) => {
            *result = Err(err);
            (None, None)
        }
    }
}

enum SchemaCheck {
    Valid(Option<Value>),
    Warn(String, Option<Value>),
}

async fn enforce_yield_schema(
    runtime: &AgentRuntime,
    session_id: &str,
    schema: &Value,
    mode: SchemaMode,
    done: &mut TurnResult,
    record: &AgentRecord,
    tracker: &BudgetTracker,
) -> Result<SchemaCheck> {
    let mut last_invalid_raw: Option<String> = None;
    let mut last_invalid_val: Option<Value> = None;
    let mut last_errors: Vec<String>;

    for attempt in 0..=MAX_YIELD_REMINDERS {
        let raw_yield = take_yield_slot(record);
        if let Some(raw) = raw_yield {
            match serde_json::from_str::<Value>(&raw) {
                Ok(val) => {
                    let errs = validate_json(schema, &val);
                    if errs.is_empty() {
                        set_yield_slot(record, raw);
                        return Ok(SchemaCheck::Valid(Some(val)));
                    }
                    last_invalid_raw = Some(raw);
                    last_invalid_val = Some(val);
                    last_errors = errs;
                }
                Err(e) => {
                    last_invalid_raw = Some(raw);
                    last_errors = vec![format!("yield payload is not valid JSON: {e}")];
                }
            }
        } else {
            last_errors = vec!["missing structured output: you must call the `yield` tool".into()];
        }

        if attempt == MAX_YIELD_REMINDERS {
            if last_invalid_raw.is_none()
                && let Some(recovered) = recover_json_from_text(&done.final_text)
                && validate_json(schema, &recovered).is_empty()
            {
                return Ok(SchemaCheck::Valid(Some(recovered)));
            }
            let detail = last_errors.join("; ");
            return match mode {
                SchemaMode::Strict => Err(HrnError::Tool(format!(
                    "structured output schema validation failed: {detail}"
                ))),
                SchemaMode::Permissive => {
                    if let Some(raw) = last_invalid_raw {
                        set_yield_slot(record, raw);
                    }
                    Ok(SchemaCheck::Warn(
                        format!("[schema warning] output did not match outputSchema: {detail}"),
                        last_invalid_val,
                    ))
                }
            };
        }

        let reminder = format!(
            "[system] Schema reminder ({}/{MAX_YIELD_REMINDERS}): your response did not pass \
             `outputSchema` validation ({}). Call the `yield` tool now with valid JSON matching \
             the required schema.",
            attempt + 1,
            last_errors.join("; ")
        );
        let next = runtime
            .run_turn_settled(session_id, &reminder, &record.control, tracker.make_io())
            .await?;
        *done = next;
    }
    Ok(SchemaCheck::Valid(None))
}

fn recover_json_from_text(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }
    let start = trimmed.find("```json").or_else(|| trimmed.find("```"))?;
    let after = trimmed.get(start..)?;
    let body_start = after.find('\n')? + 1;
    let body = after.get(body_start..)?;
    let end = body.find("```")?;
    serde_json::from_str::<Value>(body.get(..end)?.trim()).ok()
}

pub(crate) async fn finalize_isolation(
    guard: &JobGuard,
    record: &AgentRecord,
    iso: JobIsolation,
    child_succeeded: bool,
) -> Vec<String> {
    let artifacts = guard.registry().and_then(|r| r.artifacts());
    let req = IsolationFinalize {
        root_session_id: record.root.clone(),
        agent_id: record.id.clone(),
        allowed_paths: iso.allowed_paths,
        denied_paths: iso.denied_paths,
        config: iso.config,
        child_succeeded,
        artifacts,
    };
    match iso.isolator.finalize(&iso.worktree, req).await {
        Ok(outcome) => {
            record.set_isolation_result(outcome.patch_path.clone(), outcome.branch.clone());
            let mut lines = Vec::new();
            if let Some(path) = &outcome.patch_path {
                lines.push(format!("patch: {}", path.display()));
            }
            if let Some(branch) = &outcome.branch {
                lines.push(format!("branch: {branch}"));
            }
            if outcome.applied {
                lines.push("applied: true".to_owned());
            }
            lines.extend(outcome.notes);
            lines
        }
        Err(err) => vec![format!("worktree isolation finalize failed: {err}")],
    }
}

// ===== merged from agent_restore_job.rs =====
// Background subagent crash-restore job driver and root-tree `--restore` orchestration.
//
// On `--restore`, persisted child sessions left mid-turn (excluding tombstoned or user-Esc-aborted
// children) are repaired and cold-revived FIRST as background jobs of their parents before the
// main session's own interrupted turn resumes.

use crate::runtime::diagnose_interrupt;

/// Summary of what `--restore` found and resumed across the root session and its subagent tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeRestoreSummary {
    /// Whether the main (root) session itself had an interrupted turn.
    pub main_interrupted: bool,
    /// Number of mid-turn child subagents revived as background jobs of their parents.
    pub subagent_count: usize,
}

impl TreeRestoreSummary {
    /// Format the `omp`-style startup flash message describing what `--restore` resumed.
    #[must_use]
    pub fn flash_message(self) -> String {
        match (self.main_interrupted, self.subagent_count) {
            (true, n) if n > 0 => {
                format!("restored: going on with the interrupted turn and {n} subagents.")
            }
            (false, n) if n > 0 => {
                format!("restored: {n} subagents going on with their interrupted turns.")
            }
            (true, _) => "restored: going on with the interrupted turn…".to_owned(),
            (false, _) => "restored: nothing was left unfinished".to_owned(),
        }
    }
}

/// Prepared background restore job for one interrupted child session.
pub(crate) struct RestoreJob {
    pub runtime: AgentRuntime,
    pub session: SessionMeta,
    pub parent_model: Option<String>,
    pub model_from_profile: bool,
    pub approval: Option<Arc<dyn ApprovalGate>>,
    pub slot: GuardSlot,
}

/// Execute one cold-revived interrupted child session to completion, write artifacts, and deliver
/// its result to the parent session's inbox.
pub(crate) async fn run_restore(job: RestoreJob) {
    let RestoreJob {
        runtime,
        session,
        parent_model,
        model_from_profile,
        approval,
        slot,
    } = job;
    let guard = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(guard) = guard else { return };
    let record = Arc::clone(guard.record());
    let tracker = BudgetTracker::new(&record, &runtime, approval.clone());
    let outcome = runtime
        .restore_turn(&session.id, &record.control, tracker.make_io())
        .await;
    match outcome {
        Ok(Some(done)) => {
            refresh_live_handle(
                &record,
                runtime,
                session,
                parent_model,
                model_from_profile,
                approval,
            );
            let stop = tracker.stop_reason();
            settle(guard, Ok(done), &record, stop, None, None, &[]);
        }
        Ok(None) => {
            guard.cancel_reservation();
            let now = runtime.clock.now_ms();
            let mut s = record.state();
            s.status = JobStatus::Idle;
            s.idle_since_ms = now;
            s.finished_ms = now;
        }
        Err(err) => {
            let stop = tracker.stop_reason();
            settle(guard, Err(err), &record, stop, None, None, &[]);
        }
    }
}

/// Recursively collect every persisted descendant session of `root_session_id` (parents before
/// children), guarding against malformed cycles.
fn collect_descendants(runtime: &AgentRuntime, root_session_id: &str) -> Result<Vec<SessionMeta>> {
    let mut visited = HashSet::new();
    visited.insert(root_session_id.to_owned());
    let mut queue = vec![root_session_id.to_owned()];
    let mut out = Vec::new();
    while let Some(parent_id) = queue.first().cloned() {
        queue.remove(0);
        for child in runtime.store.list_children(&parent_id)? {
            if visited.insert(child.id.clone()) {
                queue.push(child.id.clone());
                out.push(child);
            }
        }
    }
    Ok(out)
}

fn is_child_restorable(
    runtime: &AgentRuntime,
    meta: &SessionMeta,
    record: Option<&AgentRecord>,
) -> Result<bool> {
    if let Some(rec) = record {
        let status = rec.snapshot().status;
        if matches!(status, JobStatus::Aborted | JobStatus::Running) {
            return Ok(false);
        }
    }
    let branch = match &meta.head_node_id {
        Some(head) => runtime.store.load_branch(&meta.id, head)?,
        None => Vec::new(),
    };
    Ok(diagnose_interrupt(meta, &branch).is_interrupted())
}

impl AgentRuntime {
    /// Count how many persisted non-tombstoned child sessions of `root_session_id` were left
    /// mid-turn and are eligible for cold-revive on `--restore` (read-only inspection).
    ///
    /// # Errors
    /// Propagates session store read failures.
    pub fn count_interrupted_subagents(&self, root_session_id: &str) -> Result<usize> {
        let records = self.agents.in_tree(root_session_id);
        let descendants = collect_descendants(self, root_session_id)?;
        let mut count = 0usize;
        for child in &descendants {
            let rec = records
                .iter()
                .find(|r| !r.is_main && r.snapshot().session_id.as_deref() == Some(&child.id));
            if is_child_restorable(self, child, rec.map(Arc::as_ref))? {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Find every persisted child session of `root_session_id` left mid-turn (skipping tombstoned
    /// or user-Esc-aborted children), repair and cold-revive each as a background job of its
    /// parent, and return the revived agent IDs.
    ///
    /// # Errors
    /// Propagates store read, live-handle reconstruction, or background spawner failures.
    pub fn restore_interrupted_subagents(
        &self,
        root_session_id: &str,
        approval: Option<&Arc<dyn ApprovalGate>>,
    ) -> Result<Vec<String>> {
        if self.agents.host().is_none() {
            self.agents.attach_host(self);
        }
        let _ = self
            .agents
            .resurrect_tree(root_session_id, self.store.as_ref())?;
        let descendants = collect_descendants(self, root_session_id)?;
        let mut revived = Vec::new();
        for child in &descendants {
            let Some(record) = self.agents.in_tree(root_session_id).into_iter().find(|r| {
                !r.is_main && r.snapshot().session_id.as_deref() == Some(child.id.as_str())
            }) else {
                continue;
            };
            if !is_child_restorable(self, child, Some(record.as_ref()))? {
                continue;
            }
            if self.spawn_child_restore(&record, approval.cloned())? {
                revived.push(record.id.clone());
            }
        }
        Ok(revived)
    }

    fn spawn_child_restore(
        &self,
        record: &Arc<AgentRecord>,
        approval: Option<Arc<dyn ApprovalGate>>,
    ) -> Result<bool> {
        let mut live = self.agents.prepare_live_handle(record)?;
        if approval.is_some() {
            live.approval = approval;
        }
        let now = self.clock.now_ms();
        let (new_gen, prev) = {
            let mut s = record.state();
            if matches!(s.status, JobStatus::Aborted | JobStatus::Running) {
                return Ok(false);
            }
            let backup = (s.status, s.finished_ms, s.idle_since_ms, s.generation);
            s.generation = s.generation.saturating_add(1);
            s.status = JobStatus::Running;
            s.finished_ms = 0;
            s.idle_since_ms = 0;
            "restoring interrupted turn".clone_into(&mut s.activity);
            s.cancel = CancelFlag::new();
            s.session_id = Some(live.session.id.clone());
            s.live = Some(live.clone());
            (s.generation, backup)
        };
        record.note_lifecycle(format!("restoring interrupted turn (gen {new_gen})"), now);
        let guard = self.agents.make_guard(Arc::clone(record), new_gen);
        let slot: GuardSlot = Arc::new(Mutex::new(Some(guard)));
        let job = RestoreJob {
            runtime: live.runtime,
            session: live.session,
            parent_model: live.parent_model,
            model_from_profile: live.model_from_profile,
            approval: live.approval,
            slot: Arc::clone(&slot),
        };
        if let Err(err) = self.agents.spawn(Box::pin(run_restore(job))) {
            let mut s = record.state();
            s.status = prev.0;
            s.finished_ms = prev.1;
            s.idle_since_ms = prev.2;
            s.generation = prev.3;
            drop(s);
            if let Some(g) = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                g.cancel_reservation();
            }
            return Err(err);
        }
        self.agents.persist_meta(record);
        self.agents.inbox().wake();
        Ok(true)
    }

    /// Full `--restore` flow for `root_session_id`: FIRST revives all interrupted non-tombstoned
    /// child subagents as background jobs of their parents, THEN restores the main session's own
    /// interrupted turn (if any).
    ///
    /// # Errors
    /// Propagates subagent restore or main turn restore errors.
    pub async fn restore_tree(
        &self,
        root_session_id: &str,
        control: &SessionControl,
        io: TurnIo,
    ) -> Result<(Vec<String>, Option<TurnResult>)> {
        let revived_subagents =
            self.restore_interrupted_subagents(root_session_id, io.approval.as_ref())?;
        let main_result = self.restore_turn(root_session_id, control, io).await?;
        Ok((revived_subagents, main_result))
    }
}

// ===== merged from agent_revive.rs =====
// Agent revival (from `Idle`, `Parked`, or `Aborted`) and startup/resume resurrection from
// on-disk artifacts and stored child sessions.

use crate::artifacts::{StoredArtifact, parent_of_id};
use crate::profiles::{default_subagent_profiles, load_subagent_profiles, lookup_profile};
use crate::subagent::filter_child_tools;
use hrn_kernel::{DagPayload, Msg, PermissionMode, Role};
use std::collections::HashMap;

impl AgentRegistry {
    /// Revive an `Idle`, `Parked`, or `Aborted` agent in `root`, incrementing its generation and
    /// spawning a follow-up turn with `prompt` on its stored transcript/session.
    ///
    /// Returns the new generation number (`>= 2`).
    pub fn revive(self: &Arc<Self>, root: &str, id: &str, prompt: &str) -> Result<u32> {
        if prompt.trim().is_empty() {
            return Err(HrnError::Tool(
                "a follow-up message to an agent must not be empty".to_owned(),
            ));
        }
        let record = self
            .get_in_tree(root, id)
            .ok_or_else(|| HrnError::Tool(format!("no background agent `{id}` in this session")))?;
        if record.is_main {
            return Err(HrnError::Tool("cannot revive the main agent".to_owned()));
        }
        let prev_status = record.snapshot().status;
        if !prev_status.can_revive() {
            return Err(HrnError::Tool(format!(
                "agent `{id}` is {} and cannot be revived; start a new one with `task`",
                prev_status.as_str()
            )));
        }

        let live = self.prepare_live_handle(&record)?;
        let (new_gen, prev_state) = {
            let mut s = record.state();
            if !s.status.can_revive() {
                return Err(HrnError::Tool(format!(
                    "agent `{id}` is {} and cannot be revived",
                    s.status.as_str()
                )));
            }
            let backup = (s.status, s.finished_ms, s.idle_since_ms, s.generation);
            s.generation = s.generation.saturating_add(1);
            s.status = JobStatus::Running;
            s.finished_ms = 0;
            s.idle_since_ms = 0;
            s.activity.clear();
            s.cancel = CancelFlag::new();
            s.session_id = Some(live.session.id.clone());
            s.live = Some(live.clone());
            (s.generation, backup)
        };
        // Clear any previous turn's structured yield so the follow-up starts clean.
        *record
            .control
            .yield_data()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        let guard = self.make_guard(Arc::clone(&record), new_gen);
        let slot: GuardSlot = Arc::new(Mutex::new(Some(guard)));
        let job = Job {
            runtime: live.runtime,
            session: live.session,
            parent_model: live.parent_model,
            model_from_profile: live.model_from_profile,
            prompt: prompt.to_owned(),
            extra_steps: Vec::new(),
            output_schema: None,
            schema_mode: SchemaMode::Permissive,
            isolation: None,
            approval: live.approval,
            slot: Arc::clone(&slot),
        };
        if let Err(err) = self.spawn(Box::pin(run(job))) {
            let mut s = record.state();
            s.status = prev_state.0;
            s.finished_ms = prev_state.1;
            s.idle_since_ms = prev_state.2;
            s.generation = prev_state.3;
            drop(s);
            if let Some(g) = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                g.cancel_reservation();
            }
            return Err(err);
        }
        self.persist_meta(&record);
        Ok(new_gen)
    }

    /// Revive a `Parked` or `Aborted` (or already `Idle`) agent in `root` to `JobStatus::Idle`
    /// with a resident live runtime and session, without submitting a turn prompt yet.
    /// Used by Agent Hub `r` and focusing (`Enter`) a parked/aborted agent.
    ///
    /// # Errors
    /// Fails if the agent is unknown, main, `Failed`, or rebuilding its live handle fails.
    pub fn revive_to_idle(self: &Arc<Self>, root: &str, id: &str) -> Result<SessionMeta> {
        let record = self
            .get_in_tree(root, id)
            .ok_or_else(|| HrnError::Tool(format!("no background agent `{id}` in this session")))?;
        if record.is_main {
            return Err(HrnError::Tool("cannot revive the main agent".to_owned()));
        }
        let status = record.snapshot().status;
        if status == JobStatus::Running {
            let live = self.prepare_live_handle(&record)?;
            return Ok(live.session);
        }
        if !status.can_revive() {
            return Err(HrnError::Tool(format!(
                "agent `{id}` is {} and cannot be revived",
                status.as_str()
            )));
        }
        let live = self.prepare_live_handle(&record)?;
        let session = live.session.clone();
        let now = self.clock().now_ms();
        let new_gen = {
            let mut s = record.state();
            if s.status != JobStatus::Idle {
                s.generation = s.generation.saturating_add(1);
            }
            s.status = JobStatus::Idle;
            s.idle_since_ms = now;
            s.cancel = CancelFlag::new();
            s.session_id = Some(session.id.clone());
            s.live = Some(live);
            s.generation
        };
        if status != JobStatus::Idle {
            record.note_lifecycle(format!("revived to idle (gen {new_gen})"), now);
            self.persist_meta(&record);
            self.inbox().wake();
        }
        Ok(session)
    }

    pub(crate) fn prepare_live_handle(
        self: &Arc<Self>,
        record: &AgentRecord,
    ) -> Result<LiveHandle> {
        let (existing_live, session_id, spec, prior_output) = {
            let s = record.state();
            (
                s.live.clone(),
                s.session_id.clone(),
                s.revive_spec.clone(),
                s.output.clone(),
            )
        };
        if let Some(mut live) = existing_live {
            if let Some(fresh) = live.runtime.store.get_session(&live.session.id)? {
                live.session = fresh;
            }
            return Ok(live);
        }
        let host = self.host().ok_or_else(|| {
            HrnError::Config(format!(
                "cannot revive `{}`: no runtime host is attached to this registry",
                record.id
            ))
        })?;
        self.rebuild_live_handle(&host, record, session_id.as_deref(), spec, &prior_output)
    }

    fn rebuild_live_handle(
        self: &Arc<Self>,
        host: &ReviveHost,
        record: &AgentRecord,
        session_id: Option<&str>,
        spec: Option<ReviveSpec>,
        prior_output: &str,
    ) -> Result<LiveHandle> {
        let base_rt = self.assemble_host_runtime(host);
        let existing_session = match session_id {
            Some(sid) => host.store.get_session(sid)?,
            None => None,
        };
        let cwd = existing_session
            .as_ref()
            .map(|m| PathBuf::from(&m.cwd))
            .or_else(|| spec.as_ref().map(|s| s.cwd.clone()))
            .or_else(|| {
                host.store
                    .get_session(&record.root)
                    .ok()
                    .flatten()
                    .map(|m| PathBuf::from(&m.cwd))
            })
            .unwrap_or_else(|| PathBuf::from("."));

        let spec = match spec {
            Some(s) => s,
            None => ReviveSpec {
                profile: resolve_profile(host.fs.as_ref(), &cwd, &record.profile),
                scope: ScopePolicy::default(),
                depth: record.id.matches('.').count(),
                parent_model: None,
                model_from_profile: false,
                approval: None,
                cwd: cwd.clone(),
            },
        };
        let child_tools = filter_child_tools(&base_rt, &spec.profile, spec.depth, false);
        let mut child_rt = base_rt
            .with_tools(child_tools)
            .without_advisor()
            .as_subagent();
        if let Some(max_steps) = spec.profile.max_steps {
            child_rt = child_rt.with_max_steps(max_steps);
        }
        if !spec.scope.is_empty() {
            child_rt = spec.scope.apply_to_runtime(&child_rt, spec.cwd.clone());
        }
        let session = match existing_session {
            Some(meta) => meta,
            None => seed_session_from_artifact(&child_rt, record, &spec.profile, prior_output)?,
        };
        Ok(LiveHandle {
            runtime: child_rt,
            session,
            parent_model: spec.parent_model,
            model_from_profile: spec.model_from_profile,
            approval: spec.approval,
        })
    }

    fn assemble_host_runtime(self: &Arc<Self>, host: &ReviveHost) -> AgentRuntime {
        let urls = host.urls.upgrade().unwrap_or_default();
        let procs = host
            .procs
            .upgrade()
            .unwrap_or_else(|| Arc::new(ProcTable::new(vec![Arc::clone(self) as _])));
        let mut rt = AgentRuntime {
            llm: Arc::clone(&host.llm),
            store: Arc::clone(&host.store),
            tools: host.base_tools.clone(),
            fs: Arc::clone(&host.fs),
            clock: Arc::clone(&host.clock),
            config: host.config.clone(),
            extra_hooks: host.extra_hooks.clone(),
            rules_seen: Arc::clone(&host.rules_seen),
            urls,
            agents: Arc::clone(self),
            procs,
            todos: Arc::clone(&host.todos),
            grants: Arc::clone(&host.grants),
            isolator: host.isolator.clone(),
            dynamic_tools: host.dynamic_tools.clone(),
            snap_renderer: host.snap_renderer.clone(),
            speculative: Arc::clone(&host.speculative),
            title_revs: Arc::default(),
        };
        // Avoid losing empty router if weak reference was not initialized.
        if Arc::strong_count(&rt.urls) == 0 {
            rt.urls = Arc::new(UrlRouter::new());
        }
        rt
    }

    /// Resurrect parked agent rows and parent/child lineage for `root_session_id` from on-disk
    /// artifacts (`<data>/sessions/<root>/artifacts/`) and stored child sessions in `store`.
    /// Returns the number of newly added records.
    pub fn resurrect_tree(&self, root_session_id: &str, store: &dyn SessionStore) -> Result<usize> {
        let mut added = 0usize;
        let mut session_to_agent: HashMap<String, String> = HashMap::new();
        for existing in self.in_tree(root_session_id) {
            if let Some(sid) = existing.snapshot().session_id {
                session_to_agent.insert(sid, existing.id.clone());
            }
        }
        if let Some(artifacts) = self.artifacts() {
            let stored = artifacts.list_for_root(root_session_id)?;
            for item in stored {
                if self.get_in_tree(root_session_id, &item.id).is_some() {
                    continue;
                }
                let rec = build_resurrected_record(&artifacts, root_session_id, item)?;
                if let Some(sid) = rec.snapshot().session_id {
                    session_to_agent.insert(sid, rec.id.clone());
                }
                self.list().push(rec);
                added += 1;
            }
        }
        added += self.resurrect_store_children(
            root_session_id,
            root_session_id,
            store,
            &mut session_to_agent,
        )?;
        Ok(added)
    }

    fn resurrect_store_children(
        &self,
        root_session_id: &str,
        parent_session_id: &str,
        store: &dyn SessionStore,
        session_to_agent: &mut HashMap<String, String>,
    ) -> Result<usize> {
        let mut added = 0usize;
        let children = store.list_children(parent_session_id)?;
        for child in children {
            if !session_to_agent.contains_key(&child.id) {
                let parent_agent = session_to_agent.get(parent_session_id).cloned();
                let profile = child
                    .title
                    .strip_prefix("subagent:")
                    .unwrap_or("explore")
                    .to_owned();
                let id = {
                    let agents = self.list();
                    allocate_id(&profile, parent_agent.as_deref(), |cand| {
                        agents
                            .iter()
                            .any(|a| a.root == root_session_id && a.id == cand)
                    })
                };
                let output = last_assistant_text(store, &child);
                let artifact_dir = self.artifacts().map(|s| s.artifacts_dir(root_session_id));
                let rec = Arc::new(AgentRecord {
                    id: id.clone(),
                    name: profile.clone(),
                    profile,
                    owner: parent_session_id.to_owned(),
                    root: root_session_id.to_owned(),
                    parent: parent_agent,
                    task: child.title.clone(),
                    is_main: false,
                    control: SessionControl::default(),
                    started_ms: child.created_at_ms,
                    artifact_dir,
                    state: Mutex::new(AgentState {
                        status: JobStatus::Parked,
                        session_id: Some(child.id.clone()),
                        steps: 0,
                        usage: AgentUsage::default(),
                        activity: String::new(),
                        output,
                        yield_json: None,
                        progress_notes: Vec::new(),
                        finished_ms: child.updated_at_ms,
                        idle_since_ms: 0,
                        generation: 1,
                        artifact_path: None,
                        patch_path: None,
                        isolated_branch: None,
                        worktree: None,
                        cancel: CancelFlag::new(),
                        live: None,
                        revive_spec: None,
                        telemetry: AgentTelemetry::default(),
                        focus_sink: None,
                    }),
                });
                session_to_agent.insert(child.id.clone(), id);
                self.list().push(rec);
                added += 1;
            }
            added +=
                self.resurrect_store_children(root_session_id, &child.id, store, session_to_agent)?;
        }
        Ok(added)
    }
}

fn build_resurrected_record(
    store: &crate::artifacts::ArtifactStore,
    root: &str,
    item: StoredArtifact,
) -> Result<Arc<AgentRecord>> {
    let md = store.read_md(root, &item.id)?.unwrap_or_default();
    let json_val = store.read_json(root, &item.id).ok().flatten();
    let dir = store.artifacts_dir(root);
    let derived_parent = parent_of_id(&item.id).map(str::to_owned);
    let leaf_name = item
        .id
        .rsplit_once('.')
        .map_or(item.id.as_str(), |(_, child)| child)
        .to_owned();
    let (
        name,
        profile,
        owner,
        parent,
        task,
        session_id,
        status,
        started_ms,
        finished_ms,
        generation,
        usage,
        progress_notes,
        patch_path,
        isolated_branch,
    ) = match item.meta {
        Some(m) => (
            m.name,
            m.profile,
            m.owner,
            m.parent.or(derived_parent),
            m.task,
            m.session_id,
            if m.status == JobStatus::Aborted {
                JobStatus::Aborted
            } else {
                JobStatus::Parked
            },
            m.started_ms,
            m.finished_ms,
            m.generation.max(1),
            m.usage,
            m.progress_notes,
            m.patch_path.or(item.patch_path),
            m.isolated_branch,
        ),
        None => (
            leaf_name,
            "resurrected".to_owned(),
            root.to_owned(),
            derived_parent,
            format!("resurrected artifact {}", item.id),
            None,
            JobStatus::Parked,
            0,
            0,
            1,
            AgentUsage::default(),
            Vec::new(),
            item.patch_path,
            None,
        ),
    };
    Ok(Arc::new(AgentRecord {
        id: item.id,
        name,
        profile,
        owner,
        root: root.to_owned(),
        parent,
        task,
        is_main: false,
        control: SessionControl::default(),
        started_ms,
        artifact_dir: Some(dir),
        state: Mutex::new(AgentState {
            status,
            session_id,
            steps: usize::try_from(usage.requests).unwrap_or(0),
            usage,
            activity: String::new(),
            output: md,
            yield_json: json_val,
            progress_notes,
            finished_ms,
            idle_since_ms: 0,
            generation,
            artifact_path: item.md_path,
            patch_path,
            isolated_branch,
            worktree: None,
            cancel: CancelFlag::new(),
            live: None,
            revive_spec: None,
            telemetry: AgentTelemetry::default(),
            focus_sink: None,
        }),
    }))
}

fn resolve_profile(
    fs: &dyn hrn_kernel::WorkspaceFs,
    cwd: &std::path::Path,
    name: &str,
) -> SubagentProfile {
    if let Ok(profiles) = load_subagent_profiles(fs, cwd)
        && let Some(p) = lookup_profile(&profiles, name)
    {
        return p.clone();
    }
    let builtins = default_subagent_profiles();
    lookup_profile(&builtins, name)
        .or_else(|| lookup_profile(&builtins, "scout"))
        .cloned()
        .unwrap_or_else(|| SubagentProfile {
            name: name.to_owned(),
            aliases: Vec::new(),
            description: "Resurrected subagent".to_owned(),
            instructions: String::new(),
            read_only: true,
            model: None,
            thinking: None,
            tools: Vec::new(),
            deny_tools: Vec::new(),
            max_steps: Some(25),
            spawns: None,
            output: None,
            blocking: false,
            structured: false,
            advisor: None,
            prewalk_ignored: false,
            source: crate::discovery::DiscoverySource::BuiltIn,
        })
}

fn seed_session_from_artifact(
    child_rt: &AgentRuntime,
    record: &AgentRecord,
    profile: &SubagentProfile,
    prior_output: &str,
) -> Result<SessionMeta> {
    let parent = child_rt
        .store
        .get_session(&record.owner)?
        .or(child_rt.store.get_session(&record.root)?)
        .ok_or_else(|| HrnError::Store(format!("unknown parent session `{}`", record.owner)))?;
    let mode = if profile.read_only {
        PermissionMode::Plan
    } else {
        parent.permission_mode
    };
    let mut session = child_rt.create_child_session(
        &parent,
        parent.head_node_id.clone(),
        format!("subagent:{}", profile.name),
        profile.model.clone(),
        mode,
    )?;
    if !prior_output.trim().is_empty() {
        child_rt.append_message(&mut session, "resurrect", Msg::assistant(prior_output))?;
    }
    Ok(session)
}

fn last_assistant_text(store: &dyn SessionStore, meta: &SessionMeta) -> String {
    let Some(head) = meta.head_node_id.as_deref() else {
        return String::new();
    };
    store
        .load_branch(&meta.id, head)
        .map(|branch| {
            branch
                .iter()
                .rev()
                .find_map(|node| match &node.payload {
                    DagPayload::Message { msg, .. } if msg.role == Role::Assistant => {
                        Some(msg.content.clone())
                    }
                    _ => None,
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

// ===== merged from agent_urls.rs =====
// `agent://`: inspect, message, revive, broadcast, extract JSON paths, and record progress notes
// for background agents through the internal URL router.
//
// Grammar: `agent://<id>[.<child>][/<json-pointer>][?progress]` or `agent://all`.

use hrn_kernel::{ToolContext, UrlBody, UrlScheme};

/// Canonical grammar named in all malformed-spec errors.
pub const AGENT_URL_GRAMMAR: &str =
    "agent://<id>[.<child>][/<json-pointer>][?progress] or agent://all";

/// Parsed `agent://` URL after stripping the `agent://` scheme prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentUrl {
    /// Bare `agent://`: list all agents of the current session tree.
    List,
    /// `agent://all`: broadcast target (main agent only).
    All,
    /// Specific agent (`<id>` or nested `<parent>.<child>`), with optional JSON pointer path
    /// (`/<key>/<index>/...`) or `?progress` note target.
    Target {
        /// Full dotted agent identifier (`id` or `id.child`).
        id: String,
        /// Unescaped JSON pointer segments (empty when addressing the agent itself).
        json_path: Vec<String>,
        /// Whether `?progress` was requested.
        progress: bool,
    },
}

fn malformed(rest: &str, detail: &str) -> HrnError {
    HrnError::Tool(format!(
        "malformed `agent://{rest}` ({detail}); expected {AGENT_URL_GRAMMAR}"
    ))
}

fn valid_id_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn unescape_pointer_segment(seg: &str) -> String {
    seg.replace("~1", "/").replace("~0", "~")
}

/// Parse the remainder of an `agent://` URL.
pub fn parse_agent_url(rest: &str) -> Result<AgentUrl> {
    if rest.is_empty() || rest == "/" {
        return Ok(AgentUrl::List);
    }
    let (path, progress) = match rest.split_once('?') {
        Some((p, q)) => {
            if q.contains('?') {
                return Err(malformed(rest, "multiple `?` query separators"));
            }
            if q != "progress" {
                return Err(malformed(rest, &format!("unsupported query `?{q}`")));
            }
            (p, true)
        }
        None => (rest, false),
    };
    if path.is_empty() || path.starts_with('/') || path.ends_with('/') || path.contains("//") {
        return Err(malformed(rest, "invalid path separators"));
    }
    if path == "all" {
        if progress {
            return Err(malformed(
                rest,
                "`?progress` is not valid with `agent://all`",
            ));
        }
        return Ok(AgentUrl::All);
    }
    let (id_part, ptr_part) = match path.split_once('/') {
        Some((id, ptr)) => (id, Some(ptr)),
        None => (path, None),
    };
    if id_part.is_empty() || !id_part.split('.').all(valid_id_segment) {
        return Err(malformed(rest, &format!("invalid agent id `{id_part}`")));
    }
    let json_path = match ptr_part {
        Some(ptr) => {
            if progress {
                return Err(malformed(
                    rest,
                    "`?progress` cannot be combined with a JSON pointer path",
                ));
            }
            let mut segs = Vec::new();
            for raw_seg in ptr.split('/') {
                if raw_seg.is_empty() || raw_seg == ".." {
                    return Err(malformed(rest, "empty or disallowed JSON pointer segment"));
                }
                segs.push(unescape_pointer_segment(raw_seg));
            }
            segs
        }
        None => Vec::new(),
    };
    Ok(AgentUrl::Target {
        id: id_part.to_owned(),
        json_path,
        progress,
    })
}

fn age(ms: i64) -> String {
    match ms.max(0) / 1000 {
        s @ 0..60 => format!("{s}s"),
        s @ 60..3600 => format!("{}m{}s", s / 60, s % 60),
        s => format!("{}h{}m", s / 3600, s % 3600 / 60),
    }
}

fn is_subagent_ctx(ctx: &ToolContext) -> bool {
    ctx.is_subagent || ctx.session_id != ctx.root_session_id
}

fn find(agents: &AgentRegistry, ctx: &ToolContext, id: &str) -> Result<Arc<AgentRecord>> {
    agents.get_in_tree(&ctx.root_session_id, id).ok_or_else(|| {
        HrnError::Tool(format!(
            "no background agent `{id}` in this session (list them with `read agent://`)"
        ))
    })
}

/// The listing text of one agent: id, profile, status, generation, steps, age, and task.
pub(crate) fn line(agent: &AgentRecord, clock: &dyn Clock) -> String {
    let s = agent.snapshot();
    let end = if s.finished_ms > 0 {
        s.finished_ms
    } else {
        clock.now_ms()
    };
    let mut out = format!(
        "{} [{}] {} · gen {} · {} steps · {}",
        agent.id,
        agent.profile,
        s.status.as_str(),
        s.generation,
        s.steps,
        age(end - agent.started_ms)
    );
    if let Some(parent) = &agent.parent {
        let _ = write!(out, " · parent {parent}");
    }
    if s.status == JobStatus::Running && !s.activity.is_empty() {
        let _ = write!(out, " · {}", s.activity);
    }
    let _ = write!(out, "\n    {}", agent.task);
    out
}

/// One entry per agent in the session tree, oldest first.
fn listing(agents: &AgentRegistry, ctx: &ToolContext) -> String {
    let clock = agents.clock();
    let out = agents
        .in_tree(&ctx.root_session_id)
        .iter()
        .map(|agent| line(agent, clock.as_ref()))
        .collect::<Vec<_>>()
        .join("\n");
    if out.is_empty() {
        "(no background agents yet)".to_owned()
    } else {
        out
    }
}

/// Status, lineage, usage, progress, and full output of one agent (reading `<id>.md` on disk
/// when available so parked/resurrected agents serve their persisted artifact).
pub(crate) fn detail(
    agent: &AgentRecord,
    artifacts: Option<&ArtifactStore>,
    clock: &dyn Clock,
) -> String {
    let s = agent.snapshot();
    let end = if s.finished_ms > 0 {
        s.finished_ms
    } else {
        clock.now_ms()
    };
    let mut out = format!(
        "agent {}\nstatus: {} (gen {})\nprofile: {} · name: {}\nsteps: {} · requests: {} · \
         tool_calls: {} · tokens: {}/{} · {}\ntask: {}",
        agent.id,
        s.status.as_str(),
        s.generation,
        agent.profile,
        agent.name,
        s.steps,
        s.requests,
        s.tool_calls,
        s.input_tokens,
        s.output_tokens,
        age(end - agent.started_ms),
        agent.task
    );
    if s.cost_usd > 0.0 {
        let _ = write!(out, "\ncost: ${:.6}", s.cost_usd);
    }
    if let Some(parent) = &agent.parent {
        let _ = write!(out, "\nparent: {parent}");
    }
    if !s.activity.is_empty() && s.status == JobStatus::Running {
        let _ = write!(out, "\nactivity: {}", s.activity);
    }
    if let Some(session) = &s.session_id {
        let _ = write!(out, "\ntranscript: history://{session}");
    }
    if let Some(path) = &s.artifact_path {
        let _ = write!(out, "\nartifact: {}", path.display());
    }
    let patch_path = s.patch_path.clone().or_else(|| {
        artifacts
            .and_then(|store| store.patch_path(&agent.root, &agent.id).ok())
            .filter(|p| p.is_file())
    });
    if let Some(path) = &patch_path {
        let _ = write!(out, "\npatch: {}", path.display());
    }
    if let Some(branch) = &s.isolated_branch {
        let _ = write!(out, "\nbranch: {branch}");
    }
    if let Some(last_note) = s.progress_notes.last() {
        let _ = write!(out, "\nprogress: {last_note}");
    }
    if s.status != JobStatus::Running {
        let body = artifacts
            .and_then(|store| store.read_md(&agent.root, &agent.id).ok().flatten())
            .filter(|text| !text.is_empty())
            .unwrap_or(s.output);
        let _ = write!(out, "\n\n{body}");
    }
    out
}

fn extract_json_path(root: &Value, id: &str, segments: &[String]) -> Result<String> {
    let mut cur = root;
    for seg in segments {
        cur = match cur {
            Value::Object(map) => map.get(seg.as_str()),
            Value::Array(arr) => seg.parse::<usize>().ok().and_then(|idx| arr.get(idx)),
            _ => None,
        }
        .ok_or_else(|| {
            HrnError::Tool(format!(
                "JSON path `/{}` not found in `agent://{id}`",
                segments.join("/")
            ))
        })?;
    }
    Ok(match cur {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_owned(),
        other => serde_json::to_string_pretty(other)?,
    })
}

/// `agent://`: the background subagents of this session.
pub struct AgentScheme {
    agents: Arc<AgentRegistry>,
}

impl AgentScheme {
    /// Serve the agents of `agents`.
    pub fn new(agents: Arc<AgentRegistry>) -> Self {
        Self { agents }
    }

    fn read_json_path(&self, ctx: &ToolContext, id: &str, segments: &[String]) -> Result<String> {
        let disk_json = self
            .agents
            .artifacts()
            .and_then(|store| store.read_json(&ctx.root_session_id, id).transpose())
            .transpose()?;
        let value = if let Some(v) = disk_json {
            v
        } else {
            let agent = find(&self.agents, ctx, id)?;
            agent.snapshot().yield_json.ok_or_else(|| {
                HrnError::Tool(format!(
                    "agent `{id}` has no structured JSON output (`{id}.json`)"
                ))
            })?
        };
        extract_json_path(&value, id, segments)
    }

    fn read_target(
        &self,
        ctx: &ToolContext,
        id: &str,
        json_path: &[String],
        progress: bool,
    ) -> Result<UrlBody> {
        if progress {
            let agent = find(&self.agents, ctx, id)?;
            let notes = agent.snapshot().progress_notes;
            let body = if notes.is_empty() {
                "(no progress notes yet)".to_owned()
            } else {
                notes.join("\n")
            };
            return Ok(UrlBody::Text(body));
        }
        if !json_path.is_empty() {
            return self.read_json_path(ctx, id, json_path).map(UrlBody::Text);
        }
        let artifacts = self.agents.artifacts();
        if let Some(agent) = self.agents.get_in_tree(&ctx.root_session_id, id) {
            return Ok(UrlBody::Text(detail(
                &agent,
                artifacts.as_deref(),
                self.agents.clock().as_ref(),
            )));
        }
        if let Some(store) = artifacts
            && let Some(md) = store.read_md(&ctx.root_session_id, id)?
        {
            return Ok(UrlBody::Text(md));
        }
        Err(HrnError::Tool(format!(
            "no background agent `{id}` in this session (list them with `read agent://`)"
        )))
    }

    fn broadcast(&self, ctx: &ToolContext, content: &str) -> Result {
        if is_subagent_ctx(ctx) {
            return Err(HrnError::Tool(
                "`agent://all` broadcast is only allowed for the main agent, not subagents"
                    .to_owned(),
            ));
        }
        if content.trim().is_empty() {
            return Err(HrnError::Tool(
                "a message to an agent must not be empty".to_owned(),
            ));
        }
        let tree: Vec<Arc<AgentRecord>> = self
            .agents
            .in_tree(&ctx.root_session_id)
            .into_iter()
            .filter(|a| !a.is_main)
            .collect();
        if tree.is_empty() {
            return Err(HrnError::Tool(
                "no background agents in this session to broadcast to".to_owned(),
            ));
        }
        let mut delivered = 0usize;
        for agent in tree {
            match agent.snapshot().status {
                JobStatus::Running => {
                    agent.control.push_steer(content.to_owned());
                    delivered += 1;
                }
                JobStatus::Idle => {
                    self.agents
                        .revive(&ctx.root_session_id, &agent.id, content)?;
                    delivered += 1;
                }
                JobStatus::Parked | JobStatus::Aborted | JobStatus::Failed => {}
            }
        }
        if delivered == 0 {
            return Err(HrnError::Tool(
                "no active (running or idle) agents in this session to broadcast to".to_owned(),
            ));
        }
        self.agents.inbox().wake();
        Ok(())
    }
}

impl UrlScheme for AgentScheme {
    fn scheme(&self) -> &'static str {
        "agent"
    }

    fn about(&self) -> &'static str {
        "background subagents of this session: bare `agent://` lists them, \
         `agent://<id>[.<child>][/<json-pointer>]` inspects output or structured JSON fields, \
         `write agent://<id>` steers a running agent or revives an idle/parked/aborted one, \
         `write agent://<id>?progress` records a progress note, and `write agent://all` \
         broadcasts (main agent only)"
    }

    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        match parse_agent_url(rest)? {
            AgentUrl::List => Ok(UrlBody::Text(listing(&self.agents, ctx))),
            AgentUrl::All => {
                if is_subagent_ctx(ctx) {
                    return Err(HrnError::Tool(
                        "`agent://all` is only allowed for the main agent, not subagents"
                            .to_owned(),
                    ));
                }
                Ok(UrlBody::Text(listing(&self.agents, ctx)))
            }
            AgentUrl::Target {
                id,
                json_path,
                progress,
            } => self.read_target(ctx, &id, &json_path, progress),
        }
    }

    fn write(&self, ctx: &ToolContext, rest: &str, content: &str) -> Result {
        match parse_agent_url(rest)? {
            AgentUrl::List => Err(malformed(rest, "write requires an agent id or `all`")),
            AgentUrl::All => self.broadcast(ctx, content),
            AgentUrl::Target {
                id,
                json_path,
                progress,
            } => {
                if !json_path.is_empty() {
                    return Err(HrnError::Tool(format!(
                        "JSON pointer paths (`agent://{id}/{}`) are read-only",
                        json_path.join("/")
                    )));
                }
                if content.trim().is_empty() {
                    return Err(HrnError::Tool(
                        "a message to an agent must not be empty".to_owned(),
                    ));
                }
                let agent = find(&self.agents, ctx, &id)?;
                if progress {
                    agent.add_progress_note(content.trim().to_owned());
                    self.agents.persist_meta(&agent);
                    self.agents.inbox().wake();
                    return Ok(());
                }
                match agent.snapshot().status {
                    JobStatus::Running => {
                        agent.control.push_steer(content.to_owned());
                        self.agents.inbox().wake();
                        Ok(())
                    }
                    JobStatus::Idle | JobStatus::Parked | JobStatus::Aborted => {
                        let _gen = self
                            .agents
                            .revive(&ctx.root_session_id, &agent.id, content)?;
                        Ok(())
                    }
                    JobStatus::Failed => Err(HrnError::Tool(format!(
                        "agent `{}` failed and cannot be revived; start a new one with `task`",
                        agent.id
                    ))),
                }
            }
        }
    }
}

#[cfg(test)]
mod agent_urls_inline_tests {
    use super::*;

    #[test]
    fn parses_full_agent_url_grammar_and_names_grammar_on_error() {
        assert_eq!(parse_agent_url("").unwrap(), AgentUrl::List);
        assert_eq!(parse_agent_url("/").unwrap(), AgentUrl::List);
        assert_eq!(parse_agent_url("all").unwrap(), AgentUrl::All);
        assert_eq!(
            parse_agent_url("lead").unwrap(),
            AgentUrl::Target {
                id: "lead".into(),
                json_path: vec![],
                progress: false,
            }
        );
        assert_eq!(
            parse_agent_url("lead.worker-2").unwrap(),
            AgentUrl::Target {
                id: "lead.worker-2".into(),
                json_path: vec![],
                progress: false,
            }
        );
        assert_eq!(
            parse_agent_url("lead.worker/reports/0/a~1b").unwrap(),
            AgentUrl::Target {
                id: "lead.worker".into(),
                json_path: vec!["reports".into(), "0".into(), "a/b".into()],
                progress: false,
            }
        );
        assert_eq!(
            parse_agent_url("lead.worker?progress").unwrap(),
            AgentUrl::Target {
                id: "lead.worker".into(),
                json_path: vec![],
                progress: true,
            }
        );
        for bad in [
            "lead..child",
            ".lead",
            "lead.",
            "lead/",
            "/lead",
            "lead//key",
            "lead?unknown",
            "lead?",
            "all?progress",
            "lead/key?progress",
            "bad id",
            "lead/..",
        ] {
            let err = parse_agent_url(bad).expect_err(bad).to_string();
            assert!(
                err.contains(AGENT_URL_GRAMMAR),
                "malformed `{bad}` error must name grammar, got: {err}"
            );
        }
    }
}
