//! Application state and how backend events change it. Keys live in `keys`, slash commands in
//! `commands`, painting in `ui`.

mod actions;
mod agent;
mod approvals;
mod asks;
mod background;
mod btw;
mod collab;
mod commands;
mod commands_ext;
mod compact;
mod git_commit;
mod hub;
mod keys;
mod login;
pub(crate) mod loopmode;
mod model_hub;
mod options;
mod overlay;
pub(crate) mod panels;
mod queue;
mod remote;
mod settings_hub;
mod theme_cmd;
mod tiers;
pub(crate) mod ui;

pub use options::Options;

use crate::agents_hub::AgentsHub;
use crate::ask::AskPanel;
use crate::btw::BtwPanel;
use crate::commit_modal::CommitModal;
use crate::git;
use crate::git_ui::GitWorkbench;
use crate::hub::AgentHub;
use crate::imgview::{ImageModal, ImageTray};
use crate::input::Editor;
use crate::links::LinkHintsOverlay;
use crate::local_picker::LocalPicker;
use crate::login::LoginView;
use crate::logout::LogoutView;
use crate::picker::Picker;
use crate::port::{
    ApprovalAsk, AskPrompt, Backend, DoubleEscapeAction, Inbound, Message, PinnedAgentsMode,
    Settings, UiReceiver, UiSender,
};
use crate::remote_qr::RemoteQrOverlay;
use crate::render::Ctx;
use crate::setup_wizard::SetupWizardView;
use crate::transcript::{Level, Transcript};
use crate::tree::TreeOverlay;
use crate::views;
use crate::vim::VimEngine;
use crossterm::event::{Event, KeyEvent};
use hrn_kernel::{PermissionMode, SessionMeta, ToolTier};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use ui::Banner;

const SPINNER_TICK: Duration = Duration::from_millis(80);
const FLASH_FOR: Duration = Duration::from_secs(3);
const QUIT_WINDOW: Duration = Duration::from_secs(2);
const ESC_DOUBLE_WINDOW: Duration = Duration::from_millis(900);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const ASK_COUNTDOWN: Duration = Duration::from_secs(1);

enum Task {
    Idle,
    Turn {
        /// `TurnStart` of the submitted prompt is still due; a later one is the backend's
        /// follow-up turn for steering that arrived after the last step.
        awaiting_start: bool,
    },
    Compact,
}

/// Completion popup state. Dismissing it (`esc`) hides it until the input changes.
#[derive(Default)]
struct Popup {
    selected: usize,
    dismissed_for: Option<String>,
}

/// The interactive session: feed it terminal and backend messages, draw it after each batch.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent UI toggles: verbose, quit, show_thinking, todo_editing, background.held"
)]
pub struct App {
    backend: Box<dyn Backend>,
    settings: Settings,
    cwd: PathBuf,
    cwd_label: String,
    branch: Option<String>,
    worktree: Option<git::Status>,
    git_tx: Option<UiSender>,
    transcript: Transcript,
    editor: Editor,
    task: Task,
    busy_since: Option<Instant>,
    turn_started: Option<Instant>,
    approvals: VecDeque<ApprovalAsk>,
    approval_dock: Option<crate::approvals::ApprovalDock>,
    always: BTreeMap<String, ToolTier>,
    clock: std::sync::Arc<dyn Fn() -> Instant + Send + Sync>,
    active_ask: Option<(AskPanel, AskPrompt)>,
    ask_timeout: Option<Duration>,
    ask_queue: VecDeque<AskPrompt>,
    picker: Option<Picker>,
    tree: Option<TreeOverlay>,
    login: Option<LoginView>,
    logout: Option<LogoutView>,
    setup: Option<SetupWizardView>,
    images: ImageTray,
    image_modal: Option<ImageModal>,
    pre_plan_mode: PermissionMode,
    popup: Popup,
    background: background::Background,
    verbose: bool,
    quit: bool,
    flow: ui::Flow,
    flash: Option<(String, Instant)>,
    quit_armed: Option<Instant>,
    esc_armed: Option<Instant>,
    title: Option<String>,
    tokens: (u64, u64),
    /// What the session has cost so far, in millionths of a dollar; `None` until a priced call.
    cost: Option<u64>,
    /// Reasoning tokens over the session, for providers that report them.
    reasoning_tokens: u64,
    last_prompt_tokens: u64,
    context_window: u64,
    frame: usize,
    last_tick: Instant,
    width: usize,
    /// Named-action key chords (embedded defaults plus `~/.harness/keybindings.toml`).
    keymap: crate::keymap::Keymap,
    /// `Ctrl+R`: the prompt-history search overlay.
    history: Option<crate::history::HistorySearch>,
    /// A slash-command page (`/hotkeys`, `/context`, …) or list.
    view: Option<views::View>,
    /// `/model` (Alt+M) Roles Hub, `Alt+P` temporary model picker, or `/modelpreset` picker.
    model_hub: Option<crate::modelhub::ModelHub>,
    /// Interactive tabbed `/settings` overlay.
    settings_ui: Option<crate::settings_ui::SettingsUi>,
    /// `Ctrl+T`: whether thinking blocks are expanded by default.
    show_thinking: bool,
    /// `Ctrl+Shift+O`: whether tool call cards are visible in the transcript.
    show_tools: bool,
    /// Modal Vim editing state machine (`[tui] vim_mode`).
    vim: VimEngine,
    /// `Alt+G`: inline picker when multiple `local://` artifacts exist.
    local_picker: Option<LocalPicker>,
    /// `Alt+O`: OSC-8 / URL link hint overlay.
    link_hints: Option<LinkHintsOverlay>,
    /// Injectable external editor/URL/clipboard hooks (overridden in tests).
    io: actions::IoHooks,
    /// The live todo checklist (`todo` tool, `/todo`), rendered as the HUD above the prompt.
    todo: Vec<hrn_kernel::TodoPhase>,
    /// Follow-ups queued via `/queue` or `Ctrl+Enter`; `Alt+Up` takes the newest back.
    followups: VecDeque<String>,
    /// Active `/loop` state machine (`Off`, `Active`, or `Paused`).
    loop_mode: loopmode::LoopState,
    /// Monotonic sequence id for asynchronous `/loop --while|--until` condition checks.
    loop_check_seq: u64,
    /// The prompt of the last turn, for `F5` / `/retry`.
    last_prompt: Option<String>,
    /// Desktop/title notifications.
    notify: crate::notify::Notifier,
    /// The session `/delete` waits to confirm; cleared by any other key or timeout.
    pending_delete: Option<String>,
    /// `/todo edit` is pending: the next send commits the checklist instead of prompting.
    todo_editing: bool,
    /// The area full views were drawn into last frame, for view key routing.
    overlay_area: ratatui::layout::Rect,
    /// Up to 4 recent sessions in `cwd`, shown in the landing banner.
    recent_sessions: Vec<SessionMeta>,
    /// Active `/btw` side-thread panel or topic history overlay.
    btw: Option<BtwPanel>,
    /// Whether a `/btw` side request is currently in flight (only one allowed at a time).
    btw_running: bool,
    /// `[session] branch_summary`: default-offer branch summarization when abandoning nodes.
    branch_summary: bool,
    /// `[interaction] double_escape_action` when the editor is empty.
    double_escape_action: DoubleEscapeAction,
    /// `[interaction] btw_inject`: enable `<inject>…</inject>` rail and `Alt+I` send-once.
    btw_inject: bool,
    /// Pending `/compact [instructions]` waiting for an interrupted in-flight turn to settle.
    pending_compact: Option<compact::PendingCompact>,
    /// Prompt to resume once compaction completes (`auto_continue = true`).
    resume_after_compact: Option<String>,
    /// Background speculative compaction is currently running (`AgentEvent::CompactionSpeculating`).
    speculating_compact: bool,
    /// Live Agent Hub overlay (`Alt+A` / double-`←`).
    agent_hub: Option<AgentHub>,
    /// `/agents` definition hub overlay.
    agents_hub: Option<AgentsHub>,
    /// Currently focused child agent id (when swapped into a subagent session).
    focused_agent: Option<String>,
    /// Main session settings stashed while focused on a child agent.
    saved_main_settings: Option<Settings>,
    /// Armed first `←` press on an empty editor for double-`←` gesture.
    left_armed: Option<Instant>,
    /// `[tui] pinned_agents` display mode.
    pinned_agents: PinnedAgentsMode,
    /// `[tui] subagent_live_preview` live tool preview flag.
    subagent_live_preview: bool,
    /// `[tui] mouse` click-to-focus enable flag.
    mouse: bool,
    /// Hitboxes `(live_y, agent_id)` recorded on the last frame for pinned jump block rows.
    pinned_hitboxes: Vec<(u16, String)>,
    /// Last frame's live-region top row on screen (for terminal-anchored mouse clicks).
    last_live_origin_y: u16,
    /// Active appearance configuration (theme slots, symbol preset, statusline preset, profile).
    appearance: crate::port_theme::AppearanceConfig,
    /// Optional deterministic epoch-ms clock override (used by VT render tests).
    now_ms_override: Option<i64>,
    /// Active `/commit` confirmation overlay.
    commit_modal: Option<CommitModal>,
    /// Active `/git [revision]` fullscreen workbench overlay.
    git_ui: Option<GitWorkbench>,
    remote_qr: Option<RemoteQrOverlay>,
}

impl App {
    /// A fresh app around `backend`.
    pub fn new(backend: Box<dyn Backend>, options: Options) -> Self {
        let mut app = Self {
            backend,
            settings: options.settings,
            cwd_label: git::display_path(&options.cwd),
            branch: git::branch(&options.cwd),
            worktree: None,
            git_tx: None,
            cwd: options.cwd,
            transcript: Transcript::default(),
            editor: Editor::default(),
            task: Task::Idle,
            busy_since: None,
            turn_started: None,
            approvals: VecDeque::new(),
            approval_dock: None,
            always: BTreeMap::new(),
            clock: std::sync::Arc::new(Instant::now),
            active_ask: None,
            ask_timeout: options.ask_timeout,
            ask_queue: VecDeque::new(),
            picker: None,
            tree: None,
            login: None,
            logout: None,
            setup: None,
            images: ImageTray::default(),
            image_modal: None,
            pre_plan_mode: PermissionMode::Default,
            popup: Popup::default(),
            background: background::Background::default(),
            verbose: false,
            quit: false,
            flow: ui::Flow::default(),
            flash: None,
            quit_armed: None,
            esc_armed: None,
            title: None,
            tokens: (0, 0),
            cost: None,
            reasoning_tokens: 0,
            last_prompt_tokens: 0,
            context_window: 0,
            frame: 0,
            last_tick: Instant::now(),
            width: 80,
            keymap: crate::keymap::Keymap::default(),
            history: None,
            view: None,
            model_hub: None,
            settings_ui: None,
            show_thinking: true,
            show_tools: true,
            vim: VimEngine::new(options.appearance.vim_mode),
            local_picker: None,
            link_hints: None,
            io: actions::IoHooks::default(),
            todo: Vec::new(),
            followups: VecDeque::new(),
            loop_mode: loopmode::LoopState::Off,
            loop_check_seq: 0,
            last_prompt: None,
            notify: crate::notify::Notifier::new(crate::port::Notify::default()),
            pending_delete: None,
            todo_editing: false,
            overlay_area: ratatui::layout::Rect::new(0, 0, 1, 1),
            recent_sessions: options.recent_sessions,
            btw: None,
            btw_running: false,
            branch_summary: options.branch_summary,
            double_escape_action: options.double_escape_action,
            btw_inject: options.btw_inject,
            pending_compact: None,
            resume_after_compact: None,
            speculating_compact: false,
            agent_hub: None,
            agents_hub: None,
            focused_agent: None,
            saved_main_settings: None,
            left_armed: None,
            pinned_agents: options.pinned_agents,
            subagent_live_preview: options.subagent_live_preview,
            mouse: options.mouse,
            pinned_hitboxes: Vec::new(),
            last_live_origin_y: 0,
            appearance: options.appearance,
            now_ms_override: None,
            commit_modal: None,
            git_ui: None,
            remote_qr: None,
        };
        app.init_startup(
            options.theme.as_deref(),
            options.resumed,
            options.initial_notices,
            options.initial_flash,
        );
        app
    }

    /// Continue an interrupted turn (and any interrupted subagents first) on startup (`--restore`).
    pub(crate) fn begin_restore(&mut self) {
        match self.backend.restore_active() {
            Ok(start) => {
                if start.main_started {
                    self.start_task(Task::Turn {
                        awaiting_start: false,
                    });
                }
                self.flash(start.flash_message());
                if start.subagents_started > 0 {
                    self.refresh_hub_snapshot();
                }
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// Apply one message from the UI channel.
    pub fn handle(&mut self, inbound: Inbound) {
        match inbound.0 {
            Message::Terminal(events) => self.on_terminal(events),
            Message::Backend(event) => self.on_backend(event),
            Message::Git(cwd, status) if cwd == self.cwd => self.worktree = status,
            Message::Git(..) => {}
        }
        self.deliver_inbox();
    }

    /// Probe the working tree now and after every task through `tx`.
    pub(crate) fn watch_git(&mut self, tx: UiSender) {
        self.git_tx = Some(tx);
        self.refresh_git();
    }

    fn refresh_git(&mut self) {
        self.branch = git::branch(&self.cwd);
        if let Some(tx) = &self.git_tx {
            git::probe(&self.cwd, tx.clone());
        }
    }

    /// Handle every message that arrives within `wait`; returns how many there were.
    pub fn pump(&mut self, rx: &UiReceiver, wait: Duration) -> usize {
        let deadline = Instant::now() + wait;
        let mut handled = 0;
        while let Some(inbound) = rx.recv(Some(deadline.saturating_duration_since(Instant::now())))
        {
            self.handle(inbound);
            handled += 1;
        }
        handled
    }

    /// Feed one key press, as if typed.
    pub fn press(&mut self, key: KeyEvent) {
        self.on_terminal(vec![Event::Key(key)]);
    }

    /// Feed pasted text.
    pub fn paste(&mut self, text: &str) {
        self.on_terminal(vec![Event::Paste(text.to_owned())]);
    }

    /// The user asked to leave.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// A turn or compaction is running.
    pub fn is_busy(&self) -> bool {
        !matches!(self.task, Task::Idle)
    }

    /// A tool call waits for the user's decision.
    pub fn awaiting_approval(&self) -> bool {
        !self.approvals.is_empty()
    }

    /// An `ask` form is waiting for the user's answers.
    pub fn awaiting_ask(&self) -> bool {
        self.active_ask.is_some()
    }

    /// Erase the screen and write the whole history again on the next frame; the transcript was
    /// replaced (`banner`: it starts with the welcome banner) or what is written changed.
    fn restart_screen(&mut self, banner: bool) {
        self.flow.banner = if banner { Banner::Due } else { Banner::Off };
        self.flow.replay = true;
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            width: self.width,
            verbose: self.verbose,
            frame: self.frame,
            now: Instant::now(),
        }
    }

    fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), Instant::now()));
    }

    fn notice(&mut self, level: Level, text: impl Into<String>) {
        self.transcript.notice(level, text, Instant::now());
    }

    /// Current monotonic instant (injectable in tests for deterministic guard timing).
    pub(super) fn now(&self) -> Instant {
        (self.clock)()
    }

    /// When the next animation frame or timeout is due; `None` sleeps until input.
    pub(crate) fn next_wake(&self, now: Instant) -> Option<Duration> {
        let mut wake: Option<Duration> = None;
        let mut consider = |at: Instant| {
            let left = at.saturating_duration_since(now);
            wake = Some(wake.map_or(left, |w| w.min(left)));
        };
        if self.is_busy() || self.speculating_compact {
            consider(self.last_tick + SPINNER_TICK);
        }
        if self
            .background
            .hub
            .agents
            .iter()
            .any(|a| !a.is_main && a.status == crate::port::AgentHubStatus::Running)
        {
            consider(now + Duration::from_secs(1));
        }
        if let Some(dock) = &self.approval_dock
            && dock.is_guarded(now)
        {
            consider(dock.guard_until());
        }
        if let Some((_, at)) = &self.flash {
            consider(*at + FLASH_FOR);
        }
        if let Some(at) = self.quit_armed {
            consider(at + QUIT_WINDOW);
        }
        if let Some(at) = self.esc_armed {
            consider(at + ESC_DOUBLE_WINDOW);
        }
        if let Some(at) = self.left_armed {
            consider(at + hub::LEFT_DOUBLE_WINDOW);
        }
        if let Some(at) = self
            .active_ask
            .as_ref()
            .and_then(|(panel, _)| panel.deadline())
        {
            // The countdown on screen ticks every second.
            consider(at.min(now + ASK_COUNTDOWN));
        }
        wake
    }

    /// Advance animations and expire transient messages; `true` when a repaint is due.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        let mut changed = self.expire_ask(now) || self.active_ask_counts_down();
        if (self.is_busy() || self.speculating_compact)
            && now.duration_since(self.last_tick) >= SPINNER_TICK
        {
            self.frame = self.frame.wrapping_add(1);
            self.last_tick = now;
            changed = true;
        }
        if let Some(started) = self.turn_started {
            self.notify
                .long_turn(started.elapsed().as_secs(), crate::notify::State::Working);
        }
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, at)| now.duration_since(*at) >= FLASH_FOR)
        {
            self.flash = None;
            changed = true;
        }
        if self
            .quit_armed
            .is_some_and(|at| now.duration_since(at) >= QUIT_WINDOW)
        {
            self.quit_armed = None;
            changed = true;
        }
        if self
            .esc_armed
            .is_some_and(|at| now.duration_since(at) >= ESC_DOUBLE_WINDOW)
        {
            self.esc_armed = None;
            changed = true;
        }
        if self
            .left_armed
            .is_some_and(|at| now.duration_since(at) >= hub::LEFT_DOUBLE_WINDOW)
        {
            self.left_armed = None;
            changed = true;
        }
        if self
            .background
            .hub
            .agents
            .iter()
            .any(|a| !a.is_main && a.status == crate::port::AgentHubStatus::Running)
        {
            changed = true;
        }
        changed
    }

    /// Stop background work before exit: interrupt it and give it a moment to settle.
    pub(crate) fn shutdown(&mut self, rx: &UiReceiver) {
        self.quit = true;
        self.backend.cancel_loop_condition();
        self.approvals.clear();
        self.approval_dock = None;
        self.active_ask = None;
        self.ask_queue.clear();
        if !self.is_busy() {
            return;
        }
        self.backend.interrupt();
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while self.is_busy() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if let Some(Inbound(Message::Backend(event))) = rx.recv(Some(left)) {
                self.on_backend(event);
            }
        }
    }
}
