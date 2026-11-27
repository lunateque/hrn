//! Roles Hub (`/model`, `Alt+M`), temporary model switch (`/switch`, `Alt+P`), and `Ctrl+P`
//! role cycling on [`ChatBackend`].

use super::ChatBackend;
use crate::wiring::build_runtime_with_tools;
use hrn_edge_llm::{
    effective_cycle_entries, enabled_presets, expand_selector, resolve_alias, standard_role_names,
};
use hrn_kernel::{LevelSuffix, Result, ThinkingLevel};
use hrn_tui::{ModelInfo, RoleAssignment};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

impl ChatBackend {
    /// Rich model catalog with display names, provider, context window, reasoning, pricing,
    /// and reverse-mapped role badges for `/model` and `/switch`.
    pub(super) fn model_catalog_list(&self) -> Vec<ModelInfo> {
        let mut roles_by_alias: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in self.role_rows_list() {
            let resolved = resolve_alias(&self.llm, Some(&row.target))
                .unwrap_or_else(|_| hrn_kernel::split_level_suffix(&row.target).0.to_owned());
            let entry = roles_by_alias.entry(resolved).or_default();
            if !entry.contains(&row.role) {
                entry.push(row.role);
            }
        }
        enabled_presets(&self.llm)
            .into_iter()
            .map(|(alias, preset)| {
                let name = short_model_name(alias, preset);
                let reasoning = preset.reasoning.unwrap_or(false)
                    || preset.thinking.is_some()
                    || preset.thinking_budgets.is_some();
                let roles = roles_by_alias.remove(alias).unwrap_or_default();
                let tag = self
                    .llm
                    .model_tags
                    .get(alias)
                    .or_else(|| roles.iter().find_map(|role| self.llm.model_tags.get(role)))
                    .map(|t| (t.name.clone(), t.color.clone()));
                ModelInfo {
                    alias: alias.to_owned(),
                    id: preset.id.clone(),
                    name,
                    provider: preset.provider.clone(),
                    context_window: preset.context_window.unwrap_or(200_000),
                    max_context_window: preset.max_context_window,
                    reasoning,
                    thinking: preset.thinking,
                    price: preset.price,
                    roles,
                    tag,
                }
            })
            .collect()
    }

    /// Ordered role assignments tagged as core (`standard_role_names`) vs custom user roles.
    pub(super) fn role_rows_list(&self) -> Vec<RoleAssignment> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for role in standard_role_names() {
            seen.insert(role.to_owned());
            if let Some(target) = self.llm.roles.get(role) {
                out.push(RoleAssignment {
                    role: role.to_owned(),
                    target: target.clone(),
                    is_standard: true,
                });
            } else if let Ok(resolved) = resolve_alias(&self.llm, Some(role)) {
                out.push(RoleAssignment {
                    role: role.to_owned(),
                    target: resolved,
                    is_standard: true,
                });
            }
        }
        for (role, target) in &self.llm.roles {
            if seen.insert(role.clone()) {
                out.push(RoleAssignment {
                    role: role.clone(),
                    target: target.clone(),
                    is_standard: false,
                });
            }
        }
        out
    }

    /// Ordered `(role, target)` entries shown in the `/model` Roles Hub.
    pub(super) fn role_assignments_list(&self) -> Vec<(String, String)> {
        self.role_rows_list()
            .into_iter()
            .map(|r| (r.role, r.target))
            .collect()
    }

    /// Persist `[roles.<role>] = <target>` to the configured `role_storage` and rebuild the live
    /// runtime without disrupting MCP servers or background work.
    pub(super) fn assign_role_live(&mut self, role: &str, target: &str) -> Result<String> {
        let storage_path = crate::roles::role_storage_path(&self.cfg, &self.config_path, &self.cwd);
        let mut staged = self.cfg.clone();
        let report = crate::roles::assign_role(&mut staged, &storage_path, role, target)?;
        let new_runtime = build_runtime_with_tools(
            &staged,
            Arc::clone(&self.sqlite),
            &self.cwd,
            &self.mcp_tools,
            &self.background,
        )?;
        self.runtime = new_runtime;
        self.idle.sync_runtime(&self.runtime);
        self.llm = staged.llm.clone();
        self.model_names = crate::stats::names(&staged.llm);
        self.cfg = staged;
        if role == "default"
            && let Ok(resolved) = expand_selector(&self.llm, Some("@default"))
        {
            self.settings.model = resolved.alias;
            if let LevelSuffix::Level(level) = resolved.level {
                self.settings.thinking = level;
            }
        }
        Ok(report)
    }

    /// `/switch` & `Alt+P`: session-only temporary switch; records `ModelChange { role: "temporary" }`
    /// in the active session DAG and never modifies `[roles]` on disk.
    pub(super) fn switch_temporary_live(
        &mut self,
        selector: &str,
    ) -> Result<(String, Option<ThinkingLevel>)> {
        let resolved = expand_selector(&self.llm, Some(selector))?;
        let thinking = match resolved.level {
            LevelSuffix::Level(level) => Some(level),
            LevelSuffix::None | LevelSuffix::Inherit => None,
        };
        if let Some(session_id) = self.active_session_id() {
            let _ = self.runtime.switch_model_temporary(
                &session_id,
                selector,
                &resolved.alias,
                thinking,
                Some("temporary"),
            )?;
        }
        self.settings.model.clone_from(&resolved.alias);
        if let Some(level) = thinking {
            self.settings.thinking = level;
        }
        Ok((resolved.alias, thinking))
    }

    /// Ordered `Ctrl+P` cycle entries `(label, resolved_alias, optional_thinking)`.
    pub(super) fn cycle_entries_list(&self) -> Vec<(String, String, Option<ThinkingLevel>)> {
        effective_cycle_entries(&self.llm)
            .into_iter()
            .map(|entry| {
                let thinking = match entry.level {
                    LevelSuffix::Level(level) => Some(level),
                    LevelSuffix::None | LevelSuffix::Inherit => None,
                };
                (entry.label, entry.alias, thinking)
            })
            .collect()
    }

    /// Configured `[modelPresets.*]` names for the bare `/modelpreset` picker.
    pub(super) fn model_presets_list(&self) -> Vec<String> {
        self.cfg.llm.role_presets.keys().cloned().collect()
    }
}

fn short_model_name(alias: &str, preset: &hrn_edge_llm::ModelPreset) -> String {
    if let Some(name) = preset.name.as_deref().map(str::trim)
        && !name.is_empty()
    {
        return name.to_owned();
    }
    let id_tail = preset.id.rsplit('/').next().unwrap_or(&preset.id).trim();
    if !id_tail.is_empty() {
        return id_tail.to_owned();
    }
    ModelInfo::short_name_from_alias(alias)
}
