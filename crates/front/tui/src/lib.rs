//! `hrn`'s interactive terminal UI.
//!
//! The crate knows the agent only through [`Backend`] (what the UI asks for) and the
//! [`UiEvent`] channel (what the runtime reports), both expressed in kernel types; the binary
//! wires the real runtime behind them. Rendering is synchronous: one thread reads the terminal,
//! the caller's thread runs the event loop, sleeps while nothing changes and paints at most one
//! frame per 16 ms, as a synchronized update. There is no alternate screen: finished transcript
//! lines are written once into the terminal's scrollback and only the live region below them is
//! repainted, row by row, where the bytes changed.

mod agents_hub;
mod app;
mod approvals;
mod ask;
mod btw;
mod card;
mod clipboard;
mod commit_modal;
mod diff;
mod exted;
mod git;
mod git_ui;
mod git_ui_ops;
mod highlight;
mod history;
mod hub;
mod hub_model;
mod hub_render;
mod imgview;
mod inline;
mod input;
mod keymap;
mod landing;
mod links;
mod local_picker;
mod login;
mod login_draw;
mod logout;
mod markdown;
mod modelhub;
mod modelhub_draw;
mod modelhub_draw_rows;
mod modelhub_ops;
mod notify;
mod picker;
mod pinned;
mod port;
mod port_events;
mod port_git;
mod port_hub;
mod port_info;
mod port_login;
mod port_theme;
mod port_ui;
mod remote_qr;
mod render;
mod settings_catalog;
mod settings_picker;
mod settings_picker_draw;
mod settings_ui;
mod settings_ui_draw;
mod setup_wizard;
mod slash;
mod status;
mod status_presets;
mod status_segments;
mod symbols;
mod table;
mod term;
mod theme;
mod tool;
mod tool_group;
mod toolfmt;
mod trace_view;
mod transcript;
mod tree;
mod tree_rows;
mod views;
mod vim;
mod vim_motion;
mod vim_normal;
mod vim_ops;
mod vim_textobj;
mod wrap;

pub use app::ui::Composed;
pub use app::{App, Options};
pub use git_ui::{
    BoxCommitMsgGenerator, CommitMsgGenerator, GitUiAction, GitWorkbench, run_git_workbench,
};
pub use git_ui_ops::{GitFileEntry, find_repo_root, validate_revision};
pub use port::{
    AgentActivityItem, AgentActivityKind, AgentCounts, AgentDefRow, AgentHubEntry,
    AgentHubSnapshot, AgentHubStatus, ApprovalAsk, AskPrompt, Backend, CommitPreview,
    ContextBreakdown, CustomCommandInfo, DoubleEscapeAction, ExternalSessionRow, Inbound, JobInfo,
    LoginOutcome, LoginProviderChoice, LoginSubmission, LoginUiEvent, LogoutProviderChoice,
    McpResourceInfo, McpStatus, ModelInfo, Notify, PinnedAgentsMode, RemoteCommandOutcome,
    RemoteSessionView, RestoreStart, RoleAssignment, SettingItem, SettingKind, SettingUpdate,
    Settings, SetupWizardInfo, TaskEnd, ToolInfo, TurnOutcome, UiEvent, UiReceiver, UiSender,
    WorktreeOutcome, channel,
};
pub use port_theme::{
    AppearanceConfig, SeparatorStyle, StatuslineConfig, StatuslinePreset, SymbolPreset,
    TerminalTone, ThemeConfig, ThemeMode, VimModeDisplay,
};
pub use symbols::{preset as symbol_preset, set_preset as set_symbol_preset};
pub use theme::{
    ThemeEntry, ThemeWatcher, active_theme_name, active_theme_path, available_theme_names,
    available_theme_names_in, available_themes, color_blind_mode, discover_in, is_known_theme,
    reload_active_theme, set_color_blind_mode, set_themes_dir, themes_dir,
};
pub use vim::CursorShape;

use inline::{Frame, Inline};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

/// The shortest time between two frames.
const FRAME: Duration = Duration::from_millis(16);
/// How long the output must stay quiet before the live region is written once more.
const NUDGE: Duration = Duration::from_millis(150);
/// Messages applied before a frame is due, so a flood of deltas cannot starve painting.
const BATCH: usize = 512;

/// Run the TUI on the current terminal until the user quits. A running turn is interrupted
/// before this returns; the terminal is restored even when drawing fails.
///
/// # Errors
/// The terminal could not be set up, drawn on or restored.
pub fn run(backend: Box<dyn Backend>, rx: &UiReceiver, options: Options) -> io::Result<()> {
    let initial = options.initial_prompt.clone();
    let start_restore = options.start_restore;
    let mouse = options.mouse;
    let mut app = App::new(backend, options);
    app.watch_git(rx.sender());
    if start_restore {
        app.begin_restore();
    } else if let Some(prompt) = initial {
        app.send_prompt(&prompt, None);
    }
    term::enter(mouse)?;
    let mut screen = Inline::new(io::stdout(), theme::truecolor());
    let result =
        term::spawn_input(rx.sender(), mouse).and_then(|()| event_loop(&mut screen, &mut app, rx));
    app.shutdown(rx);
    let cleared = screen.finish();
    let restored = term::leave();
    result.and(cleared).and(restored)
}

fn event_loop(screen: &mut Inline<Stdout>, app: &mut App, rx: &UiReceiver) -> io::Result<()> {
    let mut dirty = true;
    let mut last_draw: Option<Instant> = None;
    let mut nudge_at: Option<Instant> = None;
    loop {
        let now = Instant::now();
        if !dirty && nudge_at.is_some_and(|at| now >= at) {
            screen.nudge()?;
            nudge_at = None;
        }
        if dirty && last_draw.is_none_or(|at| now.duration_since(at) >= FRAME) {
            let (width, height) = term::size()?;
            let frame = app.compose(width, height);
            let wrote = screen.paint(&Frame {
                reset: frame.reset,
                scrollback: &frame.scrollback,
                live: &frame.live,
                cursor: frame.cursor,
                cursor_shape: frame.cursor_shape,
            })?;
            if wrote {
                nudge_at = Some(now + NUDGE);
            }
            last_draw = Some(now);
            dirty = false;
        }
        if app.should_quit() {
            return Ok(());
        }
        let mut wait = app.next_wake(Instant::now());
        if let Some(at) = nudge_at {
            let left = at.saturating_duration_since(Instant::now());
            wait = Some(wait.map_or(left, |w| w.min(left)));
        }
        if dirty {
            let left = last_draw.map_or(Duration::ZERO, |at| FRAME.saturating_sub(at.elapsed()));
            wait = Some(wait.map_or(left, |w| w.min(left)));
        }
        if let Some(inbound) = rx.recv(wait) {
            app.handle(inbound);
            for _ in 1..BATCH {
                let Some(more) = rx.try_recv() else { break };
                app.handle(more);
            }
            dirty = true;
        }
        dirty |= app.tick(Instant::now());
    }
}
