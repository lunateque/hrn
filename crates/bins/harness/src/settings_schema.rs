//! Declarative settings schema (`data/settings.schema.toml`), typed parsing, secret masking,
//! and fuzzy key suggestions for `hrn config` and `cfg://`.

use hrn_kernel::{HrnError, Result};
use serde::Deserialize;
use std::sync::OnceLock;

/// Embedded baseline core defaults (layer 1).
pub(crate) const DEFAULTS_TOML: &str = include_str!("../data/config.defaults.toml");
/// Embedded settings schema metadata.
const SCHEMA_TOML: &str = include_str!("../data/settings.schema.toml");

/// Value kind declared for one schema entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SettingKind {
    String,
    Bool,
    Integer,
    Float,
    Array,
    Table,
}

impl SettingKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Bool => "bool",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::Array => "array",
            Self::Table => "table",
        }
    }
}

/// Merge strategy for array settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MergeRule {
    #[default]
    Replace,
    Prepend,
}

/// One enumerable setting row from `data/settings.schema.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingSpec {
    pub(crate) key: String,
    #[serde(rename = "type")]
    pub(crate) kind: SettingKind,
    pub(crate) tab: String,
    #[serde(default)]
    pub(crate) default: Option<toml::Value>,
    #[serde(default)]
    pub(crate) values: Vec<String>,
    #[serde(default)]
    pub(crate) env: Option<String>,
    #[serde(default)]
    pub(crate) secret: bool,
    #[serde(default)]
    pub(crate) merge: MergeRule,
    #[serde(default)]
    pub(crate) path_scoped: bool,
    #[serde(default)]
    pub(crate) operator_no_loosen: bool,
    pub(crate) description: String,
}

#[derive(Debug, Deserialize)]
struct SchemaFile {
    settings: Vec<SettingSpec>,
}

/// Loaded schema index.
pub(crate) struct SettingsSchema {
    entries: Vec<SettingSpec>,
}

static SCHEMA: OnceLock<SettingsSchema> = OnceLock::new();

/// Access the embedded settings schema (parsed once).
pub(crate) fn schema() -> &'static SettingsSchema {
    SCHEMA.get_or_init(|| {
        let parsed: SchemaFile = toml::from_str(SCHEMA_TOML).unwrap_or(SchemaFile {
            settings: Vec::new(),
        });
        SettingsSchema {
            entries: parsed.settings,
        }
    })
}

impl SettingsSchema {
    /// Every setting row in declaration order.
    pub(crate) fn all(&self) -> &[SettingSpec] {
        &self.entries
    }

    /// Exact schema lookup by dotted key.
    pub(crate) fn get(&self, key: &str) -> Option<&SettingSpec> {
        self.entries.iter().find(|s| s.key == key)
    }

    /// Every schema entry that declares an `HRN_*` environment override.
    pub(crate) fn env_bindings(&self) -> impl Iterator<Item = (&str, &SettingSpec)> {
        self.entries
            .iter()
            .filter_map(|s| s.env.as_deref().map(|env| (env, s)))
    }

    /// Up to 5 similar setting keys for an unknown key error message.
    pub(crate) fn similar_keys(&self, needle: &str) -> Vec<String> {
        let low = needle.to_ascii_lowercase();
        let mut scored: Vec<(usize, &str)> = self
            .entries
            .iter()
            .filter_map(|s| {
                let cand = s.key.to_ascii_lowercase();
                let score = if cand.contains(&low) || low.contains(&cand) {
                    0
                } else {
                    let dist = edit_distance(&low, &cand);
                    (dist <= 5).then_some(dist)?
                };
                Some((score, s.key.as_str()))
            })
            .collect();
        scored.sort_by_key(|(score, key)| (*score, *key));
        scored
            .into_iter()
            .take(5)
            .map(|(_, k)| k.to_owned())
            .collect()
    }
}

/// omp's `parseFlag` boolean environment parser:
/// empty/whitespace -> `None` (unset), `1/y/true/yes/on` -> `Some(true)`,
/// `0/n/false/no/off` -> `Some(false)`, anything else -> `None` (unparseable = unset).
pub(crate) fn parse_flag(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "y" | "true" | "yes" | "on" => Some(true),
        "0" | "n" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Parse an environment variable according to `spec.kind`; returns `None` when unset or invalid.
pub(crate) fn parse_env_value(spec: &SettingSpec, raw: &str) -> Option<toml::Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match spec.kind {
        SettingKind::Bool => parse_flag(trimmed).map(toml::Value::Boolean),
        SettingKind::Integer => trimmed.parse::<i64>().ok().map(toml::Value::Integer),
        SettingKind::Float => trimmed
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(toml::Value::Float),
        SettingKind::String => {
            if !spec.values.is_empty() && !spec.values.iter().any(|v| v == trimmed) {
                return None;
            }
            Some(toml::Value::String(trimmed.to_owned()))
        }
        SettingKind::Array | SettingKind::Table => {
            let json: serde_json::Value = serde_json::from_str(trimmed).ok()?;
            let val = json_to_toml(&json)?;
            match (spec.kind, &val) {
                (SettingKind::Array, toml::Value::Array(_))
                | (SettingKind::Table, toml::Value::Table(_)) => Some(val),
                _ => None,
            }
        }
    }
}

/// Parse a user-supplied CLI or `cfg://` value according to `spec`.
pub(crate) fn parse_cli_value(spec: &SettingSpec, raw: &str) -> Result<toml::Value> {
    let trimmed = raw.trim();
    match spec.kind {
        SettingKind::Bool => parse_flag(trimmed)
            .map(toml::Value::Boolean)
            .ok_or_else(|| {
                HrnError::Config(format!(
                    "`{}` expects a boolean (true/false/yes/no/1/0/on/off), got `{raw}`",
                    spec.key
                ))
            }),
        SettingKind::Integer => trimmed
            .parse::<i64>()
            .map(toml::Value::Integer)
            .map_err(|_| {
                HrnError::Config(format!("`{}` expects an integer, got `{raw}`", spec.key))
            }),
        SettingKind::Float => trimmed
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(toml::Value::Float)
            .ok_or_else(|| {
                HrnError::Config(format!(
                    "`{}` expects a finite number, got `{raw}`",
                    spec.key
                ))
            }),
        SettingKind::String => {
            let unquoted = unquote_string(trimmed);
            if !spec.values.is_empty() && !spec.values.iter().any(|v| v == unquoted) {
                return Err(HrnError::Config(format!(
                    "`{}` must be one of: {} (got `{unquoted}`)",
                    spec.key,
                    spec.values.join(", ")
                )));
            }
            Ok(toml::Value::String(unquoted.to_owned()))
        }
        SettingKind::Array => {
            let json: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
                HrnError::Config(format!("`{}` expects a JSON array: {e}", spec.key))
            })?;
            let Some(val) = json_to_toml(&json) else {
                return Err(HrnError::Config(format!(
                    "`{}` contains a value that cannot be stored in TOML",
                    spec.key
                )));
            };
            if !val.is_array() {
                return Err(HrnError::Config(format!(
                    "`{}` expects a JSON array, got `{raw}`",
                    spec.key
                )));
            }
            Ok(val)
        }
        SettingKind::Table => {
            let json: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
                HrnError::Config(format!("`{}` expects a JSON object: {e}", spec.key))
            })?;
            let Some(val) = json_to_toml(&json) else {
                return Err(HrnError::Config(format!(
                    "`{}` contains a value that cannot be stored in TOML",
                    spec.key
                )));
            };
            if !val.is_table() {
                return Err(HrnError::Config(format!(
                    "`{}` expects a JSON object, got `{raw}`",
                    spec.key
                )));
            }
            Ok(val)
        }
    }
}

/// Convert a [`serde_json::Value`] into a [`toml::Value`]; returns `None` for `Null` or non-finite floats.
pub(crate) fn json_to_toml(val: &serde_json::Value) -> Option<toml::Value> {
    match val {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(toml::Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(toml::Value::Integer(i))
            } else {
                n.as_f64().filter(|f| f.is_finite()).map(toml::Value::Float)
            }
        }
        serde_json::Value::String(s) => Some(toml::Value::String(s.clone())),
        serde_json::Value::Array(items) => items
            .iter()
            .map(json_to_toml)
            .collect::<Option<Vec<_>>>()
            .map(toml::Value::Array),
        serde_json::Value::Object(map) => {
            let mut tbl = toml::Table::new();
            for (k, v) in map {
                tbl.insert(k.clone(), json_to_toml(v)?);
            }
            Some(toml::Value::Table(tbl))
        }
    }
}

/// Convert a [`toml::Value`] into a [`serde_json::Value`].
pub(crate) fn toml_to_json(val: &toml::Value) -> serde_json::Value {
    serde_json::to_value(val).unwrap_or(serde_json::Value::Null)
}

/// Format a [`toml::Value`] concisely for display (masking secrets when requested).
pub(crate) fn format_value(
    spec: &SettingSpec,
    val: Option<&toml::Value>,
    mask_secret: bool,
    redacted_label: &str,
) -> String {
    let Some(v) = val else {
        return "(unset)".to_owned();
    };
    if mask_secret && spec.secret {
        return redacted_label.to_owned();
    }
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn unquote_string(raw: &str) -> &str {
    if raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')))
    {
        raw.get(1..raw.len() - 1).unwrap_or(raw)
    } else {
        raw
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        if let Some(first) = curr.first_mut() {
            *first = i + 1;
        }
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let del = prev
                .get(j + 1)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            let ins = curr.get(j).copied().unwrap_or(usize::MAX).saturating_add(1);
            let sub = prev
                .get(j)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(cost);
            if let Some(slot) = curr.get_mut(j + 1) {
                *slot = del.min(ins).min(sub);
            }
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev.last().copied().unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn defaults_round_trip() {
        let parsed: SchemaFile = toml::from_str(SCHEMA_TOML).expect("valid schema");
        assert!(!parsed.settings.is_empty());

        let defaults_table: toml::Table = toml::from_str(DEFAULTS_TOML).expect("valid defaults");
        // Verify embedded defaults deserialize cleanly through `Config` when paired with a minimal model.
        let minimal = format!(
            "{DEFAULTS_TOML}\n\
             [[providers]]\n\
             name = \"local\"\n\
             kind = \"openai_chat\"\n\
             base_url = \"http://localhost:11434/v1\"\n\
             [models.m]\n\
             provider = \"local\"\n\
             id = \"qwen\"\n\
             context_window = 128000\n\
             [roles]\n\
             default = \"m\"\n"
        );
        let cfg = Config::parse(&minimal).expect("defaults round-trip through ConfigFile");
        assert_eq!(cfg.thinking, hrn_kernel::ThinkingLevel::Medium);
        assert_eq!(cfg.mcp_tool_budget, 4_000);
        assert!(cfg.premium_defer);
        assert!(cfg.notify.enabled);
        assert_eq!(cfg.notify.after.as_secs(), 30);
        assert_eq!(cfg.notify.title, "hrn");
        assert_eq!(cfg.appearance.theme.dark, "titanium");
        assert_eq!(cfg.appearance.theme.light, "titanium-light");
        assert_eq!(cfg.appearance.theme.current, hrn_tui::ThemeMode::Auto);
        assert_eq!(cfg.appearance.symbol_preset, hrn_tui::SymbolPreset::Unicode);
        assert!(!cfg.appearance.color_blind_mode);
        assert_eq!(
            cfg.appearance.statusline.preset,
            hrn_tui::StatuslinePreset::Default
        );
        assert_eq!(
            cfg.appearance.statusline.separator,
            hrn_tui::SeparatorStyle::Powerline
        );
        assert!(cfg.appearance.statusline.session_accent);
        assert!(!cfg.appearance.statusline.transparent);
        assert!(!cfg.appearance.statusline.show_hook_status);
        assert!(!cfg.appearance.vim_mode);
        assert_eq!(
            cfg.appearance.vim_mode_display,
            hrn_tui::VimModeDisplay::Text
        );
        assert!(cfg.appearance.recall_cleared_drafts);

        // Every leaf in `config.defaults.toml` must exist in `settings.schema.toml` with the same default.
        let mut flat = Vec::new();
        flatten_table("", &defaults_table, &mut flat);
        for (key, val) in flat {
            let spec = schema()
                .get(&key)
                .unwrap_or_else(|| panic!("default key `{key}` missing from settings.schema.toml"));
            assert_eq!(
                spec.default.as_ref(),
                Some(&val),
                "schema default mismatch for `{key}`"
            );
        }
    }

    fn flatten_table(prefix: &str, tbl: &toml::Table, out: &mut Vec<(String, toml::Value)>) {
        for (k, v) in tbl {
            let key = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}.{k}")
            };
            if let toml::Value::Table(sub) = v {
                flatten_table(&key, sub, out);
            } else {
                out.push((key, v.clone()));
            }
        }
    }
}
