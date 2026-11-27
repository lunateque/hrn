//! Searchable `/switch` (`Alt+P`) temporary model picker, tabbed `/model` (`Alt+M`) Roles &
//! Models Hub, and `/modelpreset` picker state and key routing.

use crate::modelhub_draw;
use crate::modelhub_ops::{collect_providers, keep_visible, role_matches, score_model};
use crate::port::{ModelInfo, RoleAssignment};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hrn_kernel::ThinkingLevel;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};

/// Outcome of feeding a keystroke into [`ModelHub`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HubAction {
    Stay,
    Close,
    /// Persist `role = target`; `keep_open` refreshes the hub in place with an inline toast.
    AssignRole {
        role: String,
        target: String,
        keep_open: bool,
    },
    /// Session-only temporary switch (`/switch`, `Alt+P`, or `Alt+Enter`/`Ctrl+T`).
    SwitchTemporary(String),
    /// Save the current effective roles as a named preset (`Ctrl+S` in Roles Hub).
    SavePreset(String),
    /// Switch to an existing named preset.
    SwitchPreset(String),
}

/// Top-level overlay mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HubKind {
    TemporaryPicker,
    RolesHub,
    PresetPicker,
}

/// Active tab inside [`HubKind::RolesHub`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HubTab {
    Models,
    Roles,
    Presets,
}

impl HubTab {
    const ALL: [Self; 3] = [Self::Models, Self::Roles, Self::Presets];

    fn step(self, forward: bool) -> Self {
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        let next = if forward {
            (idx + 1) % Self::ALL.len()
        } else {
            (idx + Self::ALL.len() - 1) % Self::ALL.len()
        };
        Self::ALL.get(next).copied().unwrap_or(Self::Models)
    }
}

/// Flattened display row inside the Roles tab.
#[derive(Clone, Debug)]
pub(crate) enum RoleRow {
    Header(&'static str),
    Item(RoleAssignment),
}

impl RoleRow {
    pub(crate) const fn is_selectable(&self) -> bool {
        matches!(self, Self::Item(_))
    }
}

/// Nested sub-view inside [`ModelHub`].
#[derive(Clone, Debug)]
pub(crate) enum SubMode {
    Main,
    PickModelForRole {
        role: String,
        query: String,
        provider_idx: usize,
        selected: usize,
        top: usize,
    },
    SavePresetName {
        draft: String,
    },
}

/// Interactive overlay serving `/model`, `/switch`, `Alt+M`, `Alt+P`, and `/modelpreset`.
pub(crate) struct ModelHub {
    pub(crate) kind: HubKind,
    pub(crate) tab: HubTab,
    pub(crate) models: Vec<ModelInfo>,
    pub(crate) providers: Vec<String>,
    pub(crate) roles: Vec<RoleAssignment>,
    pub(crate) presets: Vec<String>,
    pub(crate) current_model: String,
    pub(crate) current_thinking: ThinkingLevel,
    pub(crate) session_only: bool,
    pub(crate) query: String,
    pub(crate) provider_idx: usize,
    pub(crate) selected: usize,
    pub(crate) top: usize,
    pub(crate) sub: SubMode,
    /// Inline feedback toast `(message, is_error)` displayed in the bottom inspector.
    pub(crate) status: Option<(String, bool)>,
}

impl ModelHub {
    /// Open the `/model` / `Alt+M` tabbed Roles & Models Hub.
    pub(crate) fn roles_hub(
        roles: Vec<RoleAssignment>,
        models: Vec<ModelInfo>,
        presets: Vec<String>,
        current_model: &str,
        current_thinking: ThinkingLevel,
        session_only: bool,
    ) -> Self {
        let providers = collect_providers(&models);
        let tab = if models.is_empty() && !roles.is_empty() {
            HubTab::Roles
        } else {
            HubTab::Models
        };
        let mut hub = Self {
            kind: HubKind::RolesHub,
            tab,
            models,
            providers,
            roles,
            presets,
            current_model: current_model.to_owned(),
            current_thinking,
            session_only,
            query: String::new(),
            provider_idx: 0,
            selected: 0,
            top: 0,
            sub: SubMode::Main,
            status: None,
        };
        hub.select_initial_row();
        hub
    }

    /// Open the searchable `/switch` / `Alt+P` temporary model picker (never mutates `[roles]`).
    pub(crate) fn temporary_picker(
        models: Vec<ModelInfo>,
        current_model: &str,
        current_thinking: ThinkingLevel,
        initial_query: Option<&str>,
    ) -> Self {
        let providers = collect_providers(&models);
        let query = initial_query.map(str::trim).unwrap_or_default().to_owned();
        let mut hub = Self {
            kind: HubKind::TemporaryPicker,
            tab: HubTab::Models,
            models,
            providers,
            roles: Vec::new(),
            presets: Vec::new(),
            current_model: current_model.to_owned(),
            current_thinking,
            session_only: true,
            query,
            provider_idx: 0,
            selected: 0,
            top: 0,
            sub: SubMode::Main,
            status: None,
        };
        hub.select_initial_row();
        hub
    }

    /// Open the bare `/modelpreset` searchable preset picker.
    pub(crate) fn preset_picker(
        presets: Vec<String>,
        current_model: &str,
        current_thinking: ThinkingLevel,
    ) -> Self {
        Self {
            kind: HubKind::PresetPicker,
            tab: HubTab::Presets,
            models: Vec::new(),
            providers: vec!["All".to_owned()],
            roles: Vec::new(),
            presets,
            current_model: current_model.to_owned(),
            current_thinking,
            session_only: false,
            query: String::new(),
            provider_idx: 0,
            selected: 0,
            top: 0,
            sub: SubMode::Main,
            status: None,
        }
    }

    /// Refresh catalog, role, and preset snapshots in place after an inline mutation.
    pub(crate) fn refresh(
        &mut self,
        roles: Vec<RoleAssignment>,
        models: Vec<ModelInfo>,
        presets: Vec<String>,
        current_model: &str,
        current_thinking: ThinkingLevel,
        status: Option<(String, bool)>,
    ) {
        let active_provider = self.providers.get(self.provider_idx).cloned();
        self.providers = collect_providers(&models);
        self.provider_idx = active_provider
            .and_then(|p| self.providers.iter().position(|cand| cand == &p))
            .unwrap_or(0);
        self.roles = roles;
        self.models = models;
        self.presets = presets;
        current_model.clone_into(&mut self.current_model);
        self.current_thinking = current_thinking;
        self.status = status;
        self.clamp_selection();
    }

    /// Filter and rank models matching `query` and `provider_idx`.
    pub(crate) fn filtered_models(&self, query: &str, provider_idx: usize) -> Vec<&ModelInfo> {
        let prov = (provider_idx > 0)
            .then(|| self.providers.get(provider_idx).map(String::as_str))
            .flatten();
        let tokens: Vec<String> = query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        let mut scored: Vec<(u32, usize, &ModelInfo)> = self
            .models
            .iter()
            .enumerate()
            .filter(|(_, m)| prov.is_none_or(|p| m.provider == p))
            .filter_map(|(idx, m)| {
                let score = score_model(&tokens, m)?;
                Some((score, idx, m))
            })
            .collect();
        if !tokens.is_empty() {
            scored.sort_by_key(|(score, idx, _)| (*score, *idx));
        }
        scored.into_iter().map(|(_, _, m)| m).collect()
    }

    /// Filtered role rows split into `Core Roles` and `Custom Roles` sections.
    pub(crate) fn filtered_role_rows(&self) -> Vec<RoleRow> {
        let tokens: Vec<String> = self
            .query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        let mut core = Vec::new();
        let mut custom = Vec::new();
        for row in &self.roles {
            if !role_matches(&tokens, row, &self.models) {
                continue;
            }
            if row.is_standard {
                core.push(row.clone());
            } else {
                custom.push(row.clone());
            }
        }
        let mut out = Vec::new();
        if !core.is_empty() {
            out.push(RoleRow::Header("Core Roles"));
            out.extend(core.into_iter().map(RoleRow::Item));
        }
        if !custom.is_empty() {
            out.push(RoleRow::Header("Custom Roles"));
            out.extend(custom.into_iter().map(RoleRow::Item));
        }
        out
    }

    /// Filtered preset names matching `self.query`.
    pub(crate) fn filtered_presets(&self) -> Vec<&str> {
        let tokens: Vec<String> = self
            .query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        self.presets
            .iter()
            .map(String::as_str)
            .filter(|name| {
                let low = name.to_ascii_lowercase();
                tokens.iter().all(|tok| low.contains(tok))
            })
            .collect()
    }

    /// Resolve a role target alias to its rich catalog entry when present.
    pub(crate) fn find_model(&self, target: &str) -> Option<&ModelInfo> {
        let (base, _) = hrn_kernel::split_level_suffix(target.trim());
        self.models.iter().find(|m| m.alias == base || m.id == base)
    }

    /// Handle one terminal keystroke inside the overlay.
    pub(crate) fn key(&mut self, key: KeyEvent, area: Rect) -> HubAction {
        let visible = modelhub_draw::list_rows(self, area);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return HubAction::Close;
        }
        self.status = None;
        match &self.sub {
            SubMode::SavePresetName { .. } => self.key_save_preset(key),
            SubMode::PickModelForRole { .. } => self.key_pick_for_role(key, visible),
            SubMode::Main => self.key_main(key, visible),
        }
    }

    /// Render the overlay and return the terminal cursor position when a text input is active.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        modelhub_draw::draw(self, area, buf)
    }

    fn key_save_preset(&mut self, key: KeyEvent) -> HubAction {
        let SubMode::SavePresetName { draft } = &mut self.sub else {
            return HubAction::Stay;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.sub = SubMode::Main;
                HubAction::Stay
            }
            KeyCode::Enter => {
                let name = draft.trim().to_owned();
                self.sub = SubMode::Main;
                if name.is_empty() {
                    HubAction::Stay
                } else {
                    HubAction::SavePreset(name)
                }
            }
            KeyCode::Backspace => {
                draft.pop();
                HubAction::Stay
            }
            KeyCode::Char('u') if ctrl => {
                draft.clear();
                HubAction::Stay
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                if !c.is_whitespace() {
                    draft.push(c);
                }
                HubAction::Stay
            }
            _ => HubAction::Stay,
        }
    }

    fn key_pick_for_role(&mut self, key: KeyEvent, visible: usize) -> HubAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let SubMode::PickModelForRole {
            query,
            provider_idx,
            selected,
            ..
        } = &self.sub
        else {
            return HubAction::Stay;
        };
        let filtered = self.filtered_models(query, *provider_idx);
        let count = filtered.len();
        let picked_alias = filtered.get(*selected).map(|m| m.alias.clone());
        let prov_count = self.providers.len().max(1);

        let SubMode::PickModelForRole {
            role,
            query,
            provider_idx,
            selected,
            top,
        } = &mut self.sub
        else {
            return HubAction::Stay;
        };
        let mut reset_pos = false;
        match key.code {
            KeyCode::Esc if !query.is_empty() => {
                query.clear();
                reset_pos = true;
            }
            KeyCode::Esc if *provider_idx > 0 => {
                *provider_idx = 0;
                reset_pos = true;
            }
            KeyCode::Esc => {
                self.sub = SubMode::Main;
                return HubAction::Stay;
            }
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down if *selected + 1 < count => *selected += 1,
            KeyCode::PageUp => *selected = selected.saturating_sub(visible.max(1)),
            KeyCode::PageDown => {
                *selected = (*selected + visible.max(1)).min(count.saturating_sub(1));
            }
            KeyCode::Home => *selected = 0,
            KeyCode::End => *selected = count.saturating_sub(1),
            KeyCode::Left | KeyCode::BackTab => {
                *provider_idx = (*provider_idx + prov_count - 1) % prov_count;
                reset_pos = true;
            }
            KeyCode::Right | KeyCode::Tab => {
                *provider_idx = (*provider_idx + 1) % prov_count;
                reset_pos = true;
            }
            KeyCode::Backspace => {
                query.pop();
                reset_pos = true;
            }
            KeyCode::Char('u') if ctrl => {
                query.clear();
                reset_pos = true;
            }
            KeyCode::Enter => {
                let role_name = role.clone();
                self.sub = SubMode::Main;
                return match picked_alias {
                    Some(alias) if self.session_only => HubAction::SwitchTemporary(alias),
                    Some(alias) => HubAction::AssignRole {
                        role: role_name,
                        target: alias,
                        keep_open: true,
                    },
                    None => HubAction::Stay,
                };
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                query.push(c);
                reset_pos = true;
            }
            _ => return HubAction::Stay,
        }
        if reset_pos {
            *selected = 0;
            *top = 0;
        }
        keep_visible(*selected, top, visible);
        HubAction::Stay
    }

    fn key_main(&mut self, key: KeyEvent, visible: usize) -> HubAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.select_initial_row();
                    return HubAction::Stay;
                }
                if self.provider_idx > 0 && self.uses_provider_filter() {
                    self.provider_idx = 0;
                    self.select_initial_row();
                    return HubAction::Stay;
                }
                return HubAction::Close;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let forward = key.code == KeyCode::Tab;
                if self.kind == HubKind::RolesHub {
                    self.tab = self.tab.step(forward);
                    self.query.clear();
                    self.provider_idx = 0;
                    self.select_initial_row();
                } else if self.uses_provider_filter() {
                    self.step_provider(forward);
                }
                return HubAction::Stay;
            }
            KeyCode::Left | KeyCode::Right => {
                let forward = key.code == KeyCode::Right;
                if self.uses_provider_filter() {
                    self.step_provider(forward);
                    return HubAction::Stay;
                }
                if self.kind == HubKind::RolesHub && self.tab == HubTab::Roles && !self.session_only
                {
                    return self.cycle_selected_role_level(forward);
                }
                return HubAction::Stay;
            }
            KeyCode::Char('l' | 'L') if ctrl => {
                if self.kind == HubKind::RolesHub && self.tab == HubTab::Roles && !self.session_only
                {
                    return self.cycle_selected_role_level(true);
                }
                return HubAction::Stay;
            }
            KeyCode::Char('s' | 'S') if ctrl => {
                if self.kind == HubKind::RolesHub && !self.session_only {
                    self.sub = SubMode::SavePresetName {
                        draft: String::new(),
                    };
                }
                return HubAction::Stay;
            }
            KeyCode::Char('t' | 'T') if ctrl => {
                return self.activate_temporary_model();
            }
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-(visible.max(1).cast_signed())),
            KeyCode::PageDown => self.move_sel(visible.max(1).cast_signed()),
            KeyCode::Home => self.select_first(),
            KeyCode::End => self.select_last(),
            KeyCode::Backspace => {
                self.query.pop();
                self.select_first();
            }
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.select_initial_row();
            }
            KeyCode::Enter => return self.activate_selected(alt),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.query.push(c);
                self.select_first();
            }
            _ => return HubAction::Stay,
        }
        keep_visible(self.selected, &mut self.top, visible);
        HubAction::Stay
    }
}
