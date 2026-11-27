//! Slash commands and the settings hotkeys.

use super::App;
use crate::imgview::{self, ImageModal};
use crate::picker::Picker;
use crate::slash::{ALL_MODES, Command, MODES};
use crate::transcript::Level;
use crate::tree::TreeMode;
use hrn_kernel::{PermissionMode, ThinkingLevel};
use std::path::Path;

// The full ladder; `auto` classifies per prompt and `max` is the top budget.
const THINKING_CYCLE: [ThinkingLevel; 8] = [
    ThinkingLevel::Auto,
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::Xhigh,
    ThinkingLevel::Max,
];

const HELP: &str = "\
keys (remappable in ~/.harness/keybindings.toml · /hotkeys shows all)
  enter            send · while the agent works: steer it at the next step
  shift+enter      newline (also alt+enter, ctrl+j)
  esc              interrupt the running turn
  esc esc          clear input (if typing) or open session tree (rewind/fork)
  ctrl+c ctrl+c    quit
  ctrl+r           search prompt history
  ctrl+g           edit the draft in $VISUAL / $EDITOR (notepad on Windows)
  alt+g            open a session local:// artifact in $VISUAL / $EDITOR
  ctrl+enter       queue a follow-up for after the turn · alt+up takes it back
  shift+tab        cycle thinking effort
  ctrl+t           show/hide thinking blocks
  ctrl+y           cycle permission mode: default → auto-edit → yolo
  ctrl+p           cycle role/model · shift+ctrl+p backwards · alt+m roles hub · alt+p temp switch
  alt+a / ← ←      open live Agent Hub (or ← ← / esc to return from focused subagent)
  alt+shift+p      toggle plan mode
  f5               retry the last turn
  alt+shift+c      copy the last answer to the clipboard
  ctrl+o           expand every card and reasoning block
  ctrl+shift+o     show/hide tool call cards
  alt+o            open link hint overlay for links in the transcript
  ctrl+l           redraw the screen
  ctrl+h           the hotkeys screen
  alt+v / ctrl+v   paste clipboard image ([Image #N]) or text (alt+v on Windows Terminal)
  ctrl+shift+v     paste plain text from the clipboard (also alt+shift+v)
  wheel / scrollbar  the transcript is the terminal's own scrollback: scroll, select and copy there
  up down          prompt history
approvals
  y once · a always for this tool this session · n refuse · esc interrupt
commands
  /login [provider|url]  /logout [provider]  /setup  /auth  /model [alias]  /switch [alias]  /modelpreset [save|switch|delete|list]  /think <level>  /fast [ultra|off]  /slow [on|off]  /extended-context [on|off|status]  /mode <mode>  /plan [on|off|<prompt>]
  /advisor [on|off|model <alias>]  /attach <path>  /image <N>
  /tree  /fork [node]  /wt [branch]  /commit  /git [rev]  /review [staged|base|<focus>]  /vibe [on|off|status]  /new  /sessions  /compact [instructions]  /shake  /export [path]  /dump [path]  /trace [export <file>]  /stats  /usage  /quit
  /share [--no-redact]  /collab [view|status|stop]  /join <link>  /leave  /remote [view|lan|relay|status|stop]
  /queue [msg]  /loop [count|dur] [--while|--until '<cmd>'] [prompt]  /memory [view|stats|search|enqueue|clear]  /init
  /settings  /theme [name] [save]  /context  /tools  /mcp  /jobs  /agents  /agy [status|<prompt>]  /todo [edit]  /hotkeys
  /pin [session]  /unpin [session]  /delete [session]  /rename [title]  /retry  /copy
  /skill:<name> [args] — load a discovered SKILL.md into the conversation";

fn next_in<T: Copy + PartialEq>(cycle: &[T], current: T) -> Option<T> {
    let at = cycle.iter().position(|item| *item == current);
    let next = at.map_or(0, |i| (i + 1) % cycle.len());
    cycle.get(next).copied()
}

impl App {
    pub(super) fn run_command(&mut self, command: Command) {
        if let Some(msg) = self.streaming_guard(&command) {
            self.notice(Level::Warn, msg);
            return;
        }
        match command {
            Command::Login(arg) => self.handle_login(arg.as_deref()),
            Command::Logout(arg) => self.handle_logout(arg.as_deref()),
            Command::Setup => self.handle_setup(),
            Command::Auth => self.notice(Level::Info, self.backend.auth_status()),
            Command::Model(alias) => self.set_model(alias.as_deref()),
            Command::Switch(alias) => self.handle_switch(alias.as_deref()),
            Command::Think(level) => self.set_thinking(level.as_deref()),
            Command::Fast(arg) => self.handle_fast(arg.as_deref()),
            Command::Slow(arg) => self.handle_slow(arg.as_deref()),
            Command::ExtendedContext(arg) => self.handle_extended_context(arg.as_deref()),
            Command::Mode(mode) => self.set_mode(mode.as_deref()),
            Command::ForgetApprovals => self.handle_forget_approvals(),
            Command::Plan(arg) => self.handle_plan(arg),
            Command::Advisor(arg) => self.handle_advisor(arg.as_deref()),
            Command::Attach(path) => self.handle_attach(path.as_deref()),
            Command::Image(num) => self.handle_image_preview(num.as_deref()),
            Command::Export(path) => match self.backend.export(path.as_deref()) {
                Ok(written) => self.notice(Level::Info, format!("exported to {written}")),
                Err(err) => self.notice(Level::Error, err.to_string()),
            },
            Command::Share(arg) => self.handle_share(arg.as_deref()),
            Command::Collab(arg) => self.handle_collab(arg.as_deref()),
            Command::Join(arg) => self.handle_join(arg.as_deref()),
            Command::Leave => self.handle_leave(),
            Command::Stats => match self.backend.stats() {
                Ok(text) => self.notice(Level::Info, text),
                Err(err) => self.notice(Level::Error, err.to_string()),
            },
            Command::Usage => match self.backend.fetch_usage() {
                Ok(()) => self.flash("reading provider limits…"),
                Err(err) => self.notice(Level::Error, err.to_string()),
            },
            Command::Help => self.notice(Level::Info, HELP),
            Command::Quit => self.quit = true,
            Command::Settings => self.open_settings(),
            Command::Context => {
                let text = self.context_report();
                self.open_view("context", &text);
            }
            Command::Tools => {
                let text = self.tools_report();
                self.open_view("tools", &text);
            }
            Command::Mcp(arg) => self.handle_mcp(arg.as_deref()),
            Command::Jobs => {
                let text = self.jobs_report();
                self.open_view("jobs", &text);
            }
            Command::Agents => self.open_agents_hub(),
            Command::Hotkeys => self.open_hotkeys(),
            Command::Todo(arg) => self.handle_todo(arg.as_deref()),
            Command::Pin(session) => self.toggle_pin(session.as_deref(), true),
            Command::Unpin(session) => self.toggle_pin(session.as_deref(), false),
            Command::Delete(session) => self.delete_session_prompt(session.as_deref()),
            Command::Rename(title) => self.handle_rename(title.as_deref()),
            Command::Retry => self.retry_last_turn(),
            Command::Copy => self.copy_last_answer(),
            Command::Queue(arg) => self.handle_queue(arg.as_deref()),
            Command::Loop(arg) => self.handle_loop(arg.as_deref()),
            Command::Wt(branch) => self.handle_wt(branch.as_deref()),
            Command::Memory(arg) => self.handle_memory(arg.as_deref()),
            Command::Init => self.handle_init(),
            Command::Trace(arg) => self.handle_trace(arg.as_deref()),
            Command::Dump(path) => self.handle_dump(path.as_deref()),
            Command::Tree => self.open_tree_mode(TreeMode::Tree),
            Command::Branch => self.open_tree_mode(TreeMode::Branch),
            Command::Handoff(focus) => self.handle_handoff(focus.as_deref()),
            Command::Btw(arg) => self.handle_btw(arg.as_deref()),
            Command::Fork(node) => self.fork_session(node.as_deref()),
            Command::New => self.reset_to_new_session(),
            Command::Sessions => self.open_sessions_picker(),
            Command::Goal(arg) => self.handle_goal(arg.as_deref()),
            Command::Modelpreset(arg) => self.handle_modelpreset(arg.as_deref()),
            Command::Theme(arg) => self.handle_theme(arg.as_deref()),
            Command::Commit(arg) => self.handle_commit(arg.as_deref()),
            Command::Git(rev) => self.handle_git(rev.as_deref()),
            Command::Remote(arg) => self.handle_remote(arg.as_deref()),
            Command::Review(arg) => self.handle_review(arg.as_deref()),
            Command::Vibe(arg) => self.handle_vibe(arg.as_deref()),
            Command::Agy(arg) => self.handle_agy(arg.as_deref()),
            Command::Compact(instructions) => self.handle_compact(instructions.as_deref()),
            Command::Shake => self.handle_shake(),
        }
    }

    /// `/forget-approvals`: clear all session and persistent approval grants.
    fn handle_forget_approvals(&mut self) {
        self.always.clear();
        match self.backend.forget_approvals() {
            Ok(()) => self.flash("cleared all remembered and session approval grants"),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// `/new`: clear local transcript/counters and start a fresh session.
    fn reset_to_new_session(&mut self) {
        self.backend.new_session();
        self.backend.cancel_loop_condition();
        self.loop_mode = super::loopmode::LoopState::Off;
        self.followups.clear();
        self.transcript.clear();
        self.images.clear();
        self.image_modal = None;
        self.restart_screen(true);
        self.title = None;
        self.always.clear();
        self.tokens = (0, 0);
        self.cost = None;
        self.reasoning_tokens = 0;
        self.last_prompt_tokens = 0;
        self.flash("new session");
    }

    fn open_sessions_picker(&mut self) {
        match self.backend.sessions() {
            Ok(sessions) => {
                let lifecycles = self.backend.session_lifecycles();
                self.picker = Some(Picker::with_lifecycles(sessions, lifecycles));
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }
    /// `/goal [args]`, then re-read the stored session's goal so the band follows.
    fn handle_goal(&mut self, arg: Option<&str>) {
        match self.backend.goal_command(arg) {
            Ok(text) => self.notice(Level::Info, text),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
        self.backend.pull_goal(&mut self.settings);
    }

    /// `/modelpreset [save|switch|delete|list …]`.
    pub(super) fn handle_modelpreset(&mut self, arg: Option<&str>) {
        if arg.map(str::trim).is_none_or(str::is_empty) {
            self.open_preset_picker_or_list();
            return;
        }
        match self.backend.modelpreset_command(arg) {
            Ok(text) => {
                self.backend.pull_goal(&mut self.settings);
                self.context_window = self.backend.context_window(&self.settings.model);
                self.notice(Level::Info, text);
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    pub(super) fn resume(&mut self, id: &str) {
        match self.backend.resume(id) {
            Ok((meta, nodes)) => {
                self.show_session(&meta, &nodes);
                self.backend.apply_settings(&self.settings);
                if let Some(warn) = self.backend.lease_warning() {
                    self.notice(Level::Warn, warn);
                }
                let title = self.title.clone().unwrap_or_else(|| "session".to_owned());
                self.flash(format!("resumed · {title}"));
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    pub(super) fn open_tree(&mut self) {
        self.open_tree_mode(TreeMode::Tree);
    }

    pub(super) fn fork_session(&mut self, node_id: Option<&str>) {
        match self.backend.fork_at(node_id) {
            Ok((meta, nodes)) => {
                self.show_session(&meta, &nodes);
                self.backend.apply_settings(&self.settings);
                let title = self.title.clone().unwrap_or_else(|| "session".to_owned());
                self.flash(format!("forked · {title}"));
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    pub(super) fn enter_plan_mode(&mut self) {
        if self.settings.mode != PermissionMode::Plan {
            self.pre_plan_mode = self.settings.mode;
            self.settings.mode = PermissionMode::Plan;
            self.backend.apply_settings(&self.settings);
        }
        self.flash("plan mode · read-only (mutating tools blocked)");
    }

    pub(super) fn exit_plan_mode(&mut self) {
        let target = if self.pre_plan_mode == PermissionMode::Plan {
            PermissionMode::Default
        } else {
            self.pre_plan_mode
        };
        self.settings.mode = target;
        self.backend.apply_settings(&self.settings);
        self.settle_approvals();
        self.flash(format!("plan mode off → {}", target.as_str()));
    }

    pub(super) fn handle_plan(&mut self, arg: Option<String>) {
        match arg {
            None => {
                if self.settings.mode == PermissionMode::Plan {
                    self.exit_plan_mode();
                } else {
                    self.enter_plan_mode();
                }
            }
            Some(ref s) if s.eq_ignore_ascii_case("on") => self.enter_plan_mode(),
            Some(ref s) if s.eq_ignore_ascii_case("off") => self.exit_plan_mode(),
            Some(prompt) => {
                self.enter_plan_mode();
                self.send_prompt(&prompt, None);
            }
        }
    }

    fn handle_advisor(&mut self, raw: Option<&str>) {
        let Some(arg) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            self.settings.advisor = !self.settings.advisor;
            self.backend.apply_settings(&self.settings);
            let label = self.advisor_label();
            self.flash(format!("advisor → {label}"));
            return;
        };
        if arg.eq_ignore_ascii_case("on") {
            self.settings.advisor = true;
            self.backend.apply_settings(&self.settings);
            let label = self.advisor_label();
            self.flash(format!("advisor → {label}"));
            return;
        }
        if arg.eq_ignore_ascii_case("off") {
            self.settings.advisor = false;
            self.backend.apply_settings(&self.settings);
            self.flash("advisor → off");
            return;
        }
        if let Some(rest) = arg.strip_prefix("model") {
            let alias = rest.trim();
            if alias.is_empty() {
                let all = self.backend.model_aliases().join(", ");
                self.notice(
                    Level::Info,
                    format!(
                        "advisor: {} · available models: {all}",
                        self.advisor_label()
                    ),
                );
                return;
            }
            match self.backend.resolve_model(alias) {
                Ok(model) => {
                    self.settings.advisor = true;
                    self.settings.advisor_model = Some(model.clone());
                    self.backend.apply_settings(&self.settings);
                    self.flash(format!("advisor → on ({model})"));
                }
                Err(err) => self.notice(Level::Warn, err.to_string()),
            }
            return;
        }
        self.notice(
            Level::Info,
            format!(
                "advisor: {} · usage: /advisor [on|off|model <alias>]",
                self.advisor_label()
            ),
        );
    }

    fn advisor_label(&self) -> String {
        if !self.settings.advisor {
            return "off".to_owned();
        }
        match self.settings.advisor_model.as_deref() {
            Some(m) => format!("on ({m})"),
            None => "on (auto)".to_owned(),
        }
    }

    pub(super) fn attach_image_path(&mut self, path: &Path) {
        match imgview::load_image_file(path) {
            Ok(att) => {
                let dims = match (att.width, att.height) {
                    (Some(w), Some(h)) => format!(" ({w}×{h})"),
                    _ => String::new(),
                };
                let id = self.images.stage(att);
                self.editor.insert(&format!("[Image #{id}] "));
                self.flash(format!("attached [Image #{id}]{dims}"));
            }
            Err(err) => self.notice(Level::Error, err),
        }
    }

    pub(super) fn paste_clipboard(&mut self) {
        if let Some(res) = imgview::read_clipboard_image() {
            match res {
                Ok(att) => {
                    let dims = match (att.width, att.height) {
                        (Some(w), Some(h)) => format!(" ({w}×{h})"),
                        _ => String::new(),
                    };
                    let id = self.images.stage(att);
                    self.editor.insert(&format!("[Image #{id}] "));
                    self.flash(format!("pasted [Image #{id}]{dims}"));
                }
                Err(err) => self.notice(Level::Error, err),
            }
            return;
        }
        if let Some(text) = imgview::read_clipboard_text() {
            if let Some(path) = imgview::unquote_image_path(&text, &self.cwd) {
                self.attach_image_path(&path);
            } else {
                self.editor.insert(&text);
            }
        } else {
            self.flash("clipboard has no image or text");
        }
    }

    fn handle_attach(&mut self, raw: Option<&str>) {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            self.notice(
                Level::Info,
                "usage: /attach <path-to-image> (or press ctrl+v / drop an image path)",
            );
            return;
        };
        let path = imgview::unquote_image_path(raw, &self.cwd).unwrap_or_else(|| {
            let p = Path::new(raw);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.cwd.join(p)
            }
        });
        self.attach_image_path(&path);
    }

    fn handle_image_preview(&mut self, raw: Option<&str>) {
        let parsed = raw
            .map(str::trim)
            .map(|s| s.trim_start_matches('#'))
            .and_then(|s| s.parse::<usize>().ok());
        let Some(id) = parsed else {
            self.notice(Level::Info, "usage: /image <N> (e.g. /image 1)");
            return;
        };
        if let Some(att) = self.images.get(id).cloned() {
            self.image_modal = Some(ImageModal::new(id, att));
        } else {
            self.notice(Level::Warn, format!("no [Image #{id}] in this session"));
        }
    }

    fn apply(&mut self, what: &str) {
        self.backend.apply_settings(&self.settings);
        let note = if self.is_busy() && what != "mode" {
            " · from the next turn"
        } else {
            ""
        };
        let value = match what {
            "model" => self.settings.model.clone(),
            "thinking" => self.settings.thinking.as_str().to_owned(),
            _ => self.settings.mode.as_str().to_owned(),
        };
        self.flash(format!("{what} → {value}{note}"));
    }

    pub(super) fn set_model(&mut self, alias: Option<&str>) {
        let Some(alias) = alias else {
            self.open_roles_hub();
            return;
        };
        match self.backend.resolve_model(alias) {
            Ok(model) => {
                self.context_window = self.backend.context_window(&model);
                self.settings.model = model;
                // the `model:level` suffix switches the model and the level at once.
                if let hrn_kernel::LevelSuffix::Level(level) =
                    hrn_kernel::split_level_suffix(alias).1
                {
                    self.settings.thinking = level;
                }
                self.apply("model");
            }
            Err(err) => self.notice(Level::Warn, err.to_string()),
        }
    }

    fn set_thinking(&mut self, level: Option<&str>) {
        let Some(raw) = level else {
            let current = self.settings.thinking.as_str();
            self.notice(
                Level::Info,
                format!("thinking: {current} · auto off minimal low medium high xhigh max"),
            );
            return;
        };
        match ThinkingLevel::parse(raw) {
            Some(level) => {
                self.settings.thinking = level;
                self.apply("thinking");
            }
            None => self.notice(Level::Warn, format!("unknown thinking level `{raw}`")),
        }
    }

    fn set_mode(&mut self, mode: Option<&str>) {
        let Some(raw) = mode else {
            let current = self.settings.mode.as_str();
            self.notice(
                Level::Info,
                format!("mode: {current} · default auto-edit yolo plan"),
            );
            return;
        };
        match PermissionMode::parse(raw).filter(|m| ALL_MODES.contains(m)) {
            Some(mode) => {
                if mode == PermissionMode::Plan && self.settings.mode != PermissionMode::Plan {
                    self.pre_plan_mode = self.settings.mode;
                }
                self.settings.mode = mode;
                self.apply("mode");
                self.settle_approvals();
            }
            None => self.notice(Level::Warn, format!("unknown mode `{raw}`")),
        }
    }

    pub(super) fn cycle_thinking(&mut self) {
        if let Some(level) = next_in(&THINKING_CYCLE, self.settings.thinking) {
            self.settings.thinking = level;
            self.apply("thinking");
        }
    }

    pub(super) fn cycle_mode(&mut self) {
        if let Some(mode) = next_in(&MODES, self.settings.mode) {
            self.settings.mode = mode;
            self.apply("mode");
            self.settle_approvals();
        }
    }
}
