//! `/theme [name] [save]` slash command and live appearance / theme-watcher reload handling.

use super::App;
use crate::port_theme::AppearanceConfig;
use crate::symbols;
use crate::theme;
use crate::transcript::Level;
use std::fmt::Write as _;

/// Parse `<name> [save]` into `(name, should_save)`.
fn parse_theme_arg(raw: &str) -> (&str, bool) {
    let trimmed = raw.trim();
    if let Some((prefix, last)) = trimmed.rsplit_once(char::is_whitespace)
        && last.eq_ignore_ascii_case("save")
        && !prefix.trim().is_empty()
    {
        return (prefix.trim(), true);
    }
    (trimmed, false)
}

impl App {
    /// Apply startup backend state, keymap, theme/symbol settings, resumed session, and initial notices/flash.
    pub(super) fn init_startup(
        &mut self,
        legacy_theme: Option<&str>,
        resumed: Option<(hrn_kernel::SessionMeta, Vec<hrn_kernel::DagNode>)>,
        initial_notices: Vec<String>,
        initial_flash: Option<String>,
    ) {
        self.context_window = self.backend.context_window(&self.settings.model);
        self.notify = crate::notify::Notifier::new(self.backend.notify());
        self.todo = self.backend.todos();
        let (keymap, mut notices) =
            crate::keymap::Keymap::load_files(&self.appearance.keymap_paths);
        self.keymap = keymap;
        notices.extend(initial_notices);
        theme::set_themes_dir(self.appearance.themes_dir.clone());
        theme::set_color_blind_mode(self.appearance.color_blind_mode);
        symbols::set_preset(self.appearance.symbol_preset);
        let theme_problem = match legacy_theme {
            Some(spec) => theme::install_or_default(Some(spec)),
            None => theme::install_config(&self.appearance.theme),
        };
        if let Some(problem) = theme_problem {
            self.notice(Level::Warn, problem);
        }
        if let Some((meta, nodes)) = resumed {
            self.show_session(&meta, &nodes);
        }
        for warn in notices {
            self.notice(Level::Warn, warn);
        }
        if let Some(flash) = initial_flash {
            self.flash(flash);
        }
    }

    /// `/theme`: list built-in and user themes; `/theme <name>`: live preview;
    /// `/theme <name> save`: apply live and persist to `config.toml`.
    pub(super) fn handle_theme(&mut self, raw: Option<&str>) {
        let Some(arg) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            self.list_themes();
            return;
        };
        if arg.eq_ignore_ascii_case("list") {
            self.list_themes();
            return;
        }
        let (name, save) = parse_theme_arg(arg);
        if let Some(problem) = theme::install_or_default(Some(name)) {
            self.flow.replay = true;
            self.notice(
                Level::Warn,
                format!("{problem} (fell back to default theme)"),
            );
            return;
        }
        let active = theme::active_theme_name();
        self.flow.replay = true;
        if save {
            match self.backend.save_theme(name) {
                Ok(msg) => {
                    self.flash(format!("theme → {active} (saved)"));
                    self.notice(Level::Info, msg);
                }
                Err(err) => self.notice(Level::Error, err.to_string()),
            }
        } else {
            self.flash(format!(
                "theme → {active} (preview · `/theme {name} save` to persist)"
            ));
        }
    }

    fn list_themes(&mut self) {
        let active = theme::active_theme_name();
        let entries = theme::available_themes();
        let mut built_ins = Vec::new();
        let mut custom = Vec::new();
        for entry in entries {
            let label = if entry.name == active {
                format!("{} (active)", entry.name)
            } else {
                entry.name
            };
            if entry.built_in {
                built_ins.push(label);
            } else {
                custom.push(label);
            }
        }
        let mut out = format!("active theme: {active}\nbuilt-in: {}", built_ins.join(", "));
        if !custom.is_empty() {
            let _ = write!(out, "\nuser: {}", custom.join(", "));
        } else if let Some(dir) = theme::themes_dir() {
            let _ = write!(out, "\nuser: (none in {})", dir.display());
        }
        out.push_str("\nusage: /theme <name> (live preview) · /theme <name> save (persist)");
        self.notice(Level::Info, out);
    }

    /// Apply a freshly loaded [`AppearanceConfig`] (from startup or live config watcher reload).
    pub(super) fn apply_appearance(&mut self, cfg: AppearanceConfig, announce: bool) {
        theme::set_themes_dir(cfg.themes_dir.clone());
        theme::set_color_blind_mode(cfg.color_blind_mode);
        symbols::set_preset(cfg.symbol_preset);
        let problem = theme::install_config(&cfg.theme);
        let keymap_paths_changed = self.appearance.keymap_paths != cfg.keymap_paths;
        self.vim.set_enabled(cfg.vim_mode);
        self.appearance = cfg;
        if keymap_paths_changed {
            self.reload_keymap(false);
        }
        self.flow.replay = true;
        if let Some(err) = problem {
            self.notice(Level::Warn, err);
        } else if announce {
            self.flash(format!(
                "appearance reloaded · {}",
                theme::active_theme_name()
            ));
        }
    }

    /// Handle a live custom theme file reload outcome from the background watcher.
    pub(super) fn on_theme_reloaded(&mut self, outcome: std::result::Result<String, String>) {
        match outcome {
            Ok(name) => {
                self.flow.replay = true;
                self.flash(format!("theme reloaded · {name}"));
            }
            Err(err) => {
                self.notice(
                    Level::Warn,
                    format!("theme reload failed (kept previous palette): {err}"),
                );
            }
        }
    }
}
