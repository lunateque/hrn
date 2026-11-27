//! Fallback-chain resolution and validation supporting exact preset keys, semantic role keys,
//! `<provider>/*` wildcards, `:level` suffixes, and `@role` / role targets.

use crate::candidates::{expand_selector, is_preset_enabled, role_fallbacks};
use crate::config::{LlmConfig, ModelPreset};
use hrn_kernel::{HrnError, LevelSuffix, Result, split_level_suffix};

/// Resolve a requested selector into its ordered enabled fallback chain and winning `:level`.
///
/// Consults `cfg.fallback_chains` in priority order:
/// 1. `resolved.alias` (exact model key)
/// 2. requested role name (`None` / `"*"` -> `"default"`, `@role` or bare role name)
/// 3. provider wildcard `<provider>/*`
///
/// # Errors
///
/// When the selector cannot be resolved or no enabled preset exists in the chain.
pub(crate) fn resolve_chain(
    cfg: &LlmConfig,
    requested: Option<&str>,
) -> Result<(Vec<(String, ModelPreset)>, LevelSuffix)> {
    let resolved = expand_selector(cfg, requested)?;
    let mut out = Vec::new();
    let mut provider_wildcard: Option<String> = None;
    if let Some(primary) = cfg.presets.get(&resolved.alias)
        && is_preset_enabled(cfg, &resolved.alias, primary)
    {
        provider_wildcard = Some(format!("{}/*", primary.provider));
        out.push((resolved.alias.clone(), primary.clone()));
    }
    let mut chain_keys: Vec<String> = vec![resolved.alias.clone()];
    if let Some(role) = requested_role_name(cfg, requested)
        && !chain_keys.iter().any(|k| k == role)
    {
        chain_keys.push(role.to_owned());
    }
    if let Some(wild) = provider_wildcard
        && !chain_keys.iter().any(|k| k == &wild)
    {
        chain_keys.push(wild);
    }
    for key in &chain_keys {
        if let Some(fallbacks) = cfg.fallback_chains.get(key) {
            for fb in fallbacks {
                append_fallback_target(cfg, fb, &mut out);
            }
        }
    }
    if out.is_empty() {
        return Err(HrnError::Config(format!(
            "no enabled LLM preset matched `{}`",
            resolved.alias
        )));
    }
    Ok((out, resolved.level))
}

fn requested_role_name<'a>(cfg: &LlmConfig, requested: Option<&'a str>) -> Option<&'a str> {
    let Some(raw) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return Some("default");
    };
    if raw.contains(',') {
        return None;
    }
    let (base, _) = split_level_suffix(raw);
    let base = base.trim();
    if base == "*" {
        return Some("default");
    }
    if let Some(role) = base.strip_prefix('@').map(str::trim) {
        return (!role.is_empty()).then_some(role);
    }
    if !cfg.presets.contains_key(base)
        && (cfg.roles.contains_key(base) || role_fallbacks(base).is_some())
    {
        return Some(base);
    }
    None
}

fn append_fallback_target(cfg: &LlmConfig, target: &str, out: &mut Vec<(String, ModelPreset)>) {
    let resolved = if let Some(preset) = cfg.presets.get(target) {
        is_preset_enabled(cfg, target, preset).then(|| (target.to_owned(), preset.clone()))
    } else {
        expand_selector(cfg, Some(target)).ok().and_then(|r| {
            let preset = cfg.presets.get(&r.alias)?;
            if !is_preset_enabled(cfg, &r.alias, preset) {
                return None;
            }
            let mut cloned = preset.clone();
            if let LevelSuffix::Level(lvl) = r.level {
                cloned.thinking = Some(lvl);
            }
            Some((r.alias, cloned))
        })
    };
    if let Some((alias, preset)) = resolved
        && !out.iter().any(|(existing, _)| existing == &alias)
    {
        out.push((alias, preset));
    }
}

/// Whether `owner` is a valid key in `fallback_chains`: a known preset, a role name (configured
/// or standard), or a `<provider>/*` wildcard for a known provider.
pub(crate) fn is_valid_fallback_owner(cfg: &LlmConfig, owner: &str) -> bool {
    let key = owner.trim();
    if key.is_empty() {
        return false;
    }
    if cfg.presets.contains_key(key)
        || key == "default"
        || cfg.roles.contains_key(key)
        || role_fallbacks(key).is_some()
    {
        return true;
    }
    key.strip_suffix("/*").is_some_and(|prov| {
        !prov.is_empty()
            && (cfg.providers.contains_key(prov)
                || cfg.presets.values().any(|p| p.provider == prov))
    })
}

/// Whether `target` is a valid entry inside a `fallback_chains` list: a known preset (with
/// optional `:level` suffix), `<provider>/<id>`, or a known role / `@role` selector.
pub(crate) fn is_valid_fallback_target(cfg: &LlmConfig, target: &str) -> bool {
    let (raw_base, _) = split_level_suffix(target.trim());
    let base = raw_base.trim();
    if base.is_empty() {
        return false;
    }
    if base == "*"
        || cfg.presets.contains_key(base)
        || base
            .split_once('/')
            .is_some_and(|(p, id)| cfg.presets.values().any(|m| m.provider == p && m.id == id))
    {
        return true;
    }
    let role = base.strip_prefix('@').map_or(base, str::trim);
    !role.is_empty()
        && (role == "default" || cfg.roles.contains_key(role) || role_fallbacks(role).is_some())
}

/// Validate all entries in `cfg.fallback_chains`.
pub(crate) fn validate_fallback_chains(cfg: &LlmConfig) -> Result {
    for (owner, chain) in &cfg.fallback_chains {
        if !is_valid_fallback_owner(cfg, owner) {
            return Err(HrnError::Config(format!(
                "fallback_chains key references unknown preset, role, or provider `{owner}`"
            )));
        }
        for alias in chain {
            if !is_valid_fallback_target(cfg, alias) {
                return Err(HrnError::Config(format!(
                    "fallback chain of `{owner}` references unknown preset or role `{alias}`"
                )));
            }
        }
    }
    Ok(())
}
