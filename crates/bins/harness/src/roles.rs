//! omp's role flags (`--smol` / `--slow` / `--plan`), `--preset` role bundles, Roles Hub
//! persistence (`role_storage = global | project`), and thinking-level overrides.

use crate::cli::RoleFlags;
use crate::config::Config;
use hrn_edge_llm::{RolePreset, RoleStorage, expand_selector, validate_config};
use hrn_kernel::{HrnError, LevelSuffix, Result, ThinkingLevel};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// omp's `modelRoles` role names the flags map to (`cli-reference`: smol is the fast model,
/// slow the thorough one, plan the architect).
const ROLE_OF_FLAG: [(&str, &str); 3] = [("smol", "smol"), ("slow", "slow"), ("plan", "plan")];

/// Target TOML file where `[roles]` mutations from Roles Hub / `/modelpreset switch` are written.
pub(crate) fn role_storage_path(cfg: &Config, global_path: &Path, cwd: &Path) -> PathBuf {
    match cfg.llm.role_storage {
        RoleStorage::Global => global_path.to_path_buf(),
        RoleStorage::Project => cwd.join(".harness").join("config.toml"),
    }
}

/// Apply `--preset`, then the individual role flags (which win), then `--think(ing)`.
pub(crate) fn apply(cfg: &mut Config, flags: &RoleFlags, think: Option<ThinkingLevel>) -> Result {
    if let Some(name) = &flags.preset {
        let _ = apply_preset_in_memory(cfg, name)?;
    }
    for ((flag, role), value) in ROLE_OF_FLAG.iter().zip([
        flags.smol.as_deref(),
        flags.slow.as_deref(),
        flags.plan.as_deref(),
    ]) {
        let Some(value) = value else { continue };
        expand_selector(&cfg.llm, Some(value))
            .map_err(|e| HrnError::Config(format!("--{flag} model `{value}`: {e}")))?;
        cfg.llm.roles.insert((*role).to_owned(), value.to_owned());
    }
    if let Some(level) = think {
        cfg.thinking = level;
    }
    validate_config(&cfg.llm)
}

/// The effective session thinking level: an explicit `--think(ing)`/`--thinking` flag, else a
/// `model:level` suffix on the selector, else the config value.
pub(crate) fn thinking_for(
    cfg: &Config,
    model: Option<&str>,
    explicit: Option<ThinkingLevel>,
) -> ThinkingLevel {
    if let Some(explicit) = explicit {
        return explicit;
    }
    if let Some(LevelSuffix::Level(level)) = model
        .map(hrn_kernel::split_level_suffix)
        .map(|(_, suffix)| suffix)
    {
        return level;
    }
    cfg.thinking
}

/// Validate and persist `[roles.<role>] = <target>` to `storage_path`, updating `cfg` in place.
///
/// # Errors
/// Validation fails (unknown target, disabled preset, role cycle) or writing TOML fails.
pub(crate) fn assign_role(
    cfg: &mut Config,
    storage_path: &Path,
    role: &str,
    target: &str,
) -> Result<String> {
    let role = role.trim();
    let target = target.trim();
    if role.is_empty() || target.is_empty() {
        return Err(HrnError::Config("role and target must be non-empty".into()));
    }
    let mut next_llm = cfg.llm.clone();
    next_llm.roles.insert(role.to_owned(), target.to_owned());
    validate_config(&next_llm)?;
    let resolved = expand_selector(&next_llm, Some(&format!("@{role}")))?;
    crate::toml_write::set_key(
        storage_path,
        &format!("roles.{role}"),
        &toml::Value::String(target.to_owned()),
    )?;
    cfg.llm = next_llm;
    Ok(format!(
        "role `{role}` → {target} ({}) saved to {}",
        resolved.alias,
        storage_path.display()
    ))
}

/// Apply `[modelPresets.<name>]` to `cfg` in memory: clears omitted roles, validates that the
/// preset's `default` resolves to an enabled preset (no-op on failure), and resolves thinking
/// level with `:inherit` / `:level` precedence.
///
/// Returns `(resolved_default_alias, effective_thinking, removed_role_names)`.
pub(crate) fn apply_preset_in_memory(
    cfg: &mut Config,
    name: &str,
) -> Result<(String, ThinkingLevel, Vec<String>)> {
    let bundle = cfg.llm.role_presets.get(name).cloned().ok_or_else(|| {
        HrnError::Config(format!(
            "no `[modelPresets.{name}]` is configured (omp's `modelPresets` role bundles)"
        ))
    })?;

    let mut next_llm = cfg.llm.clone();
    let mut next_roles: BTreeMap<String, String> = bundle.model_roles.clone();
    if !next_roles.contains_key("default")
        && let Some(existing_default) = cfg.llm.roles.get("default")
    {
        next_roles.insert("default".to_owned(), existing_default.clone());
    }
    let removed: Vec<String> = cfg
        .llm
        .roles
        .keys()
        .filter(|role| !next_roles.contains_key(role.as_str()))
        .cloned()
        .collect();
    next_llm.roles = next_roles;

    // Validate before mutating `cfg` so an unavailable default or invalid role is a clean no-op.
    validate_config(&next_llm)
        .map_err(|e| HrnError::Config(format!("modelPresets `{name}`: {e}")))?;
    let default_resolved = expand_selector(&next_llm, Some("@default"))
        .map_err(|e| HrnError::Config(format!("modelPresets `{name}` default unavailable: {e}")))?;
    for role in bundle.model_roles.keys() {
        expand_selector(&next_llm, Some(&format!("@{role}")))
            .map_err(|e| HrnError::Config(format!("modelPresets `{name}` role `{role}`: {e}")))?;
    }

    let next_thinking = match default_resolved.level {
        LevelSuffix::Inherit => cfg.thinking,
        LevelSuffix::Level(level) => level,
        LevelSuffix::None => bundle.default_thinking_level.unwrap_or(cfg.thinking),
    };

    cfg.llm = next_llm;
    cfg.thinking = next_thinking;
    Ok((default_resolved.alias, next_thinking, removed))
}

/// Switch to `[modelPresets.<name>]`, persisting included roles and deleting omitted roles from
/// `storage_path`. No-op (neither memory nor disk changes) if the preset's `default` is
/// unavailable.
///
/// # Errors
/// Preset does not exist, `default` or any role cannot resolve, or writing TOML fails.
pub(crate) fn apply_preset_switch(
    cfg: &mut Config,
    storage_path: &Path,
    name: &str,
) -> Result<(String, ThinkingLevel, String)> {
    let mut staged = cfg.clone();
    let (default_alias, next_thinking, removed) = apply_preset_in_memory(&mut staged, name)?;
    for role in &removed {
        let _ = crate::toml_write::delete_key(storage_path, &format!("roles.{role}"))?;
    }
    for (role, target) in &staged.llm.roles {
        crate::toml_write::set_key(
            storage_path,
            &format!("roles.{role}"),
            &toml::Value::String(target.clone()),
        )?;
    }
    *cfg = staged;
    let summary = cfg
        .llm
        .roles
        .iter()
        .map(|(r, m)| format!("{r}={m}"))
        .collect::<Vec<_>>()
        .join(" ");
    let cleared = if removed.is_empty() {
        String::new()
    } else {
        format!(" · cleared: {}", removed.join(", "))
    };
    Ok((
        default_alias,
        next_thinking,
        format!("switched modelpreset `{name}` ({summary}){cleared}"),
    ))
}

/// Write a preset snapshot back to `config_path` (omp's `/modelpreset save`): captures current
/// `[roles]` plus any active `--model` / session override into `roles.default` and the active
/// thinking level into `defaultThinkingLevel`.
///
/// # Errors
/// The config cannot be read, parsed or written.
pub(crate) fn save_preset(
    cfg: &mut Config,
    config_path: &Path,
    name: &str,
    active_model: Option<&str>,
    active_thinking: ThinkingLevel,
) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(HrnError::Config("preset name must be non-empty".into()));
    }
    let mut captured_roles = cfg.llm.roles.clone();
    if let Some(active) = active_model.map(str::trim).filter(|s| !s.is_empty()) {
        let current_default = expand_selector(&cfg.llm, Some("@default"))
            .ok()
            .map(|r| r.alias);
        if current_default.as_deref() != Some(active) {
            captured_roles.insert("default".to_owned(), active.to_owned());
        }
    }

    let mut bundle = toml::Table::new();
    let mut roles = toml::Table::new();
    for (role, target) in &captured_roles {
        roles.insert(role.clone(), toml::Value::String(target.clone()));
    }
    bundle.insert("modelRoles".to_owned(), toml::Value::Table(roles));
    bundle.insert(
        "defaultThinkingLevel".to_owned(),
        toml::Value::String(active_thinking.as_str().to_owned()),
    );
    crate::toml_write::set_key(
        config_path,
        &format!("modelPresets.{name}"),
        &toml::Value::Table(bundle),
    )?;
    cfg.llm.role_presets.insert(
        name.to_owned(),
        RolePreset {
            model_roles: captured_roles,
            default_thinking_level: Some(active_thinking),
        },
    );
    Ok(format!(
        "saved modelpreset `{name}` to {}",
        config_path.display()
    ))
}

/// Delete `[modelPresets.<name>]` from `config_path` and in-memory config.
///
/// # Errors
/// Preset does not exist or TOML editing fails.
pub(crate) fn delete_preset(cfg: &mut Config, config_path: &Path, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(HrnError::Config(
            "usage: /modelpreset delete <name>".to_owned(),
        ));
    }
    if !cfg.llm.role_presets.contains_key(name) {
        return Err(HrnError::Config(format!(
            "no modelpreset `{name}` · /modelpreset list"
        )));
    }
    let _ = crate::toml_write::delete_key(config_path, &format!("modelPresets.{name}"))?;
    cfg.llm.role_presets.remove(name);
    Ok(format!(
        "deleted modelpreset `{name}` from {}",
        config_path.display()
    ))
}

#[cfg(test)]
#[path = "roles_tests.rs"]
mod tests;
