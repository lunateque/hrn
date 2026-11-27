//! Startup [`Options`] configuring a TUI [`super::App`] session.

use crate::port::{DoubleEscapeAction, PinnedAgentsMode, Settings};
use crate::port_theme::AppearanceConfig;
use hrn_kernel::{DagNode, SessionMeta};
use std::path::PathBuf;
use std::time::Duration;

/// How the TUI starts.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent startup and UI feature toggles"
)]
#[derive(Debug)]
pub struct Options {
    /// Initial model, thinking effort and permission mode.
    pub settings: Settings,
    /// Working directory shown in the status band.
    pub cwd: PathBuf,
    /// A session to show on start (`--resume`), with its active branch.
    pub resumed: Option<(SessionMeta, Vec<DagNode>)>,
    /// Up to 4 most recent sessions for `cwd`, displayed under the welcome banner.
    pub recent_sessions: Vec<SessionMeta>,
    /// An `ask` form nobody touches for this long submits its current values; `None` waits.
    pub ask_timeout: Option<Duration>,
    /// Optional legacy colour theme override: a built-in name, user theme name, or theme file path.
    pub theme: Option<String>,
    /// Appearance configuration (`[tui.theme]`, `[tui] symbol_preset`, `color_blind_mode`,
    /// `[tui.statusline]`, profile-aware `<data>/themes` directory, and profile tag).
    pub appearance: AppearanceConfig,
    /// A prompt to submit as soon as the UI is up (`--goal`: start working on the objective).
    pub initial_prompt: Option<String>,
    /// Optional status-bar flash message shown on startup (e.g. `--restore` status).
    pub initial_flash: Option<String>,
    /// Optional startup warning notices (e.g. read-only single-writer lease warning).
    pub initial_notices: Vec<String>,
    /// When `true`, immediately call [`crate::Backend::restore_active`] once the UI starts.
    pub start_restore: bool,
    /// Offer `No summary / Summarize` by default when switching branches abandons nodes (`[session] branch_summary`).
    pub branch_summary: bool,
    /// Action taken on double-`Esc` when the editor is empty (`[interaction] double_escape_action`).
    pub double_escape_action: DoubleEscapeAction,
    /// Show `<inject>…</inject>` rail and enable `app.btw.inject` (`[interaction] btw_inject`).
    pub btw_inject: bool,
    /// Pinned Subagents jump block mode above the editor (`[tui] pinned_agents`).
    pub pinned_agents: PinnedAgentsMode,
    /// Show active tool + elapsed preview line (> 5s) in the pinned jump block (`[tui] subagent_live_preview`).
    pub subagent_live_preview: bool,
    /// Enable minimal mouse capture for click-to-focus on pinned subagent rows (`[tui] mouse`).
    pub mouse: bool,
}
