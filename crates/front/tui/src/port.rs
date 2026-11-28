//! The contract between the TUI and the agent runtime the binary wires in: the [`Backend`] the
//! TUI calls, and the channel the backend reports through.

pub(crate) use crate::port_events::Message;
pub use crate::port_events::{
    ApprovalAsk, AskPrompt, Inbound, TaskEnd, TurnOutcome, UiEvent, UiReceiver, UiSender, channel,
};
pub use crate::port_git::CommitPreview;
pub use crate::port_hub::{
    AgentActivityItem, AgentActivityKind, AgentCounts, AgentDefRow, AgentHubEntry,
    AgentHubSnapshot, AgentHubStatus, PinnedAgentsMode, RestoreStart,
};
pub use crate::port_info::{
    ContextBreakdown, CustomCommandInfo, DoubleEscapeAction, ExternalSessionRow, JobInfo,
    LoginOutcome, LoginProviderChoice, LoginSubmission, McpResourceInfo, McpStatus, Notify,
    ToolInfo, WorktreeOutcome,
};
pub use crate::port_login::{LoginUiEvent, LogoutProviderChoice, Settings, SetupWizardInfo};
pub use crate::port_ui::{
    ModelInfo, RemoteCommandOutcome, RemoteSessionView, RoleAssignment, SettingItem, SettingKind,
    SettingUpdate,
};

use crate::port_info::{no_cfg, no_store, no_tool};
use hrn_kernel::{
    BtwTopic, DagNode, HrnError, ImageAttachment, Result, SessionMeta, ThinkingLevel, TodoPhase,
    TraceRecord, Usage,
};
use std::collections::HashMap;

/// The runtime as the TUI sees it. Calls return quickly: model work runs in the background and
/// reports through the [`UiSender`] the backend was built with, ending with
/// [`UiEvent::Finished`].
pub trait Backend: Send {
    /// Model aliases offered by `/model` and `ctrl+p`.
    fn model_aliases(&self) -> Vec<String>;

    /// Copy the backend's current session goal (and any preset-applied thinking level) into the
    /// app's settings, so the status band follows a `/goal` or `/modelpreset` change.
    fn pull_goal(&self, settings: &mut Settings);

    /// Run `/goal` with `arg` (`None`: report status; an objective: start goal mode; empty-ish
    /// special words per /goal). Returns the user-facing report; the app band follows the
    /// `goal` field of the settings afterwards.
    fn goal_command(&mut self, arg: Option<&str>) -> Result<String>;

    /// Run `/modelpreset save|switch|list <name>` (role bundles). Returns the report.
    fn modelpreset_command(&mut self, arg: Option<&str>) -> Result<String>;

    /// Check a model alias (or role name) and return the alias that serves it.
    fn resolve_model(&self, alias: &str) -> Result<String>;

    /// Apply changed settings (model/thinking on next turn; mode on next tool call).
    fn apply_settings(&mut self, settings: &Settings);

    /// Start a turn, creating the session on first use.
    fn submit(&mut self, prompt: &str) -> Result;

    /// Start a turn with attached images, creating the session on first use.
    fn submit_with_images(&mut self, prompt: &str, _images: Vec<ImageAttachment>) -> Result {
        self.submit(prompt)
    }

    /// Queue text for the running turn's next step boundary.
    fn steer(&mut self, text: &str);
    /// Cancel the running turn or compaction; history stays valid.
    fn interrupt(&mut self);
    /// Whether the main root session currently has a turn in flight.
    fn is_main_running(&self) -> bool {
        false
    }
    /// Prompt tokens of the active session's most recent LLM call, for status-bar context `%`.
    fn last_prompt_tokens(&self) -> u64 {
        0
    }
    /// Fold older history into a summary in the background (`Ok(false)`: no session yet).
    fn compact(&mut self, instructions: Option<&str>) -> Result<bool>;
    /// Aggressively elide large old tool outputs to `local://` without an LLM call (`/shake`).
    fn shake(&mut self) -> Result<bool> {
        Ok(false)
    }
    /// Whether `[compaction] auto_continue` is enabled.
    fn compaction_auto_continue(&self) -> bool {
        true
    }
    /// Summary text of the newest persisted compaction node on the current branch (`Ctrl+O`).
    fn latest_compaction_summary(&self) -> Option<String> {
        None
    }
    /// Forget the current session; the next prompt starts a new one.
    fn new_session(&mut self);
    /// Stored sessions, most recently updated first.
    fn sessions(&self) -> Result<Vec<SessionMeta>>;
    /// Make a stored session current; returns it with its active branch, oldest node first.
    fn resume(&mut self, session_id: &str) -> Result<(SessionMeta, Vec<DagNode>)>;
    /// Context window size (in tokens) for `model`, or `0` if unknown.
    fn context_window(&self, _model: &str) -> u64 {
        0
    }

    /// User-defined slash commands available in the current workspace.
    fn custom_commands(&self) -> Vec<CustomCommandInfo> {
        Vec::new()
    }

    /// Expand a custom `/name [args]` command into a prompt, or `None` if unknown.
    fn expand_custom_command(&self, _name: &str, _args: &str) -> Option<String> {
        None
    }

    /// Full DAG tree and current head node id of the active session, or `Ok(None)` if none yet.
    fn current_tree(&self) -> Result<Option<(Option<String>, Vec<DagNode>)>> {
        Ok(None)
    }

    /// Move the active session's branch head to `node_id`; returns updated metadata and branch.
    fn checkout_node(&mut self, _node_id: &str) -> Result<(SessionMeta, Vec<DagNode>)> {
        no_store("branch checkout")
    }

    /// Fork the active session at `node_id` (or head when `None`) into a new active session.
    fn fork_at(&mut self, _node_id: Option<&str>) -> Result<(SessionMeta, Vec<DagNode>)> {
        no_store("session fork")
    }

    /// Export the current session to `path` as markdown or html; returns the path written.
    fn export(&self, _path: Option<&str>) -> Result<String> {
        no_store("session export")
    }

    /// What the current session has used so far (zero before its first call).
    fn session_usage(&self) -> Result<Usage> {
        Ok(Usage::default())
    }

    /// Start reading the providers' usage limits; the report arrives as [`UiEvent::Report`].
    fn fetch_usage(&mut self) -> Result {
        no_cfg("provider usage")
    }

    /// Usage of the current session by model, then of all time, as text (for `/stats`).
    fn stats(&self) -> Result<String> {
        no_store("usage statistics")
    }

    /// Providers available for interactive `/login` setup.
    fn login_providers(&self) -> Vec<LoginProviderChoice> {
        Vec::new()
    }

    /// Persist credentials from `/login`, update config, and hot-reload the live router.
    fn save_login(&mut self, _submission: LoginSubmission) -> Result<LoginOutcome> {
        no_cfg("interactive /login")
    }

    /// Validate and save an API-key `/login` (`Ok(Some)` sync; `Ok(None)` async via [`UiEvent::Login`]).
    fn start_key_login(&mut self, submission: LoginSubmission) -> Result<Option<LoginOutcome>> {
        self.save_login(submission).map(Some)
    }

    /// Start an in-TUI OAuth PKCE / device-code flow in the background via [`UiEvent::Login`].
    fn start_login(&mut self, _provider: &str) -> Result {
        no_cfg("in-TUI OAuth login")
    }

    /// Deliver a pasted callback redirect URL or authorization code to an active OAuth flow.
    fn submit_login_code(&mut self, _input: &str) -> Result {
        Err(HrnError::Auth(
            "no active OAuth login waiting for a callback code".to_owned(),
        ))
    }

    /// Cancel an active in-TUI OAuth/device-code login flow.
    fn cancel_login(&mut self) {}

    /// Apply any hot-reloaded runtime produced by a completed background `/login` flow.
    fn finish_login(&mut self) {}

    /// Credential-bearing providers available to remove via `/logout`.
    fn logout_providers(&self) -> Vec<LogoutProviderChoice> {
        Vec::new()
    }

    /// Delete stored credentials for `provider` and hot-reload the live router.
    fn logout_provider(&mut self, _provider: &str) -> Result<String> {
        no_cfg("/logout")
    }

    /// Snapshot of current providers, models, and themes for the 3-step `/setup` wizard.
    fn setup_info(&self) -> SetupWizardInfo {
        SetupWizardInfo {
            model_aliases: self.model_aliases(),
            themes: crate::theme::available_theme_names(),
            current_theme: crate::theme::active_theme_name(),
            ..SetupWizardInfo::default()
        }
    }

    /// Persist `[tui] theme = <theme>` during `/setup`.
    fn apply_setup_theme(&mut self, theme: &str) -> Result<String> {
        Ok(format!("theme set to `{theme}`"))
    }

    /// Persist a theme selected via `/theme <name> save`.
    fn save_theme(&mut self, theme: &str) -> Result<String> {
        self.apply_setup_theme(theme)
    }

    /// Start a turn delivering pending background results (`Some(summary)` when started).
    fn start_inbox_turn(&mut self) -> Result<Option<String>> {
        Ok(None)
    }

    /// Non-secret summary of all configured providers and their auth health (for `/auth`).
    fn auth_status(&self) -> String {
        "auth status is not supported by this backend".to_owned()
    }

    /// The id of the session the UI is showing, when one exists yet.
    fn current_session_id(&self) -> Option<String> {
        None
    }

    /// Pin or unpin a stored session (the current one when `session_id` is `None`).
    fn set_pinned(&mut self, _session_id: Option<&str>, _pinned: bool) -> Result {
        no_store("session pinning")
    }

    /// Delete a stored session with its nodes and children.
    fn delete_session(&mut self, _session_id: &str) -> Result<bool> {
        no_store("session deletion")
    }

    /// Title a stored session (the current one when `session_id` is `None`).
    fn rename_session(&mut self, _session_id: Option<&str>, _title: &str) -> Result {
        no_store("session renaming")
    }

    /// Regenerate the active session's title with the title model (bare `/rename`).
    fn regenerate_title(&mut self) -> Result {
        no_store("title regeneration")
    }

    /// Discover external Claude Code / Codex CLI sessions for the `/sessions` import sub-view (`I`).
    fn discover_external_sessions(&self) -> Result<Vec<ExternalSessionRow>> {
        Ok(Vec::new())
    }

    /// Import one external Claude/Codex session into the store and resume it.
    fn import_external_session(
        &mut self,
        _source: &str,
        _id: &str,
    ) -> Result<(SessionMeta, Vec<DagNode>)> {
        no_store("session import")
    }

    /// The live context of the current session, by part (`/context`).
    fn context_breakdown(&self) -> ContextBreakdown {
        ContextBreakdown::default()
    }

    /// Configured MCP servers and their state (`/mcp`).
    fn mcp_status(&self) -> Vec<McpStatus> {
        Vec::new()
    }

    /// Every tool the session can call, active and deferred (`/tools`).
    fn tools(&self) -> Vec<ToolInfo> {
        Vec::new()
    }

    /// Background work of the current session tree, oldest first (`/jobs`).
    fn jobs(&self) -> Vec<JobInfo> {
        Vec::new()
    }

    /// The session's todo checklist, newest board on the branch (`/todo`).
    fn todos(&self) -> Vec<TodoPhase> {
        Vec::new()
    }

    /// Replace the session's todo checklist with `phases` (a `/todo` edit).
    /// # Errors
    /// There is no session yet or persistence failed.
    fn set_todos(&mut self, _phases: Vec<TodoPhase>) -> Result {
        no_store("todo edits")
    }

    /// Notification delivery the UI should use (`[tui] notify`).
    fn notify(&self) -> Notify {
        Notify::default()
    }

    /// The effective configuration as read-only text (`/settings`).
    fn settings_report(&self) -> String {
        "the effective configuration is not available in this backend".to_owned()
    }
    /// Clear session-scoped and persisted cross-session approval grants (`/forget-approvals`).
    fn forget_approvals(&mut self) -> Result {
        Ok(())
    }
    /// Ordered `(role, target)` pairs shown in the `/model` Roles Hub.
    fn role_assignments(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Rich model catalog with display names, providers, windows, and role tags (`/model`, `/switch`).
    fn model_catalog(&self) -> Vec<ModelInfo> {
        let roles = self.role_assignments();
        self.model_aliases()
            .into_iter()
            .map(|alias| {
                let assigned = roles
                    .iter()
                    .filter(|(_, target)| hrn_kernel::split_level_suffix(target).0 == alias)
                    .map(|(role, _)| role.clone())
                    .collect();
                ModelInfo {
                    name: ModelInfo::short_name_from_alias(&alias),
                    provider: ModelInfo::provider_from_alias(&alias),
                    context_window: self.context_window(&alias),
                    id: alias.clone(),
                    alias,
                    max_context_window: None,
                    reasoning: false,
                    thinking: None,
                    price: None,
                    roles: assigned,
                    tag: None,
                }
            })
            .collect()
    }
    /// Ordered role assignments tagged as core vs custom user roles for `/model`.
    fn role_rows(&self) -> Vec<RoleAssignment> {
        self.role_assignments()
            .into_iter()
            .map(|(role, target)| RoleAssignment {
                is_standard: matches!(
                    role.as_str(),
                    "default"
                        | "smol"
                        | "slow"
                        | "vision"
                        | "plan"
                        | "commit"
                        | "task"
                        | "tiny"
                        | "memory"
                        | "advisor"
                        | "title"
                        | "fast"
                        | "reviewer"
                        | "designer"
                        | "find"
                        | "judge"
                ),
                role,
                target,
            })
            .collect()
    }
    /// Interactive settings list for `/settings` (empty falls back to `settings_report`).
    fn settings_items(&self) -> Vec<SettingItem> {
        Vec::new()
    }
    /// Persist one setting change to global config and hot-reload runtime state.
    fn set_setting(&mut self, _key: &str, _value: &str) -> Result<SettingUpdate> {
        no_cfg("interactive /settings")
    }
    /// Remove a setting from global config so it falls back to its default/lower layer.
    fn reset_setting(&mut self, _key: &str) -> Result<SettingUpdate> {
        no_cfg("interactive /settings")
    }
    /// Persist a role assignment (`[roles.<role>] = target`) and reload the live router.
    fn assign_role(&mut self, _role: &str, _target: &str) -> Result<String> {
        no_cfg("role assignment")
    }
    /// `/switch` & `Alt+P`: session-only temporary switch without modifying `[roles]`.
    fn switch_model_temporary(
        &mut self,
        selector: &str,
    ) -> Result<(String, Option<ThinkingLevel>)> {
        let model = self.resolve_model(selector)?;
        let thinking = match hrn_kernel::split_level_suffix(selector).1 {
            hrn_kernel::LevelSuffix::Level(level) => Some(level),
            hrn_kernel::LevelSuffix::None | hrn_kernel::LevelSuffix::Inherit => None,
        };
        Ok((model, thinking))
    }
    /// Ordered `Ctrl+P` cycle entries `(label, resolved_alias, optional_thinking)`.
    fn cycle_entries(&self) -> Vec<(String, String, Option<ThinkingLevel>)> {
        self.model_aliases()
            .into_iter()
            .map(|a| (a.clone(), a, None))
            .collect()
    }
    /// Configured `[modelPresets.*]` names for the bare `/modelpreset` picker.
    fn model_presets(&self) -> Vec<String> {
        Vec::new()
    }
    /// True when focused on a subagent/hub session where the Roles Hub stays session-only.
    fn is_session_only_hub(&self) -> bool {
        false
    }
    /// Lifecycle labels (`complete | interrupted | aborted | error`) keyed by session id.
    fn session_lifecycles(&self) -> HashMap<String, String> {
        HashMap::new()
    }
    /// Start continuing an interrupted turn for the active session (`hrn chat --restore`).
    fn restore_active(&mut self) -> Result<RestoreStart> {
        Ok(RestoreStart::default())
    }
    /// Read-only lease warning for the active session when another process holds the write lock.
    fn lease_warning(&self) -> Option<String> {
        None
    }
    /// Generate and commit a `/handoff [focus]` brief in the background; `Ok(false)` if no session.
    fn handoff(&mut self, _focus: Option<&str>) -> Result<bool> {
        Ok(false)
    }
    /// Move the active session's branch head to `node_id` (or `None` to rewind past root).
    fn checkout_optional(
        &mut self,
        node_id: Option<&str>,
        _summarize: bool,
    ) -> Result<(SessionMeta, Vec<DagNode>)> {
        match node_id {
            Some(id) => self.checkout_node(id),
            None => no_store("rewinding past the root turn"),
        }
    }
    /// Attach or clear a user label on `node_id` (`Shift+L`, last write wins).
    fn set_node_label(&mut self, _node_id: &str, _label: Option<&str>) -> Result {
        Ok(())
    }
    /// Start a tool-free `/btw` side question on the active session; returns the topic id.
    fn start_btw(&mut self, _topic_id: Option<&str>, _question: &str) -> Result<String> {
        no_cfg("/btw")
    }
    /// Cancel an in-flight `/btw` stream without interrupting the main agent turn.
    fn cancel_btw(&mut self) {}
    /// Load persisted `/btw` topics for the active session (newest first).
    fn btw_topics(&self) -> Result<Vec<BtwTopic>> {
        Ok(Vec::new())
    }
    /// Persist a modified `/btw` topic (e.g. after marking `<inject>` as sent).
    fn save_btw_topic(&mut self, _topic: &BtwTopic) -> Result {
        Ok(())
    }
    /// Snapshot of all agents and recent activity in the current root session tree (`Alt+A`).
    fn agent_hub_snapshot(&self) -> AgentHubSnapshot {
        AgentHubSnapshot::default()
    }
    /// Focus the TUI on child agent `agent_id` (reviving it first if `parked` or `aborted`).
    fn focus_agent(&mut self, _agent_id: &str) -> Result<(SessionMeta, Vec<DagNode>)> {
        no_tool("focusing subagents")
    }
    /// Leave focused child agent view and return to the main root session, if one exists.
    fn unfocus_agent(&mut self) -> Result<Option<(SessionMeta, Vec<DagNode>)>> {
        Ok(None)
    }
    /// Send a steer (when running) or follow-up turn (when idle/revived) to focused child.
    fn steer_or_prompt_agent(&mut self, _agent_id: &str, _text: &str) -> Result<bool> {
        no_tool("steering subagents")
    }
    /// Revive a `parked` or `aborted` agent to `idle` (`r` in Agent Hub).
    fn revive_agent(&mut self, _agent_id: &str) -> Result<String> {
        no_tool("reviving subagents")
    }
    /// Abort and kill an agent (`x` in Agent Hub).
    fn abort_agent(&mut self, _agent_id: &str) -> Result<String> {
        no_tool("aborting subagents")
    }
    /// Discovered agent profile definitions with enabled/override state (`/agents`).
    fn agent_definitions(&self) -> Vec<AgentDefRow> {
        Vec::new()
    }
    /// Enable or disable a subagent profile under `[task] disabled_agents` (`/agents`).
    fn set_agent_enabled(&mut self, _name: &str, _enabled: bool) -> Result<String> {
        no_cfg("/agents persistence")
    }
    /// Set or clear a per-agent model override under `[task.agent_models]` (`/agents`).
    fn set_agent_model_override(&mut self, _name: &str, _model: Option<&str>) -> Result<String> {
        no_cfg("/agents persistence")
    }

    /// Whether `model` supports service tiers (`/fast`).
    fn supports_service_tier(&self, _model: &str) -> bool {
        false
    }
    /// Update the session-scoped service tier override (non-persisted).
    fn set_service_tier(&mut self, _tier: Option<hrn_kernel::Tier>) {}
    /// Whether Anthropic subscription slow mode is currently active.
    fn slow_mode(&self) -> bool {
        false
    }
    /// Toggle Anthropic subscription slow mode for the session (non-persisted).
    fn set_slow_mode(&mut self, _enabled: bool) {}
    /// Whether extended context (`max_context_window`) is currently active.
    fn extended_context(&self) -> bool {
        false
    }
    /// Toggle extended context for the session (non-persisted).
    fn set_extended_context(&mut self, _enabled: bool) {}
    /// Advertised MCP resources across configured servers (`/mcp resources`).
    fn mcp_resources(&self) -> Vec<McpResourceInfo> {
        Vec::new()
    }
    /// Directory storing the active session tree's `local://` artifacts (`Alt+G`), if any.
    fn local_artifacts_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// Seal and share the active session (`/share [--no-redact]`).
    fn share_session(&mut self, _no_redact: bool) -> Result<String> {
        no_cfg("/share")
    }
    /// Host or manage a live collaboration room (`/collab [view|status|stop]`).
    fn collab_command(&mut self, _arg: Option<&str>) -> Result<String> {
        no_cfg("/collab")
    }
    /// Join a live collaboration room as a guest (`/join <link>`).
    fn join_collab(&mut self, _link: &str) -> Result<String> {
        no_cfg("/join")
    }
    /// Leave the active collaboration room (`/leave`).
    fn leave_collab(&mut self) -> Result<String> {
        no_cfg("/leave")
    }
    /// True when attached to a live collaboration room as a view-only guest.
    fn collab_guest_read_only(&self) -> bool {
        false
    }
    /// Create a git worktree carrying uncommitted changes and relocate the live session (`/wt`).
    fn relocate_worktree(&mut self, _branch: Option<&str>) -> Result<WorktreeOutcome> {
        no_tool("git worktree relocation")
    }
    /// Execute a `/memory [view|stats|search|enqueue|clear]` command over `modules/memory`.
    fn memory_command(&mut self, _arg: Option<&str>) -> Result<String> {
        no_cfg("memory")
    }
    /// One-shot prompt submitted by `/init` to generate or refresh `AGENTS.md`.
    fn init_prompt(&self) -> String {
        "Inspect this repository and generate or refresh a concise, human-editable AGENTS.md \
         documenting real build/test commands, workspace layout, conventions, and gotchas."
            .to_owned()
    }
    /// Persisted trace spans for the active session (`/trace`).
    fn traces(&self) -> Result<Vec<TraceRecord>> {
        Ok(Vec::new())
    }
    /// Render the active session branch as a plain-text transcript (`/dump`).
    fn dump_transcript(&self) -> Result<String> {
        no_store("transcript dump")
    }
    /// Run a `/loop --while|--until` condition command asynchronously.
    fn start_loop_condition(&mut self, _check_id: u64, _command: &str) -> Result {
        no_tool("loop condition checks")
    }
    /// Cancel any running `/loop` condition command.
    fn cancel_loop_condition(&mut self) {}
    /// Prepare an attributed `/commit` preview (diff + generated message) without touching git.
    fn prepare_commit_preview(&mut self) -> Result<CommitPreview> {
        no_tool("/commit")
    }
    /// Stage attributed hunks (and optionally update `CHANGELOG.md`) and run `git commit`.
    fn execute_commit(&mut self, _message: &str, _changelog: bool) -> Result<String> {
        no_tool("/commit")
    }
    /// Generate a conventional commit message for an already-staged diff (used by `/git`).
    fn generate_commit_message_for_staged(&mut self, _staged_diff: &str) -> Result<String> {
        no_tool("commit message generation")
    }
    /// Run `/remote [view|lan|relay|status|stop]` for phone/browser QR pairing.
    fn remote_command(&mut self, _arg: Option<&str>) -> Result<RemoteCommandOutcome> {
        no_cfg("/remote")
    }
    /// Complete an asynchronous `/remote` or `/collab` startup after [`UiEvent::RemoteReady`].
    fn finish_remote(&mut self) -> Option<Result<RemoteCommandOutcome>> {
        None
    }
    /// Rotate the active `/remote` QR code or toggle control/view-only access.
    fn remote_rotate(&mut self, _control: bool) -> Result<RemoteSessionView> {
        no_cfg("/remote")
    }
    /// Revoke all remote pairings and stop the `/remote` server.
    fn remote_stop(&mut self) -> Result<String> {
        no_cfg("/remote")
    }
    /// Run `/agy [status|[flags] <prompt>]` via the optional `agy` CLI bridge module.
    fn agy_command(&mut self, _arg: Option<&str>) -> Result<String> {
        no_tool("/agy")
    }
}
