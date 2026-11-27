//! `hrn` / `hrn chat`: the interactive TUI driving the real runtime. The TUI runs on a blocking
//! thread; turns and compactions run as tokio tasks and report through the UI channel.

use crate::config::Config;
use crate::config_watch::LiveSettingsPort;
use crate::session_lock::SessionLease;
use crate::wiring::Background;
use hrn_edge_llm::LlmConfig;
use hrn_edge_mcp::McpToolSet;
use hrn_edge_store::SqliteStore;
use hrn_harness::{AgentRuntime, CustomCommand, SessionControl};

#[path = "chat_skills.rs"]
mod chat_skills;
#[path = "chat_usage.rs"]
mod chat_usage;
use hrn_kernel::{CancelFlag, SessionStore};
use hrn_tui::{Settings, UiSender};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;

mod agents;
mod backend_impl;
mod btw;
pub(crate) mod chat_ui;
mod collab;
mod commands_ext;
mod compact;
mod git_commit;
mod goalcmd;
mod hub;
mod login;
mod reload;
mod remote;
mod remote_async;
mod restore;
mod roles;
mod session;
mod settings_ui;
mod tiers;
mod usage;
mod worktree_cmd;

pub(crate) use reload::run;

/// How often a pending approval re-checks the turn's cancellation.
const CANCEL_POLL: Duration = Duration::from_millis(100);
/// Sessions offered by `/sessions`.
const SESSION_LIST: usize = 200;

/// The runtime behind the TUI: one current session, created on the first prompt.
pub(crate) struct ChatBackend {
    pub(super) runtime: AgentRuntime,
    /// Agents, jobs and services; outlives a runtime rebuild.
    pub(super) background: Background,
    sqlite: Arc<SqliteStore>,
    pub(crate) store: Arc<dyn SessionStore>,
    pub(super) cfg: Config,
    /// Model names as omp spells them, for `/stats`.
    model_names: crate::stats::ModelNames,
    pub(super) config_path: PathBuf,
    mcp_tools: McpToolSet,
    pub(super) llm: LlmConfig,
    pub(super) handle: Handle,
    tx: UiSender,
    pub(super) cwd: PathBuf,
    pub(super) settings: Settings,
    pub(super) session: Option<String>,
    pub(super) control: SessionControl,
    cancel: Option<CancelFlag>,
    /// The current session id, shared so `run` can print how to resume it after exit.
    last: Arc<Mutex<Option<String>>>,
    /// Discovered `commands/*.md` slash commands for `cwd`.
    custom_commands: BTreeMap<String, CustomCommand>,
    /// Startup records per configured MCP server, for `/mcp`.
    pub(super) mcp_statuses: Vec<hrn_edge_mcp::McpServerStatus>,
    /// Live config reload slot populated by the background file watcher.
    reload: reload::ReloadSlot,
    /// Active in-TUI OAuth/device-code login paste channel and cancel flag.
    login_state: login::ActiveLogin,
    /// Validated config produced by an async in-TUI login task, applied on `finish_login`.
    login_reload: reload::ReloadSlot,
    /// Directory storing `<id>.lock` single-writer leases (`None` for `--no-session`).
    lock_dir: Option<PathBuf>,
    /// Active single-writer or read-only lease for `self.session`.
    lease: Option<SessionLease>,
    /// Independent cancellation handle for an active `/btw` side question.
    btw_cancel: Option<CancelFlag>,
    /// Independent cancellation handle for an active `/loop` condition command.
    pub(super) loop_cancel: Option<CancelFlag>,
    /// Single-flight guard ensuring at most one `/btw` runs at a time.
    btw_active: Arc<std::sync::atomic::AtomicBool>,
    /// Shared handle for the background idle-compaction watcher.
    pub(super) idle: crate::idle_compact::ChatIdleHandle,
    /// Currently focused child agent id (when swapped into a subagent view from Agent Hub).
    pub(super) focused_agent: Option<String>,
    /// Main session settings stashed while focused on a child agent.
    pub(super) saved_main_settings: Option<Settings>,
    /// True when `--no-title` or `--no-session` forces auto-titling off across config reloads.
    pub(super) no_title_forced: bool,
    /// Active live collaboration host room, if hosting.
    pub(super) collab_host: Option<crate::collab::CollabHostBridge>,
    /// Zero-config embedded relay + optional HTTPS tunnel backing `/collab` and `/remote relay`.
    pub(super) auto_relay: Option<hrn_edge_ipc::relay::AutoRelay>,
    /// Active live collaboration guest connection, if joined.
    pub(super) collab_guest: Option<hrn_edge_ipc::collab::CollabGuest>,
    /// Live `/remote` mobile supervision hub and optional LAN PWA server.
    pub(super) remote: remote::ChatRemoteState,
    /// Layered settings handle backing interactive `/settings`.
    pub(super) settings_port: Option<Arc<LiveSettingsPort>>,
}

#[cfg(test)]
#[path = "login_tests.rs"]
mod login_tests;
#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
