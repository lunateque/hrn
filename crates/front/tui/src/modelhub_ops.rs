//! Navigation, selection, filtering, and scoring helpers for [`crate::modelhub::ModelHub`].

use crate::modelhub::{HubAction, HubKind, HubTab, ModelHub, RoleRow, SubMode};
use crate::port::{ModelInfo, RoleAssignment};
use std::collections::BTreeSet;

/// Thinking-level suffixes cycled on a role row in the Roles tab.
const LEVEL_SUFFIXES: [&str; 10] = [
    "", "auto", "off", "minimal", "low", "medium", "high", "xhigh", "max", "inherit",
];

impl ModelHub {
    pub(crate) fn uses_provider_filter(&self) -> bool {
        self.providers.len() > 1
            && (self.kind == HubKind::TemporaryPicker
                || (self.kind == HubKind::RolesHub && self.tab == HubTab::Models))
    }

    pub(crate) fn step_provider(&mut self, forward: bool) {
        let len = self.providers.len().max(1);
        self.provider_idx = if forward {
            (self.provider_idx + 1) % len
        } else {
            (self.provider_idx + len - 1) % len
        };
        self.select_first();
    }

    pub(crate) fn cycle_selected_role_level(&self, forward: bool) -> HubAction {
        let rows = self.filtered_role_rows();
        let Some(RoleRow::Item(row)) = rows.get(self.selected) else {
            return HubAction::Stay;
        };
        let next = cycle_level_suffix(&row.target, forward);
        HubAction::AssignRole {
            role: row.role.clone(),
            target: next,
            keep_open: true,
        }
    }

    pub(crate) fn activate_temporary_model(&self) -> HubAction {
        if self.kind == HubKind::TemporaryPicker
            || (self.kind == HubKind::RolesHub && self.tab == HubTab::Models)
        {
            return self
                .filtered_models(&self.query, self.provider_idx)
                .get(self.selected)
                .map_or(HubAction::Stay, |m| {
                    HubAction::SwitchTemporary(m.alias.clone())
                });
        }
        HubAction::Stay
    }

    pub(crate) fn activate_selected(&mut self, alt: bool) -> HubAction {
        match self.kind {
            HubKind::TemporaryPicker => self.activate_temporary_model(),
            HubKind::PresetPicker => self
                .filtered_presets()
                .get(self.selected)
                .map_or(HubAction::Stay, |p| {
                    HubAction::SwitchPreset((*p).to_owned())
                }),
            HubKind::RolesHub => match self.tab {
                HubTab::Models => {
                    let Some(alias) = self
                        .filtered_models(&self.query, self.provider_idx)
                        .get(self.selected)
                        .map(|m| m.alias.clone())
                    else {
                        return HubAction::Stay;
                    };
                    if alt || self.session_only {
                        HubAction::SwitchTemporary(alias)
                    } else {
                        HubAction::AssignRole {
                            role: "default".to_owned(),
                            target: alias,
                            keep_open: false,
                        }
                    }
                }
                HubTab::Roles => {
                    let rows = self.filtered_role_rows();
                    let Some(RoleRow::Item(row)) = rows.get(self.selected) else {
                        return HubAction::Stay;
                    };
                    let (base, _) = hrn_kernel::split_level_suffix(&row.target);
                    let initial = self
                        .models
                        .iter()
                        .position(|m| m.alias == base)
                        .unwrap_or(0);
                    self.sub = SubMode::PickModelForRole {
                        role: row.role.clone(),
                        query: String::new(),
                        provider_idx: 0,
                        selected: initial,
                        top: 0,
                    };
                    HubAction::Stay
                }
                HubTab::Presets => self
                    .filtered_presets()
                    .get(self.selected)
                    .map_or(HubAction::Stay, |p| {
                        HubAction::SwitchPreset((*p).to_owned())
                    }),
            },
        }
    }

    pub(crate) fn select_initial_row(&mut self) {
        self.top = 0;
        if self.kind == HubKind::TemporaryPicker
            || (self.kind == HubKind::RolesHub && self.tab == HubTab::Models)
        {
            let models = self.filtered_models(&self.query, self.provider_idx);
            self.selected = if self.query.is_empty() {
                models
                    .iter()
                    .position(|m| m.alias == self.current_model)
                    .unwrap_or(0)
            } else {
                0
            };
        } else {
            self.select_first();
        }
    }

    pub(crate) fn select_first(&mut self) {
        self.top = 0;
        if self.kind == HubKind::RolesHub && self.tab == HubTab::Roles {
            let rows = self.filtered_role_rows();
            self.selected = rows
                .iter()
                .position(RoleRow::is_selectable)
                .unwrap_or_default();
        } else {
            self.selected = 0;
        }
    }

    pub(crate) fn select_last(&mut self) {
        if self.kind == HubKind::RolesHub && self.tab == HubTab::Roles {
            let rows = self.filtered_role_rows();
            self.selected = rows
                .iter()
                .rposition(RoleRow::is_selectable)
                .unwrap_or_default();
        } else {
            self.selected = self.current_row_count().saturating_sub(1);
        }
    }

    pub(crate) fn clamp_selection(&mut self) {
        let count = self.current_row_count();
        if count == 0 {
            self.selected = 0;
            self.top = 0;
        } else if self.selected >= count {
            self.select_last();
        } else if self.kind == HubKind::RolesHub && self.tab == HubTab::Roles {
            let rows = self.filtered_role_rows();
            if !rows.get(self.selected).is_some_and(RoleRow::is_selectable) {
                self.select_first();
            }
        }
    }

    pub(crate) fn current_row_count(&self) -> usize {
        match self.kind {
            HubKind::TemporaryPicker => self.filtered_models(&self.query, self.provider_idx).len(),
            HubKind::PresetPicker => self.filtered_presets().len(),
            HubKind::RolesHub => match self.tab {
                HubTab::Models => self.filtered_models(&self.query, self.provider_idx).len(),
                HubTab::Roles => self.filtered_role_rows().len(),
                HubTab::Presets => self.filtered_presets().len(),
            },
        }
    }

    pub(crate) fn move_sel(&mut self, delta: isize) {
        let count = self.current_row_count();
        if count == 0 {
            self.selected = 0;
            return;
        }
        if self.kind == HubKind::RolesHub && self.tab == HubTab::Roles {
            let rows = self.filtered_role_rows();
            let steps = delta.unsigned_abs();
            let mut cur = self.selected.min(rows.len().saturating_sub(1));
            for _ in 0..steps {
                let Some(next) = step_selectable(&rows, cur, delta > 0) else {
                    break;
                };
                cur = next;
            }
            self.selected = cur;
            return;
        }
        let next = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta.unsigned_abs()).min(count - 1)
        };
        self.selected = next;
    }
}

fn step_selectable(rows: &[RoleRow], mut idx: usize, forward: bool) -> Option<usize> {
    loop {
        idx = if forward {
            idx.checked_add(1).filter(|&n| n < rows.len())?
        } else {
            idx.checked_sub(1)?
        };
        if rows.get(idx).is_some_and(RoleRow::is_selectable) {
            return Some(idx);
        }
    }
}

pub(crate) fn collect_providers(models: &[ModelInfo]) -> Vec<String> {
    let unique: BTreeSet<&str> = models.iter().map(|m| m.provider.as_str()).collect();
    let mut out = Vec::with_capacity(unique.len() + 1);
    out.push("All".to_owned());
    out.extend(unique.into_iter().map(str::to_owned));
    out
}

/// Multi-token case-insensitive model scorer; returns `None` when any token fails to match.
pub(crate) fn score_model(tokens: &[String], m: &ModelInfo) -> Option<u32> {
    if tokens.is_empty() {
        return Some(0);
    }
    let name = m.name.to_ascii_lowercase();
    let alias = m.alias.to_ascii_lowercase();
    let id = m.id.to_ascii_lowercase();
    let provider = m.provider.to_ascii_lowercase();
    let roles: Vec<String> = m.roles.iter().map(|r| r.to_ascii_lowercase()).collect();
    let ctx = format_ctx_short(m.context_window).to_ascii_lowercase();
    let mut total = 0u32;
    for tok in tokens {
        let s = if name == *tok || alias == *tok || roles.iter().any(|r| r == tok) {
            0
        } else if name.starts_with(tok) {
            1
        } else if segment_starts_with(&name, tok) || segment_starts_with(&alias, tok) {
            2
        } else if name.contains(tok) {
            3
        } else if provider.starts_with(tok) || roles.iter().any(|r| r.starts_with(tok)) {
            4
        } else if alias.contains(tok)
            || id.contains(tok)
            || provider.contains(tok)
            || roles.iter().any(|r| r.contains(tok))
            || (!ctx.is_empty() && ctx.contains(tok))
            || (m.reasoning && "reasoning".starts_with(tok))
        {
            5
        } else {
            return None;
        };
        total = total.saturating_add(s);
    }
    Some(total)
}

fn segment_starts_with(haystack: &str, needle: &str) -> bool {
    haystack
        .split(['/', '-', '_', ':', '.'])
        .any(|part| part.starts_with(needle))
}

pub(crate) fn role_matches(tokens: &[String], row: &RoleAssignment, models: &[ModelInfo]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let role = row.role.to_ascii_lowercase();
    let target = row.target.to_ascii_lowercase();
    let (base, _) = hrn_kernel::split_level_suffix(row.target.trim());
    let short = models.iter().find(|m| m.alias == base).map_or_else(
        || ModelInfo::short_name_from_alias(base).to_ascii_lowercase(),
        |m| m.name.to_ascii_lowercase(),
    );
    let kind = if row.is_standard { "core" } else { "custom" };
    tokens.iter().all(|tok| {
        role.contains(tok) || target.contains(tok) || short.contains(tok) || kind.contains(tok)
    })
}

pub(crate) fn format_ctx_short(tokens: u64) -> String {
    if tokens == 0 {
        return String::new();
    }
    if tokens >= 1_000_000 {
        let m = tokens / 1_000_000;
        let rem = (tokens % 1_000_000) / 100_000;
        if rem == 0 {
            format!("{m}M")
        } else {
            format!("{m}.{rem}M")
        }
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn cycle_level_suffix(target: &str, forward: bool) -> String {
    let trimmed = target.trim();
    let (base, suffix) = hrn_kernel::split_level_suffix(trimmed);
    let current = match suffix {
        hrn_kernel::LevelSuffix::None => "",
        hrn_kernel::LevelSuffix::Inherit => "inherit",
        hrn_kernel::LevelSuffix::Level(lvl) => lvl.as_str(),
    };
    let len = LEVEL_SUFFIXES.len();
    let pos = LEVEL_SUFFIXES
        .iter()
        .position(|s| *s == current)
        .unwrap_or(0);
    let next_idx = if forward {
        (pos + 1) % len
    } else {
        (pos + len - 1) % len
    };
    let next = LEVEL_SUFFIXES.get(next_idx).copied().unwrap_or("");
    if next.is_empty() {
        base.to_owned()
    } else {
        format!("{base}:{next}")
    }
}

pub(crate) fn keep_visible(selected: usize, top: &mut usize, visible: usize) {
    if selected < *top {
        *top = selected;
    } else if visible > 0 && selected >= *top + visible {
        *top = selected + 1 - visible;
    }
}
