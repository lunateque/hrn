//! Live interactive `/settings` backend bridge backed by `settings.schema.toml`,
//! [`crate::config_watch::LiveSettingsPort`], and comment-preserving [`crate::toml_write`].

use super::ChatBackend;
use crate::config::Config;
use crate::config_layers::{LayerSource, MergedConfig};
use crate::settings_schema::{
    SettingKind as SchemaKind, SettingSpec, format_value, parse_cli_value, schema,
};
use crate::toml_write;
use crate::wiring::build_runtime_with_tools;
use hrn_kernel::{HrnError, Result};
use hrn_tui::{SettingItem, SettingKind, SettingUpdate};
use std::path::Path;
use std::sync::Arc;
use toml_edit::{Item, Table};

const THEME_SLOT_KEYS: [&str; 3] = ["tui.theme", "tui.theme.dark", "tui.theme.light"];

impl ChatBackend {
    /// Enumerate every schema setting with its effective value, provenance, and editor metadata.
    pub(super) fn settings_items_live(&self) -> Vec<SettingItem> {
        let Some(port) = &self.settings_port else {
            return Vec::new();
        };
        let merged = port.merged_snapshot();
        let themes = hrn_tui::available_theme_names();
        schema()
            .all()
            .iter()
            .map(|spec| build_setting_item(spec, &merged, &themes))
            .collect()
    }

    /// Validate, persist to global `config.toml`, reload, and apply a single setting live.
    pub(super) fn set_setting_live(&mut self, key: &str, raw_value: &str) -> Result<SettingUpdate> {
        let spec = schema()
            .get(key)
            .ok_or_else(|| HrnError::Config(format!("unknown setting `{key}`")))?;
        let parsed = parse_cli_value(spec, raw_value)?;
        let Some(port) = self.settings_port.clone() else {
            return Err(HrnError::Config(
                "live settings port unavailable".to_owned(),
            ));
        };
        let global_path = port.global_path().to_path_buf();
        let snapshot = FileBackup::capture(&global_path);
        write_setting_key(&global_path, key, &parsed)?;
        let (new_cfg, merged) = match port.reload_from_disk() {
            Ok(loaded) => loaded,
            Err(err) => {
                snapshot.restore();
                let _ = port.reload_from_disk();
                return Err(err);
            }
        };
        let shown = format_value(spec, Some(&parsed), true, "********");
        let base_msg = format!("saved {key} = {shown}");
        self.apply_reloaded_settings(new_cfg, &merged, key, base_msg)
    }

    /// Delete `key` from global `config.toml`, reload, and apply the effective value live.
    pub(super) fn reset_setting_live(&mut self, key: &str) -> Result<SettingUpdate> {
        let _spec = schema()
            .get(key)
            .ok_or_else(|| HrnError::Config(format!("unknown setting `{key}`")))?;
        let Some(port) = self.settings_port.clone() else {
            return Err(HrnError::Config(
                "live settings port unavailable".to_owned(),
            ));
        };
        let global_path = port.global_path().to_path_buf();
        let snapshot = FileBackup::capture(&global_path);
        let removed = toml_write::delete_key(&global_path, key)?;
        let (new_cfg, merged) = match port.reload_from_disk() {
            Ok(loaded) => loaded,
            Err(err) => {
                snapshot.restore();
                let _ = port.reload_from_disk();
                return Err(err);
            }
        };
        let base_msg = if removed {
            format!("reset {key} to default")
        } else {
            format!("{key} already at default in global config")
        };
        self.apply_reloaded_settings(new_cfg, &merged, key, base_msg)
    }

    fn apply_reloaded_settings(
        &mut self,
        mut new_cfg: Config,
        merged: &MergedConfig,
        key: &str,
        base_msg: String,
    ) -> Result<SettingUpdate> {
        if self.no_title_forced {
            new_cfg.auto_title = false;
            new_cfg.title_refresh_on_replan = false;
        }
        self.background.pool.clear_all_invalidated();
        let new_runtime = build_runtime_with_tools(
            &new_cfg,
            Arc::clone(&self.sqlite),
            &self.cwd,
            &self.mcp_tools,
            &self.background,
        )?;
        self.runtime = new_runtime;
        self.idle.sync_runtime(&self.runtime);
        self.llm = new_cfg.llm.clone();
        self.model_names = crate::stats::names(&new_cfg.llm);
        self.cfg = new_cfg;

        let mut update = SettingUpdate {
            message: append_shadow_note(base_msg, merged, key),
            appearance: Some(live_appearance(&self.cfg)),
            double_escape_action: Some(self.cfg.double_escape_action),
            pinned_agents: Some(self.cfg.pinned_agents),
            subagent_live_preview: Some(self.cfg.subagent_live_preview),
            mouse: Some(self.cfg.mouse),
            branch_summary: Some(self.cfg.branch_summary),
            btw_inject: Some(self.cfg.btw_inject),
            notify: Some(self.cfg.notify.clone()),
            ..SettingUpdate::default()
        };
        if key == "thinking" {
            self.settings.thinking = self.cfg.thinking;
            update.thinking = Some(self.cfg.thinking);
        }
        if key == "tools.approval_mode" {
            self.settings.mode = self.cfg.approval_mode;
            self.control.set_permission_mode(self.cfg.approval_mode);
            update.approval_mode = Some(self.cfg.approval_mode);
        }
        Ok(update)
    }
}

fn live_appearance(cfg: &Config) -> hrn_tui::AppearanceConfig {
    let mut appearance = cfg.appearance.clone();
    appearance.themes_dir = crate::app_data_dir().ok().map(|d| d.join("themes"));
    appearance.profile = crate::paths::active_profile();
    appearance.keymap_paths = crate::keybindings_paths();
    appearance
}

fn append_shadow_note(base: String, merged: &MergedConfig, key: &str) -> String {
    if let Some(winner) = merged.overridden_above_global(key) {
        format!("{base} (shadowed by {})", winner.label())
    } else {
        base
    }
}

fn build_setting_item(spec: &SettingSpec, merged: &MergedConfig, themes: &[String]) -> SettingItem {
    let legacy_theme = merged.get("tui.theme").filter(|v| v.is_str());
    let uses_legacy_slot =
        matches!(spec.key.as_str(), "tui.theme.dark" | "tui.theme.light") && legacy_theme.is_some();
    let effective = if uses_legacy_slot {
        legacy_theme
    } else {
        merged.get(&spec.key).or(spec.default.as_ref())
    };
    let source_layer = if uses_legacy_slot {
        merged.source_of("tui.theme")
    } else {
        merged.source_of(&spec.key)
    };
    let value = format_value(spec, effective, true, "********");
    let default = format_value(spec, spec.default.as_ref(), false, "");
    let modified = source_layer != LayerSource::Default
        || (effective.is_some() && effective != spec.default.as_ref());
    let (kind, values) = map_editor_kind(spec, &value, themes);
    SettingItem {
        key: spec.key.clone(),
        tab: spec.tab.clone(),
        kind,
        value,
        default,
        modified,
        source: source_layer.label(),
        values,
        env: spec.env.clone(),
        secret: spec.secret,
        description: spec.description.clone(),
    }
}

fn map_editor_kind(
    spec: &SettingSpec,
    current_value: &str,
    themes: &[String],
) -> (SettingKind, Vec<String>) {
    if THEME_SLOT_KEYS.contains(&spec.key.as_str()) {
        let mut vals = themes.to_vec();
        if !current_value.is_empty()
            && current_value != "(unset)"
            && !vals.iter().any(|v| v == current_value)
        {
            vals.push(current_value.to_owned());
        }
        return (SettingKind::Enum, vals);
    }
    if spec.kind == SchemaKind::String && !spec.values.is_empty() {
        return (SettingKind::Enum, spec.values.clone());
    }
    let kind = match spec.kind {
        SchemaKind::Bool => SettingKind::Bool,
        SchemaKind::String => SettingKind::String,
        SchemaKind::Integer => SettingKind::Integer,
        SchemaKind::Float => SettingKind::Float,
        SchemaKind::Array => SettingKind::Array,
        SchemaKind::Table => SettingKind::Table,
    };
    (kind, Vec::new())
}

/// Write `dotted_key = value`, smoothly migrating legacy `[tui] theme = "<name>"` to a
/// `[tui.theme]` table when setting `tui.theme.dark`, `tui.theme.light`, or `tui.theme.current`.
fn write_setting_key(path: &Path, dotted_key: &str, value: &toml::Value) -> Result {
    if dotted_key.starts_with("tui.theme.") {
        migrate_legacy_theme_table(path)?;
    }
    toml_write::set_key(path, dotted_key, value)
}

fn migrate_legacy_theme_table(path: &Path) -> Result {
    let target = toml_write::resolve_write_target(path);
    if !target.is_file() {
        return Ok(());
    }
    let _ = toml_write::edit_document(path, |doc| {
        if let Some(tui_tbl) = doc.get_mut("tui").and_then(Item::as_table_mut)
            && let Some(legacy_name) = tui_tbl
                .get("theme")
                .and_then(Item::as_str)
                .map(str::to_owned)
        {
            let mut sub = Table::new();
            sub.insert("dark", toml_edit::value(legacy_name.as_str()));
            sub.insert("light", toml_edit::value(legacy_name.as_str()));
            tui_tbl.insert("theme", Item::Table(sub));
        }
        Ok(())
    })?;
    Ok(())
}

struct FileBackup {
    path: std::path::PathBuf,
    previous: Option<String>,
}

impl FileBackup {
    fn capture(path: &Path) -> Self {
        let target = toml_write::resolve_write_target(path);
        let previous = std::fs::read_to_string(&target).ok();
        Self {
            path: target,
            previous,
        }
    }

    fn restore(self) {
        match self.previous {
            Some(content) => {
                let _ = toml_write::atomic_write(&self.path, &content);
            }
            None => {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_layers::LayerInput;
    use std::collections::BTreeMap;

    #[test]
    fn migrating_legacy_theme_string_to_structured_table_preserves_other_slot() {
        let dir =
            std::env::temp_dir().join(format!("hrn-settings-ui-theme-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, "[tui]\ntheme = \"cobalt\"\nnotify = true\n").expect("seed");

        write_setting_key(
            &cfg_path,
            "tui.theme.dark",
            &toml::Value::String("titanium-vivid".into()),
        )
        .expect("write dark slot");
        let raw = std::fs::read_to_string(&cfg_path).expect("read");
        let parsed: toml::Table = toml::from_str(&raw).expect("valid toml");
        let theme_tbl = parsed["tui"]["theme"].as_table().expect("migrated table");
        assert_eq!(theme_tbl["dark"].as_str(), Some("titanium-vivid"));
        assert_eq!(theme_tbl["light"].as_str(), Some("cobalt"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_items_exposes_theme_enums_and_all_schema_keys() {
        let input = LayerInput {
            cwd: std::path::PathBuf::from("."),
            env: BTreeMap::new(),
            ..LayerInput::default()
        };
        let merged = crate::config_layers::load_layers(&input).expect("load defaults");
        let themes = hrn_tui::available_theme_names();
        let items: Vec<SettingItem> = schema()
            .all()
            .iter()
            .map(|s| build_setting_item(s, &merged, &themes))
            .collect();
        assert_eq!(items.len(), schema().all().len());
        let dark = items
            .iter()
            .find(|i| i.key == "tui.theme.dark")
            .expect("tui.theme.dark");
        assert_eq!(dark.kind, SettingKind::Enum);
        assert!(dark.values.iter().any(|v| v == "titanium"));
        let notify = items
            .iter()
            .find(|i| i.key == "tui.notify")
            .expect("tui.notify");
        assert_eq!(notify.kind, SettingKind::Bool);
    }
}
