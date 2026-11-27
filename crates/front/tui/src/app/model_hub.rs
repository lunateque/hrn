//! Roles Hub (`/model`, `Alt+M`), searchable Temporary Picker (`/switch`, `Alt+P`), and Preset
//! Picker (`/modelpreset`) orchestration on [`App`].

use super::App;
use crate::modelhub::{HubAction, ModelHub};
use crate::port::{ModelInfo, RoleAssignment};
use crate::transcript::Level;
use crossterm::event::KeyEvent;

impl App {
    /// Gather rich model catalog rows, falling back to synthesized metadata from `model_aliases()`.
    fn load_model_catalog(&self) -> Vec<ModelInfo> {
        let catalog = self.backend.model_catalog();
        if !catalog.is_empty() {
            return catalog;
        }
        let roles = self.load_role_rows();
        self.backend
            .model_aliases()
            .into_iter()
            .map(|alias| {
                let assigned = roles
                    .iter()
                    .filter(|r| hrn_kernel::split_level_suffix(&r.target).0 == alias)
                    .map(|r| r.role.clone())
                    .collect();
                ModelInfo {
                    name: ModelInfo::short_name_from_alias(&alias),
                    provider: ModelInfo::provider_from_alias(&alias),
                    id: alias.clone(),
                    context_window: self.backend.context_window(&alias),
                    max_context_window: None,
                    reasoning: false,
                    thinking: None,
                    price: None,
                    roles: assigned,
                    tag: None,
                    alias,
                }
            })
            .collect()
    }

    /// Gather structured role rows, falling back to `role_assignments()` on minimal backends.
    fn load_role_rows(&self) -> Vec<RoleAssignment> {
        let rows = self.backend.role_rows();
        if !rows.is_empty() {
            return rows;
        }
        self.backend
            .role_assignments()
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
                ),
                role,
                target,
            })
            .collect()
    }

    /// Open the `/model` / `Alt+M` tabbed Roles & Models Hub overlay.
    pub(super) fn open_roles_hub(&mut self) {
        let roles = self.load_role_rows();
        let models = self.load_model_catalog();
        let presets = self.backend.model_presets();
        if roles.is_empty() && models.is_empty() {
            self.notice(Level::Info, "no models configured");
            return;
        }
        let session_only = self.backend.is_session_only_hub();
        self.model_hub = Some(ModelHub::roles_hub(
            roles,
            models,
            presets,
            &self.settings.model,
            self.settings.thinking,
            session_only,
        ));
    }

    /// Open the searchable `/switch` / `Alt+P` (`app.model.selectTemporary`) temporary model picker.
    pub(super) fn open_temporary_picker(&mut self, initial_query: Option<&str>) {
        let models = self.load_model_catalog();
        if models.is_empty() {
            self.notice(Level::Info, "no models configured");
            return;
        }
        self.model_hub = Some(ModelHub::temporary_picker(
            models,
            &self.settings.model,
            self.settings.thinking,
            initial_query,
        ));
    }

    /// `/switch [query]`: bare opens the searchable temporary picker; with an argument switches
    /// immediately if it resolves cleanly, otherwise opens the picker pre-filtered by `<query>`.
    pub(super) fn handle_switch(&mut self, selector: Option<&str>) {
        let Some(raw) = selector.map(str::trim).filter(|s| !s.is_empty()) else {
            self.open_temporary_picker(None);
            return;
        };
        if self.try_temporary_switch(raw).is_err() {
            let models = self.load_model_catalog();
            if models.is_empty() {
                self.notice(Level::Warn, format!("unknown model `{raw}`"));
            } else {
                self.model_hub = Some(ModelHub::temporary_picker(
                    models,
                    &self.settings.model,
                    self.settings.thinking,
                    Some(raw),
                ));
            }
        }
    }

    fn try_temporary_switch(&mut self, selector: &str) -> Result<(), String> {
        match self.backend.switch_model_temporary(selector) {
            Ok((model, thinking)) => {
                self.context_window = self.backend.context_window(&model);
                self.settings.model.clone_from(&model);
                if let Some(level) = thinking {
                    self.settings.thinking = level;
                }
                self.backend.apply_settings(&self.settings);
                let note = if self.is_busy() {
                    " · from the next turn"
                } else {
                    ""
                };
                self.flash(format!("model → {model} (session-only){note}"));
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }

    pub(super) fn apply_temporary_switch(&mut self, selector: &str) {
        if let Err(err) = self.try_temporary_switch(selector) {
            self.notice(Level::Warn, err);
        }
    }

    /// Bare `/modelpreset` opens a picker when presets exist, else prints the list/help message.
    pub(super) fn open_preset_picker_or_list(&mut self) {
        let presets = self.backend.model_presets();
        if presets.is_empty() {
            self.handle_modelpreset(Some("list"));
            return;
        }
        self.model_hub = Some(ModelHub::preset_picker(
            presets,
            &self.settings.model,
            self.settings.thinking,
        ));
    }

    /// Route one keystroke into the open [`ModelHub`]; returns `true` when consumed.
    pub(super) fn model_hub_key(&mut self, key: KeyEvent) -> bool {
        let Some(hub) = &mut self.model_hub else {
            return false;
        };
        match hub.key(key, self.overlay_area) {
            HubAction::Stay => {}
            HubAction::Close => self.model_hub = None,
            HubAction::AssignRole {
                role,
                target,
                keep_open,
            } => {
                let outcome = self.assign_role_from_hub(&role, &target);
                if keep_open {
                    let roles = self.load_role_rows();
                    let models = self.load_model_catalog();
                    let presets = self.backend.model_presets();
                    let status = match outcome {
                        Ok(msg) => Some((msg, false)),
                        Err(err) => Some((err, true)),
                    };
                    if let Some(hub) = &mut self.model_hub {
                        hub.refresh(
                            roles,
                            models,
                            presets,
                            &self.settings.model,
                            self.settings.thinking,
                            status,
                        );
                    }
                } else {
                    self.model_hub = None;
                    match outcome {
                        Ok(report) => self.flash(report),
                        Err(err) => self.notice(Level::Error, err),
                    }
                }
            }
            HubAction::SwitchTemporary(alias) => {
                self.model_hub = None;
                self.apply_temporary_switch(&alias);
            }
            HubAction::SavePreset(name) => {
                self.model_hub = None;
                self.handle_modelpreset(Some(&format!("save {name}")));
            }
            HubAction::SwitchPreset(name) => {
                self.model_hub = None;
                self.handle_modelpreset(Some(&format!("switch {name}")));
            }
        }
        true
    }

    fn assign_role_from_hub(&mut self, role: &str, target: &str) -> Result<String, String> {
        match self.backend.assign_role(role, target) {
            Ok(report) => {
                if role == "default"
                    && let Ok(resolved) = self.backend.resolve_model(target)
                {
                    self.context_window = self.backend.context_window(&resolved);
                    self.settings.model = resolved;
                    if let hrn_kernel::LevelSuffix::Level(lvl) =
                        hrn_kernel::split_level_suffix(target).1
                    {
                        self.settings.thinking = lvl;
                    }
                    self.backend.apply_settings(&self.settings);
                }
                Ok(report)
            }
            Err(err) => Err(err.to_string()),
        }
    }

    pub(super) fn cycle_model(&mut self, forward: bool) {
        let entries = self.backend.cycle_entries();
        if entries.is_empty() {
            return;
        }
        let len = entries.len();
        let at = entries
            .iter()
            .position(|(_, alias, _)| *alias == self.settings.model)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        if let Some((label, alias, thinking)) = entries.get(next) {
            alias.clone_into(&mut self.settings.model);
            if let Some(level) = thinking {
                self.settings.thinking = *level;
            }
            self.context_window = self.backend.context_window(&self.settings.model);
            self.backend.apply_settings(&self.settings);
            let tag = if label == alias {
                alias.clone()
            } else {
                format!("{label} → {alias}")
            };
            self.flash(format!("model → {tag}"));
        }
    }
}
