//! Recursive model & role selector expansion (`@role`, `*`, ordered CSV, `:level`), cycle
//! detection, `enabled_models`/`disabled_providers` filtering, and `Ctrl+P` role cycling.

use crate::config::{LlmConfig, ModelPreset};
use hrn_kernel::{HrnError, LevelSuffix, Result, split_level_suffix};

/// Maximum role-to-role indirection depth before returning a configuration error.
pub(crate) const MAX_ROLE_DEPTH: usize = 8;

/// Default `Ctrl+P` cycle roles when `[llm] cycle` is unset.
pub(crate) const DEFAULT_CYCLE_ROLES: [&str; 3] = ["smol", "default", "slow"];

/// Semantic roles a profile or tool may ask for, one per line of `roles.txt`.
const STANDARD_ROLES: &str = include_str!("../roles.txt");

/// Result of resolving a model/role selector (alias + winning `:level` suffix).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSelector {
    /// Concrete preset key inside [`LlmConfig::presets`].
    pub alias: String,
    /// Winning thinking-level suffix (referrer's suffix overrides referee's).
    pub level: LevelSuffix,
}

/// One step in the `Ctrl+P` cycle list (role name or preset alias + resolved concrete alias).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CycleEntry {
    /// Display label (e.g. `"smol"`, `"default"`, `"slow"`, or a back-compat preset alias).
    pub label: String,
    /// Concrete resolved preset alias.
    pub alias: String,
    /// Optional thinking level from the role target's `:level` suffix.
    pub level: LevelSuffix,
}

/// Ordered list of standard role names declared in `roles.txt`.
pub fn standard_role_names() -> Vec<&'static str> {
    STANDARD_ROLES
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_whitespace().next())
        .collect()
}

/// Fallback roles for a standard role (`None` when `key` is not in `roles.txt`).
pub(crate) fn role_fallbacks(key: &str) -> Option<Vec<&'static str>> {
    STANDARD_ROLES
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::split_whitespace)
        .find(|words| words.clone().next() == Some(key))
        .map(|words| words.skip(1).collect())
}

/// Whether `preset` (keyed by `alias`) is allowed by `enabled_models` and `disabled_providers`.
pub fn is_preset_enabled(cfg: &LlmConfig, alias: &str, preset: &ModelPreset) -> bool {
    if cfg
        .disabled_providers
        .iter()
        .any(|p| p.eq_ignore_ascii_case(&preset.provider))
    {
        return false;
    }
    if cfg.enabled_models.is_empty() {
        return true;
    }
    let full = format!("{}/{}", preset.provider, preset.id);
    cfg.enabled_models.iter().any(|rule| {
        let trimmed = rule.trim();
        if let Some(prefix) = trimmed.strip_suffix('*') {
            let low = prefix.to_ascii_lowercase();
            alias.to_ascii_lowercase().starts_with(&low)
                || full.to_ascii_lowercase().starts_with(&low)
                || preset.id.to_ascii_lowercase().starts_with(&low)
        } else {
            alias.eq_ignore_ascii_case(trimmed)
                || full.eq_ignore_ascii_case(trimmed)
                || preset.id.eq_ignore_ascii_case(trimmed)
        }
    })
}

/// All currently enabled preset aliases in deterministic order.
pub fn enabled_presets(cfg: &LlmConfig) -> Vec<(&str, &ModelPreset)> {
    cfg.presets
        .iter()
        .filter(|(alias, preset)| is_preset_enabled(cfg, alias, preset))
        .map(|(alias, preset)| (alias.as_str(), preset))
        .collect()
}

/// Recursively expand a model/role selector supporting `@role`, `*` (= `@default`), ordered CSV
/// candidates (first available enabled preset wins), and `:level` suffixes (referrer wins).
///
/// # Errors
///
/// Returns `HrnError::Config` on role cycles, depth overflow (> 8), unknown names, or when every
/// candidate is unavailable/disabled.
pub fn expand_selector(cfg: &LlmConfig, requested: Option<&str>) -> Result<ResolvedSelector> {
    let raw = requested.map(str::trim).filter(|s| !s.is_empty());
    let expr = raw.unwrap_or("@default");
    let mut visited = Vec::new();
    expand_csv(cfg, expr, LevelSuffix::None, &mut visited)
}

/// Pick the concrete preset alias for a request (backwards-compatible wrapper over
/// [`expand_selector`]).
///
/// # Errors
///
/// Same as [`expand_selector`].
pub fn resolve_alias(cfg: &LlmConfig, requested: Option<&str>) -> Result<String> {
    expand_selector(cfg, requested).map(|r| r.alias)
}

#[path = "candidates_fallback.rs"]
mod fallback;
pub(crate) use fallback::{
    is_valid_fallback_owner, is_valid_fallback_target, resolve_chain, validate_fallback_chains,
};

/// Validate all configured `[roles]` entries for cycles, depth overflow, and dangling references.
///
/// # Errors
///
/// When a role cycle exists, indirection exceeds [`MAX_ROLE_DEPTH`], or a role references an
/// unknown preset/role.
pub(crate) fn validate_roles(cfg: &LlmConfig) -> Result {
    for role in cfg.roles.keys() {
        let mut visited = vec![role.clone()];
        let target = cfg.roles.get(role).map_or("", String::as_str);
        // Validate every entry structurally and detect cycles even if disabled by filters.
        validate_expr(cfg, role, target, &mut visited)?;
    }
    Ok(())
}

/// Build the effective `Ctrl+P` cycle list: roles from `cycle_order` (default `smol`, `default`,
/// `slow`) plus back-compat preset aliases, keeping only entries that resolve to an enabled preset.
pub fn effective_cycle_entries(cfg: &LlmConfig) -> Vec<CycleEntry> {
    let items: Vec<&str> = if cfg.cycle_order.is_empty() {
        DEFAULT_CYCLE_ROLES.to_vec()
    } else {
        cfg.cycle_order.iter().map(String::as_str).collect()
    };
    let mut entries = Vec::new();
    for item in items {
        let selector = if cfg.roles.contains_key(item) || role_fallbacks(item).is_some() {
            format!("@{item}")
        } else {
            item.to_owned()
        };
        if let Ok(resolved) = expand_selector(cfg, Some(&selector)) {
            entries.push(CycleEntry {
                label: item.to_owned(),
                alias: resolved.alias,
                level: resolved.level,
            });
        }
    }
    if entries.is_empty() {
        for (alias, _) in enabled_presets(cfg) {
            entries.push(CycleEntry {
                label: alias.to_owned(),
                alias: alias.to_owned(),
                level: LevelSuffix::None,
            });
        }
    }
    entries
}

/// Combine referrer and referee suffixes: an explicit referrer suffix always wins.
fn combine_level(referrer: LevelSuffix, referee: LevelSuffix) -> LevelSuffix {
    match referrer {
        LevelSuffix::None => referee,
        explicit => explicit,
    }
}

fn expand_csv(
    cfg: &LlmConfig,
    expr: &str,
    inherited_level: LevelSuffix,
    visited: &mut Vec<String>,
) -> Result<ResolvedSelector> {
    let parts: Vec<&str> = expr
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return expand_single(cfg, "@default", inherited_level, visited);
    }
    let multi = parts.len() > 1;
    let mut last_err: Option<HrnError> = None;
    for part in parts {
        match expand_single(cfg, part, inherited_level, visited) {
            Ok(resolved) => return Ok(resolved),
            Err(err) => {
                if !multi || is_cycle_error(&err) {
                    return Err(err);
                }
                last_err = Some(err);
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| HrnError::Config(format!("no available model matched `{expr}`"))))
}

fn expand_single(
    cfg: &LlmConfig,
    token: &str,
    inherited_level: LevelSuffix,
    visited: &mut Vec<String>,
) -> Result<ResolvedSelector> {
    let (raw_base, own_level) = split_level_suffix(token.trim());
    let effective_level = combine_level(inherited_level, own_level);
    let base = raw_base.trim();

    if base == "*" {
        return expand_role(cfg, "default", effective_level, visited);
    }
    if let Some(role_name) = base.strip_prefix('@') {
        return expand_role(cfg, role_name.trim(), effective_level, visited);
    }

    // Direct preset alias match (must be enabled).
    if let Some(preset) = cfg.presets.get(base) {
        if is_preset_enabled(cfg, base, preset) {
            return Ok(ResolvedSelector {
                alias: base.to_owned(),
                level: effective_level,
            });
        }
        return Err(HrnError::Config(format!(
            "model `{base}` is disabled by `enabled_models` / `disabled_providers`"
        )));
    }

    // OpenRouter routing variant suffix (`:nitro`, `:floor`, `:online`, `:exacto`, `:extended`)
    // stripped for preset lookup and forwarded onto the upstream model id at dispatch time.
    if let Some((stripped, _)) = strip_openrouter_variant(base)
        && let Some(preset) = cfg.presets.get(stripped)
        && preset.provider == "openrouter"
    {
        if is_preset_enabled(cfg, stripped, preset) {
            return Ok(ResolvedSelector {
                alias: stripped.to_owned(),
                level: effective_level,
            });
        }
        return Err(HrnError::Config(format!(
            "model `{base}` is disabled by `enabled_models` / `disabled_providers`"
        )));
    }

    // Match by `<provider>/<id>` when preset is keyed under a short alias.
    if let Some((provider, id)) = base.split_once('/')
        && let Some((alias, preset)) = cfg
            .presets
            .iter()
            .find(|(_, p)| p.provider == provider && p.id == id)
    {
        if is_preset_enabled(cfg, alias, preset) {
            return Ok(ResolvedSelector {
                alias: alias.clone(),
                level: effective_level,
            });
        }
        return Err(HrnError::Config(format!(
            "model `{base}` is disabled by `enabled_models` / `disabled_providers`"
        )));
    }

    // Bare role name (configured or standard role in `roles.txt`).
    if cfg.roles.contains_key(base) || role_fallbacks(base).is_some() {
        return expand_role(cfg, base, effective_level, visited);
    }

    unknown_model_err(cfg, base)
}

fn expand_role(
    cfg: &LlmConfig,
    role: &str,
    inherited_level: LevelSuffix,
    visited: &mut Vec<String>,
) -> Result<ResolvedSelector> {
    if visited.iter().any(|v| v == role) {
        return Err(HrnError::Config(format!(
            "role cycle detected involving `{role}`"
        )));
    }
    if visited.len() >= MAX_ROLE_DEPTH {
        return Err(HrnError::Config(format!(
            "role cycle / indirection depth exceeded ({MAX_ROLE_DEPTH}) at `{role}`"
        )));
    }

    visited.push(role.to_owned());
    let outcome = if let Some(target) = cfg.roles.get(role) {
        expand_csv(cfg, target, inherited_level, visited)
    } else if role != "default"
        && let Some(fallbacks) = role_fallbacks(role)
    {
        expand_standard_fallbacks(cfg, &fallbacks, inherited_level, visited)
    } else if role == "default" {
        enabled_presets(cfg)
            .first()
            .map(|(alias, _)| ResolvedSelector {
                alias: (*alias).to_owned(),
                level: inherited_level,
            })
            .ok_or_else(|| HrnError::Config("no enabled [llm.presets] configured".to_owned()))
    } else {
        unknown_model_err(cfg, role)
    };
    visited.pop();
    outcome
}

fn expand_standard_fallbacks(
    cfg: &LlmConfig,
    fallbacks: &[&str],
    inherited_level: LevelSuffix,
    visited: &mut Vec<String>,
) -> Result<ResolvedSelector> {
    for &fb in fallbacks {
        if visited.iter().any(|v| v == fb) {
            continue;
        }
        if let Some(preset) = cfg.presets.get(fb)
            && is_preset_enabled(cfg, fb, preset)
        {
            return Ok(ResolvedSelector {
                alias: fb.to_owned(),
                level: inherited_level,
            });
        }
        if cfg.roles.contains_key(fb) {
            match expand_role(cfg, fb, inherited_level, visited) {
                Ok(found) => return Ok(found),
                Err(err) if is_cycle_error(&err) => return Err(err),
                Err(_) => {}
            }
        }
    }
    expand_role(cfg, "default", inherited_level, visited)
}

fn validate_expr(
    cfg: &LlmConfig,
    origin_role: &str,
    expr: &str,
    visited: &mut Vec<String>,
) -> Result {
    let parts: Vec<&str> = expr
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(HrnError::Config(format!(
            "role `{origin_role}` has an empty target"
        )));
    }
    let multi = parts.len() > 1;
    let mut any_valid = false;
    let mut last_err: Option<HrnError> = None;

    for part in parts {
        match validate_single_target(cfg, origin_role, part, visited) {
            Ok(()) => any_valid = true,
            Err(err) => {
                if !multi || is_cycle_error(&err) {
                    return Err(err);
                }
                last_err = Some(err);
            }
        }
    }
    if any_valid {
        Ok(())
    } else {
        Err(last_err.unwrap_or_else(|| {
            HrnError::Config(format!(
                "role `{origin_role}` references no known preset or role in `{expr}`"
            ))
        }))
    }
}

fn validate_single_target(
    cfg: &LlmConfig,
    origin_role: &str,
    token: &str,
    visited: &mut Vec<String>,
) -> Result {
    let (raw_base, _) = split_level_suffix(token.trim());
    let base = raw_base.trim();
    let role_ref = if base == "*" {
        Some("default")
    } else if let Some(r) = base.strip_prefix('@') {
        Some(r.trim())
    } else if cfg.presets.contains_key(base)
        || base
            .split_once('/')
            .is_some_and(|(p, id)| cfg.presets.values().any(|m| m.provider == p && m.id == id))
    {
        return Ok(());
    } else if cfg.roles.contains_key(base) || role_fallbacks(base).is_some() {
        Some(base)
    } else {
        None
    };

    let Some(target_role) = role_ref else {
        return Err(HrnError::Config(format!(
            "role `{origin_role}` references unknown preset `{base}`"
        )));
    };
    if visited.iter().any(|v| v == target_role) {
        return Err(HrnError::Config(format!(
            "role cycle detected involving `{target_role}`"
        )));
    }
    if visited.len() >= MAX_ROLE_DEPTH {
        return Err(HrnError::Config(format!(
            "role cycle / indirection depth exceeded ({MAX_ROLE_DEPTH}) at `{target_role}`"
        )));
    }
    if let Some(next_expr) = cfg.roles.get(target_role) {
        visited.push(target_role.to_owned());
        let res = validate_expr(cfg, target_role, next_expr, visited);
        visited.pop();
        return res;
    }
    if target_role == "default" || role_fallbacks(target_role).is_some() {
        return Ok(());
    }
    Err(HrnError::Config(format!(
        "role `{origin_role}` references unknown role `{target_role}`"
    )))
}

/// Strip an `OpenRouter` routing variant suffix (`:nitro`, `:floor`, `:online`, `:exacto`,
/// `:extended`) from a selector base, returning `(base_without_variant, variant_with_colon)`.
pub(crate) fn strip_openrouter_variant(selector: &str) -> Option<(&str, &str)> {
    const VARIANTS: [&str; 5] = [":nitro", ":floor", ":online", ":exacto", ":extended"];
    let (base, _) = split_level_suffix(selector.trim());
    let base = base.trim();
    for suffix in VARIANTS {
        if let Some(prefix) = base.strip_suffix(suffix) {
            return Some((prefix, suffix));
        }
    }
    None
}

fn is_cycle_error(err: &HrnError) -> bool {
    err.to_string().contains("role cycle")
}

fn unknown_model_err(cfg: &LlmConfig, key: &str) -> Result<ResolvedSelector> {
    if cfg.presets.is_empty() {
        return Err(HrnError::Config("no [llm.presets] configured".into()));
    }
    let known: Vec<&str> = cfg
        .presets
        .keys()
        .chain(cfg.roles.keys())
        .map(String::as_str)
        .collect();
    Err(HrnError::Config(format!(
        "unknown model or role `{key}` (known: {})",
        known.join(", ")
    )))
}

#[cfg(test)]
#[path = "candidates_tests.rs"]
mod tests;
