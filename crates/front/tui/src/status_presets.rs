//! Embedded statusline presets and separator styles loaded from `data/statusline.presets.toml`.

use crate::port_theme::{SeparatorStyle, StatuslineConfig, StatuslinePreset};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::OnceLock;

const PRESETS_TOML: &str = include_str!("../data/statusline.presets.toml");

/// Glyphs for one separator style (`inner` joins segments; `left_cap`/`right_cap` close surface pills).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SeparatorGlyphs {
    pub(crate) inner: String,
    pub(crate) left_cap: String,
    pub(crate) right_cap: String,
}

/// Segment lists for one named statusline preset.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PresetSpec {
    #[serde(default)]
    pub(crate) left_segments: Vec<String>,
    #[serde(default)]
    pub(crate) right_segments: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetsFile {
    separators: BTreeMap<String, SeparatorGlyphs>,
    presets: BTreeMap<String, PresetSpec>,
}

static CATALOG: OnceLock<PresetsFile> = OnceLock::new();
static FALLBACK_SEP: OnceLock<SeparatorGlyphs> = OnceLock::new();
static FALLBACK_ASCII_SEP: OnceLock<SeparatorGlyphs> = OnceLock::new();

fn catalog() -> &'static PresetsFile {
    CATALOG.get_or_init(|| {
        toml::from_str(PRESETS_TOML).unwrap_or_else(|_| PresetsFile {
            separators: BTreeMap::new(),
            presets: BTreeMap::new(),
        })
    })
}

fn ascii_sep() -> &'static SeparatorGlyphs {
    catalog().separators.get("ascii").unwrap_or_else(|| {
        FALLBACK_ASCII_SEP.get_or_init(|| SeparatorGlyphs {
            inner: "|".to_owned(),
            left_cap: String::new(),
            right_cap: String::new(),
        })
    })
}

fn default_sep() -> &'static SeparatorGlyphs {
    catalog().separators.get("powerline").unwrap_or_else(|| {
        FALLBACK_SEP.get_or_init(|| SeparatorGlyphs {
            inner: ">".to_owned(),
            left_cap: String::new(),
            right_cap: String::new(),
        })
    })
}

/// Resolve the separator glyphs for `style`, degrading non-ASCII glyphs to pure ASCII when
/// `ascii_only` is active.
pub(crate) fn resolve_separator(
    style: SeparatorStyle,
    ascii_only: bool,
) -> &'static SeparatorGlyphs {
    let sep = catalog()
        .separators
        .get(style.as_str())
        .unwrap_or_else(default_sep);
    if ascii_only
        && (!sep.inner.is_ascii() || !sep.left_cap.is_ascii() || !sep.right_cap.is_ascii())
    {
        return ascii_sep();
    }
    sep
}

/// Resolved segment lists for `cfg`: `(left_segments, right_segments, show_gauge)`.
pub(crate) fn resolve_segments(cfg: &StatuslineConfig) -> (Vec<String>, Vec<String>, bool) {
    let preset = catalog().presets.get(cfg.preset.as_str());
    let raw_left: Vec<String> =
        if cfg.preset == StatuslinePreset::Custom || !cfg.left_segments.is_empty() {
            cfg.left_segments.clone()
        } else {
            preset.map_or_else(Vec::new, |p| p.left_segments.clone())
        };
    let raw_right: Vec<String> =
        if cfg.preset == StatuslinePreset::Custom || !cfg.right_segments.is_empty() {
            cfg.right_segments.clone()
        } else {
            preset.map_or_else(Vec::new, |p| p.right_segments.clone())
        };
    let has_gauge = raw_left.iter().any(|s| s.eq_ignore_ascii_case("gauge"))
        || raw_right.iter().any(|s| s.eq_ignore_ascii_case("gauge"));
    let left = raw_left
        .into_iter()
        .filter(|s| !s.eq_ignore_ascii_case("gauge"))
        .collect();
    let right = raw_right
        .into_iter()
        .filter(|s| !s.eq_ignore_ascii_case("gauge"))
        .collect();
    (left, right, has_gauge)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_statusline_presets_define_all_presets_and_separators() {
        let parsed: PresetsFile =
            toml::from_str(PRESETS_TOML).expect("valid statusline.presets.toml");
        for name in [
            "default", "minimal", "compact", "full", "nerd", "ascii", "custom",
        ] {
            assert!(
                parsed.presets.contains_key(name),
                "missing preset `{name}` in statusline.presets.toml"
            );
        }
        for name in [
            "powerline",
            "powerline-thin",
            "slash",
            "pipe",
            "block",
            "none",
            "ascii",
        ] {
            assert!(
                parsed.separators.contains_key(name),
                "missing separator `{name}` in statusline.presets.toml"
            );
        }
    }
}
