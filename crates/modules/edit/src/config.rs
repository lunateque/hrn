//! Typed `[edit]` configuration, mode resolution (`model_variants`, env pins, model-family
//! fallback), and embedded data tables (`defaults.json`, `replace-families.txt`,
//! `generated-globs.txt`).

use hrn_kernel::serde::Deserialize;
use hrn_kernel::serde_json;
use hrn_kernel::{HrnError, Result};
use std::collections::BTreeMap;

/// Embedded `[edit]` defaults (kept inside `hrn-mod-edit` for strict module removability).
const DEFAULTS_JSON: &str = include_str!("../data/defaults.json");
/// Model-family substrings that fall back to `replace` unless `HRN_STRICT_EDIT_MODE` is set.
const REPLACE_FAMILIES_TXT: &str = include_str!("../data/replace-families.txt");
/// Auto-generated and lockfile glob patterns refused when `block_auto_generated = true`.
const GENERATED_GLOBS_TXT: &str = include_str!("../data/generated-globs.txt");

/// Active file-editing surface selected by `[edit]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditMode {
    /// Line-anchored patches against `[path#TAG]` snapshots.
    #[default]
    Hashline,
    /// Search-replace (`path`, `old`, `new`) through the 4-step match ladder.
    Replace,
    /// Codex multi-file patch format exposed as `edit`.
    ApplyPatch,
    /// Unregister `edit` and drop hashline anchors in `read` / `grep`.
    None,
}

impl EditMode {
    /// Canonical lowercase label for diagnostics and hints.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hashline => "hashline",
            Self::Replace => "replace",
            Self::ApplyPatch => "apply_patch",
            Self::None => "none",
        }
    }

    /// Whether `read` and `grep` should emit `[path#TAG]` hashline anchors.
    #[must_use]
    pub const fn uses_hashline_anchors(self) -> bool {
        matches!(self, Self::Hashline)
    }
}

/// Parse an edit mode string (accepts `"patch"` / `"sloppy"` as aliases of `"replace"` and
/// `"magic"` as ignored fallback to `"hashline"` per §4 divergence).
///
/// Returns `(mode, magic_ignored)`.
///
/// # Errors
/// Returns `HrnError::Config` for unrecognized mode names.
pub fn parse_mode(raw: &str) -> Result<(EditMode, bool)> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "hashline" => Ok((EditMode::Hashline, false)),
        "replace" | "patch" | "sloppy" => Ok((EditMode::Replace, false)),
        "apply_patch" => Ok((EditMode::ApplyPatch, false)),
        "none" => Ok((EditMode::None, false)),
        "magic" => Ok((EditMode::Hashline, true)),
        other => Err(HrnError::Config(format!(
            "invalid edit mode `{other}` (expected hashline, replace, patch, sloppy, apply_patch, or none)"
        ))),
    }
}

#[derive(Debug, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "deserializes [edit] boolean defaults from data/defaults.json"
)]
struct RawDefaults {
    mode: String,
    model_variants: BTreeMap<String, String>,
    fuzzy_match: bool,
    fuzzy_threshold: f64,
    block_auto_generated: bool,
    enforce_seen_lines: bool,
    recover_inline_edits: bool,
    auto_repair: bool,
    blackbox: bool,
}

/// Raw `[edit]` config table deserialized from `Registry::config()`.
#[derive(Debug, Default, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
pub(crate) struct RawEditTable {
    pub(crate) mode: Option<String>,
    pub(crate) model_variants: Option<BTreeMap<String, String>>,
    pub(crate) fuzzy_match: Option<bool>,
    pub(crate) fuzzy_threshold: Option<f64>,
    pub(crate) block_auto_generated: Option<bool>,
    pub(crate) enforce_seen_lines: Option<bool>,
    pub(crate) recover_inline_edits: Option<bool>,
    pub(crate) auto_repair: Option<bool>,
    pub(crate) blackbox: Option<bool>,
}

/// Validated `[edit]` module configuration.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors omp [edit] boolean policy switches"
)]
pub struct EditConfig {
    /// Configured mode string (default `"hashline"`).
    pub mode: String,
    /// Whether `mode` was explicitly set by user/env config rather than embedded defaults.
    pub mode_explicit: bool,
    /// Per-model substring overrides (longest case-insensitive substring wins; order breaks ties).
    pub model_variants: BTreeMap<String, String>,
    /// Enable whitespace, indentation, and fuzzy-similarity fallbacks.
    pub fuzzy_match: bool,
    /// Similarity ratio threshold (`0.0..=1.0`, default `0.95`).
    pub fuzzy_threshold: f32,
    /// Refuse edits to lockfiles and auto-generated paths.
    pub block_auto_generated: bool,
    /// Refuse edits targeting lines not yet displayed by `read`.
    pub enforce_seen_lines: bool,
    /// Recover accidental inline edit blocks emitted in assistant prose.
    pub recover_inline_edits: bool,
    /// Ask `@smol` to repair introduced AST syntax regressions (only accepted on clean reparse).
    pub auto_repair: bool,
    /// Append full file source to tool output when an edit introduces a parse regression.
    pub blackbox: bool,
}

impl Default for EditConfig {
    fn default() -> Self {
        let d: RawDefaults = serde_json::from_str(DEFAULTS_JSON).unwrap_or(RawDefaults {
            mode: "hashline".to_owned(),
            model_variants: BTreeMap::new(),
            fuzzy_match: true,
            fuzzy_threshold: 0.95,
            block_auto_generated: true,
            enforce_seen_lines: true,
            recover_inline_edits: true,
            auto_repair: false,
            blackbox: false,
        });
        Self {
            mode: d.mode,
            mode_explicit: false,
            model_variants: d.model_variants,
            fuzzy_match: d.fuzzy_match,
            fuzzy_threshold: f64_to_threshold(d.fuzzy_threshold),
            block_auto_generated: d.block_auto_generated,
            enforce_seen_lines: d.enforce_seen_lines,
            recover_inline_edits: d.recover_inline_edits,
            auto_repair: d.auto_repair,
            blackbox: d.blackbox,
        }
    }
}

/// Outcome of resolving the active [`EditMode`] against model variants, env pins, and families.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedMode {
    /// Selected editor mode.
    pub mode: EditMode,
    /// True when `"magic"` was parsed and ignored (§4 deliberate divergence).
    pub magic_ignored: bool,
    /// Optional diagnostic/debug note explaining a fallback or ignored setting.
    pub debug_note: Option<String>,
}

impl EditConfig {
    /// Overlay parsed `[edit]` table fields over embedded `data/defaults.json` and validate.
    ///
    /// # Errors
    /// Returns `HrnError::Config` for invalid mode names, invalid `model_variants` values, or
    /// out-of-range `fuzzy_threshold`.
    pub(crate) fn from_raw(raw: RawEditTable) -> Result<Self> {
        let mut cfg = Self::default();
        if let Some(mode) = raw.mode {
            let _ = parse_mode(&mode)?;
            cfg.mode = mode;
            cfg.mode_explicit = true;
        }
        if let Some(variants) = raw.model_variants {
            for (needle, val) in &variants {
                if needle.trim().is_empty() {
                    return Err(HrnError::Config(
                        "[edit] model_variants keys must not be empty".to_owned(),
                    ));
                }
                let _ = parse_mode(val)?;
            }
            cfg.model_variants = variants;
        }
        if let Some(v) = raw.fuzzy_match {
            cfg.fuzzy_match = v;
        }
        if let Some(t) = raw.fuzzy_threshold {
            if !t.is_finite() || !(0.0..=1.0).contains(&t) {
                return Err(HrnError::Config(format!(
                    "[edit] fuzzy_threshold must be a finite number in 0.0..=1.0, got {t}"
                )));
            }
            cfg.fuzzy_threshold = f64_to_threshold(t);
        }
        if let Some(v) = raw.block_auto_generated {
            cfg.block_auto_generated = v;
        }
        if let Some(v) = raw.enforce_seen_lines {
            cfg.enforce_seen_lines = v;
        }
        if let Some(v) = raw.recover_inline_edits {
            cfg.recover_inline_edits = v;
        }
        if let Some(v) = raw.auto_repair {
            cfg.auto_repair = v;
        }
        if let Some(v) = raw.blackbox {
            cfg.blackbox = v;
        }
        Ok(cfg)
    }

    /// Resolve effective [`EditMode`] using `active_models`, `HRN_EDIT_VARIANT`, and
    /// `HRN_STRICT_EDIT_MODE`.
    ///
    /// Precedence:
    /// 1. `model_variants` (longest case-insensitive substring; earlier order breaks ties)
    /// 2. `HRN_EDIT_VARIANT` (non-empty and not `"auto"`)
    /// 3. Explicit `[edit] mode` (non-default or when `HRN_STRICT_EDIT_MODE` is enabled)
    /// 4. Data-driven model-family fallback (`data/replace-families.txt` -> `replace`)
    /// 5. Configured default (`"hashline"`)
    pub fn resolve_mode(
        &self,
        active_models: &[String],
        env_variant: Option<&str>,
        strict_env: Option<&str>,
    ) -> Result<ResolvedMode> {
        let ordered: Vec<(String, String)> = self
            .model_variants
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.resolve_mode_with_order(&ordered, active_models, env_variant, strict_env)
    }

    /// Like [`Self::resolve_mode`], accepting an explicitly ordered `(needle, mode)` slice so
    /// equal-length tie-breaking preserves caller/config order.
    pub fn resolve_mode_with_order(
        &self,
        variants: &[(String, String)],
        active_models: &[String],
        env_variant: Option<&str>,
        strict_env: Option<&str>,
    ) -> Result<ResolvedMode> {
        if let Some(matched_mode) = match_model_variant(variants, active_models) {
            return Ok(wrap_parsed(parse_mode(matched_mode)?));
        }
        if let Some(env_raw) = env_variant.map(str::trim).filter(|s| !s.is_empty())
            && !env_raw.eq_ignore_ascii_case("auto")
        {
            return Ok(wrap_parsed(parse_mode(env_raw)?));
        }
        let strict = strict_env.is_some_and(parse_flag_true);
        let (cfg_mode, cfg_magic) = parse_mode(&self.mode)?;
        if self.mode_explicit && (!matches!(cfg_mode, EditMode::Hashline) || cfg_magic || strict) {
            return Ok(wrap_parsed((cfg_mode, cfg_magic)));
        }
        if !strict && matches_replace_family(active_models) {
            return Ok(ResolvedMode {
                mode: EditMode::Replace,
                magic_ignored: false,
                debug_note: None,
            });
        }
        Ok(wrap_parsed((cfg_mode, cfg_magic)))
    }

    /// Auto-generated / lockfile globs active for this config (empty when disabled).
    #[must_use]
    pub fn blocked_globs(&self) -> Vec<String> {
        if self.block_auto_generated {
            generated_globs()
        } else {
            Vec::new()
        }
    }
}

fn wrap_parsed((mode, magic_ignored): (EditMode, bool)) -> ResolvedMode {
    let debug_note = magic_ignored.then(|| {
        "edit.mode = \"magic\" is not supported in hrn (§4 divergence); using hashline".to_owned()
    });
    ResolvedMode {
        mode,
        magic_ignored,
        debug_note,
    }
}

/// Find the longest case-insensitive substring key in `variants` present in any `active_models`
/// string; strictly greater length wins so earlier config entries break ties.
pub(crate) fn match_model_variant<'a>(
    variants: &'a [(String, String)],
    active_models: &[String],
) -> Option<&'a str> {
    if variants.is_empty() || active_models.is_empty() {
        return None;
    }
    let lowered_models: Vec<String> = active_models
        .iter()
        .map(|m| m.to_ascii_lowercase())
        .collect();
    let mut best: Option<(usize, &'a str)> = None;
    for (needle, mode) in variants {
        let key = needle.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        if lowered_models.iter().any(|m| m.contains(&key)) {
            let len = key.len();
            if best.is_none_or(|(best_len, _)| len > best_len) {
                best = Some((len, mode.as_str()));
            }
        }
    }
    best.map(|(_, m)| m)
}

/// Whether any active model string matches a family in `data/replace-families.txt`.
#[must_use]
pub fn matches_replace_family(active_models: &[String]) -> bool {
    let families = replace_families();
    active_models.iter().any(|model| {
        let low = model.to_ascii_lowercase();
        families.iter().any(|fam| low.contains(fam))
    })
}

/// Data-driven list of model-family substrings defaulting to `replace`.
#[must_use]
pub fn replace_families() -> Vec<String> {
    parse_data_lines(REPLACE_FAMILIES_TXT)
        .into_iter()
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// Data-driven list of lockfile and auto-generated file globs.
#[must_use]
pub fn generated_globs() -> Vec<String> {
    parse_data_lines(GENERATED_GLOBS_TXT)
}

fn parse_data_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

fn parse_flag_true(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "y" | "true" | "yes" | "on"
    )
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "threshold is validated finite in 0.0..=1.0 before narrowing f64 to f32"
)]
fn f64_to_threshold(val: f64) -> f32 {
    val.clamp(0.0, 1.0) as f32
}
