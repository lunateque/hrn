//! Interactive `/settings` overlay lifecycle and live config delta application.

use super::App;
use crate::port::SettingUpdate;
use crate::settings_ui::{SettingsAction, SettingsUi};
use crossterm::event::KeyEvent;

impl App {
    /// Open the interactive `/settings` overlay, falling back to the read-only report when the
    /// backend does not expose structured settings items (e.g. minimal unit-test fakes).
    pub(super) fn open_settings(&mut self) {
        let items = self.backend.settings_items();
        if items.is_empty() {
            let text = self.backend.settings_report();
            self.open_view("settings", &text);
            return;
        }
        self.settings_ui = Some(SettingsUi::new(items));
    }

    pub(super) fn settings_ui_key(&mut self, key: KeyEvent) -> bool {
        let Some(ui) = &mut self.settings_ui else {
            return false;
        };
        let was_previewing = ui.is_previewing_theme();
        let action = ui.key(key);
        match action {
            SettingsAction::Stay => {}
            SettingsAction::Close => {
                self.settings_ui = None;
                if was_previewing {
                    self.apply_appearance(self.appearance.clone(), false);
                }
            }
            SettingsAction::PreviewTheme(Some(name)) => {
                let _ = crate::theme::install_or_default(Some(&name));
                self.flow.replay = true;
            }
            SettingsAction::PreviewTheme(None) => {
                self.apply_appearance(self.appearance.clone(), false);
            }
            SettingsAction::Set { key: k, value } => match self.backend.set_setting(&k, &value) {
                Ok(update) => self.finish_setting_change(update),
                Err(err) => {
                    if was_previewing {
                        self.apply_appearance(self.appearance.clone(), false);
                    }
                    if let Some(ui) = &mut self.settings_ui {
                        ui.set_status(err.to_string(), true);
                    }
                }
            },
            SettingsAction::Reset(k) => match self.backend.reset_setting(&k) {
                Ok(update) => self.finish_setting_change(update),
                Err(err) => {
                    if let Some(ui) = &mut self.settings_ui {
                        ui.set_status(err.to_string(), true);
                    }
                }
            },
        }
        true
    }

    fn finish_setting_change(&mut self, update: SettingUpdate) {
        let msg = update.message.clone();
        self.apply_setting_update(update);
        let items = self.backend.settings_items();
        if let Some(ui) = &mut self.settings_ui {
            ui.refresh(items, Some((msg, false)));
        }
    }

    fn apply_setting_update(&mut self, update: SettingUpdate) {
        if let Some(appearance) = update.appearance {
            self.apply_appearance(appearance, false);
        }
        let mut backend_changed = false;
        if let Some(thinking) = update.thinking {
            self.settings.thinking = thinking;
            backend_changed = true;
        }
        if let Some(mode) = update.approval_mode {
            self.settings.mode = mode;
            backend_changed = true;
        }
        if let Some(action) = update.double_escape_action {
            self.double_escape_action = action;
        }
        if let Some(pinned) = update.pinned_agents {
            self.pinned_agents = pinned;
        }
        if let Some(preview) = update.subagent_live_preview {
            self.subagent_live_preview = preview;
        }
        if let Some(mouse) = update.mouse {
            self.mouse = mouse;
        }
        if let Some(branch_summary) = update.branch_summary {
            self.branch_summary = branch_summary;
        }
        if let Some(btw_inject) = update.btw_inject {
            self.btw_inject = btw_inject;
        }
        if let Some(notify) = update.notify {
            self.notify = crate::notify::Notifier::new(notify);
        }
        if backend_changed {
            self.backend.apply_settings(&self.settings);
        }
    }
}
