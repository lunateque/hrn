//! The base-prompt variant a model gets: an explicit per-model declaration wins, otherwise
//! the embedded family table maps the model id to a variant. Selection is data, not code:
//! `prompt-families.txt` lists `pattern = variant` lines matched against the lower-cased
//! model id (`*` matches within a `-`/`.`-spelled name), first hit in file order wins.

use crate::candidates::resolve_alias;
use crate::config::LlmConfig;

/// The embedded family table (`pattern<TAB-or-space>=<space>variant`).
const FAMILIES: &str = include_str!("../prompt-families.txt");

/// The variant for the model `model` resolves to (alias, role or `provider/id`): the preset's
/// declared `prompt_variant`, else the first family-table match on the model id, else `None`
/// (the default `base.md`).
#[must_use]
pub(crate) fn resolve(cfg: &LlmConfig, model: Option<&str>) -> Option<String> {
    let alias = resolve_alias(cfg, model).ok()?;
    let preset = cfg.presets.get(&alias)?;
    if let Some(variant) = &preset.prompt_variant {
        return Some(variant.clone());
    }
    let id = preset.id.to_lowercase();
    let qualified = format!("{}/{}", preset.provider, preset.id).to_lowercase();
    families()
        .into_iter()
        .find(|(pattern, _)| matches_pattern(pattern, &id) || matches_pattern(pattern, &qualified))
        .map(|(_, variant)| variant)
}

/// The `(pattern, variant)` pairs of the table, in file order.
fn families() -> Vec<(String, String)> {
    FAMILIES
        .lines()
        .filter_map(|line| {
            let (pattern, variant) = line.split_once('=')?;
            let pattern = pattern.trim().to_lowercase();
            let variant = variant.trim().to_owned();
            (!pattern.is_empty() && !variant.is_empty()).then_some((pattern, variant))
        })
        .collect()
}

/// `pattern` against `id`: `*` matches any run of characters inside the name (it is required
/// only when the pattern would otherwise end where the id continues, i.e. every pattern
/// without `*` must equal the id exactly).
fn matches_pattern(pattern: &str, id: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == id;
    }
    let mut rest = id;
    let mut first = true;
    for chunk in pattern.split('*') {
        if chunk.is_empty() {
            first = false;
            continue;
        }
        let found = if first {
            rest.strip_prefix(chunk)
        } else {
            rest.find(chunk)
                .map(|at| rest.get(at + chunk.len()..).unwrap_or_default())
        };
        let Some(tail) = found else { return false };
        rest = tail;
        first = false;
    }
    if pattern.ends_with('*') {
        return true;
    }
    // The last chunk must sit at the end of the id.
    id.ends_with(pattern.rsplit('*').next().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_table_maps_the_known_families() {
        let table = families();
        assert!(
            table
                .iter()
                .any(|(p, v)| p == "claude-opus-5*" && v == "claude")
        );
        assert!(table.iter().any(|(p, v)| p == "gpt-5*" && v == "gpt5"));
    }

    #[test]
    fn star_patterns_match_names_but_never_the_wrong_prefix() {
        assert!(matches_pattern("gpt-5*", "gpt-5.6-luna"));
        assert!(matches_pattern("gpt-5*", "gpt-5"));
        assert!(!matches_pattern("gpt-5*", "gpt-4"));
        assert!(matches_pattern("claude-fable-*", "claude-fable-5-2"));
        assert!(!matches_pattern("claude-fable-*", "claude-fable"));
        assert!(matches_pattern("exact", "exact"));
        assert!(!matches_pattern("exact", "exactly"));
    }
}
