//! Deserialization and validation of `[tui]`, `[tui.theme]`, and `[tui.statusline]`.
//! Supports both legacy `[tui] theme = "<name>"` (mapping to both dark and light slots) and
//! structured `[tui.theme] dark = "...", light = "...", current = "auto" | "dark" | "light"`.

use super::{Result, bad};
use hrn_tui::{
    AppearanceConfig, Notify, PinnedAgentsMode, SeparatorStyle, StatuslineConfig, StatuslinePreset,
    SymbolPreset, ThemeConfig, ThemeMode, VimModeDisplay,
};
use serde::Deserialize;
use std::time::Duration;

/// Structured `[tui.theme]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TuiThemeTable {
    dark: Option<String>,
    light: Option<String>,
    current: Option<String>,
}

/// `[tui] theme = "..."` (legacy shorthand) or `[tui.theme] dark/light/current` table.
#[derive(Debug)]
pub(super) enum TuiThemeField {
    Legacy(String),
    Structured(TuiThemeTable),
}

impl<'de> Deserialize<'de> for TuiThemeField {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct ThemeVisitor;
        impl<'de> serde::de::Visitor<'de> for ThemeVisitor {
            type Value = TuiThemeField;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a theme name string or `[tui.theme]` table with dark/light/current")
            }

            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(TuiThemeField::Legacy(v.to_owned()))
            }

            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(TuiThemeField::Legacy(v))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let de = serde::de::value::MapAccessDeserializer::new(map);
                TuiThemeTable::deserialize(de).map(TuiThemeField::Structured)
            }
        }
        deserializer.deserialize_any(ThemeVisitor)
    }
}

/// `[tui.statusline]` block.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TuiStatuslineFile {
    #[serde(default = "default_statusline_preset")]
    preset: String,
    #[serde(default = "default_statusline_separator")]
    separator: String,
    #[serde(default)]
    left_segments: Vec<String>,
    #[serde(default)]
    right_segments: Vec<String>,
    #[serde(default = "default_true")]
    session_accent: bool,
    #[serde(default)]
    transparent: bool,
    #[serde(default)]
    show_hook_status: bool,
}

impl Default for TuiStatuslineFile {
    fn default() -> Self {
        Self {
            preset: default_statusline_preset(),
            separator: default_statusline_separator(),
            left_segments: Vec::new(),
            right_segments: Vec::new(),
            session_accent: true,
            transparent: false,
            show_hook_status: false,
        }
    }
}

/// `[tui]` block: appearance, symbols, statusline, notifications, and pinned subagent options.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent boolean TUI flags from [tui]"
)]
pub(super) struct TuiFile {
    pub(super) theme: Option<TuiThemeField>,
    #[serde(default)]
    pub(super) notify: bool,
    #[serde(default = "default_notify_after")]
    pub(super) notify_after_secs: u64,
    #[serde(default)]
    pub(super) notify_title: Option<String>,
    #[serde(default = "default_pinned_agents")]
    pub(super) pinned_agents: String,
    #[serde(default = "default_true")]
    pub(super) subagent_live_preview: bool,
    #[serde(default)]
    pub(super) mouse: bool,
    #[serde(default = "default_symbol_preset")]
    pub(super) symbol_preset: String,
    #[serde(default)]
    pub(super) color_blind_mode: bool,
    #[serde(default)]
    pub(super) vim_mode: bool,
    #[serde(default = "default_vim_mode_display")]
    pub(super) vim_mode_display: String,
    #[serde(default = "default_true")]
    pub(super) recall_cleared_drafts: bool,
    #[serde(default)]
    pub(super) hide_thinking: bool,
    #[serde(default)]
    pub(super) statusline: TuiStatuslineFile,
}

impl Default for TuiFile {
    fn default() -> Self {
        Self {
            theme: None,
            notify: true,
            notify_after_secs: default_notify_after(),
            notify_title: None,
            pinned_agents: default_pinned_agents(),
            subagent_live_preview: true,
            mouse: false,
            symbol_preset: default_symbol_preset(),
            color_blind_mode: false,
            vim_mode: false,
            vim_mode_display: default_vim_mode_display(),
            recall_cleared_drafts: true,
            hide_thinking: false,
            statusline: TuiStatuslineFile::default(),
        }
    }
}

fn default_vim_mode_display() -> String {
    "text".to_owned()
}

fn default_notify_after() -> u64 {
    30
}

fn default_pinned_agents() -> String {
    "collapsed".to_owned()
}

fn default_symbol_preset() -> String {
    "unicode".to_owned()
}

fn default_statusline_preset() -> String {
    "default".to_owned()
}

fn default_statusline_separator() -> String {
    "powerline".to_owned()
}

fn default_true() -> bool {
    true
}

/// Validated `[tui]` values ready to store on [`super::Config`].
pub(super) struct LoweredTui {
    pub(super) notify: Notify,
    pub(super) pinned_agents: PinnedAgentsMode,
    pub(super) subagent_live_preview: bool,
    pub(super) mouse: bool,
    pub(super) appearance: AppearanceConfig,
}

/// Validate and lower a raw `[tui]` section.
pub(super) fn lower_tui(raw: TuiFile) -> Result<LoweredTui> {
    let pinned_agents = PinnedAgentsMode::parse(&raw.pinned_agents).ok_or_else(|| {
        bad(format!(
            "tui.pinned_agents `{}` is not one of collapsed, full, off",
            raw.pinned_agents
        ))
    })?;
    let symbol_preset = SymbolPreset::parse(&raw.symbol_preset).ok_or_else(|| {
        bad(format!(
            "tui.symbol_preset `{}` is not one of unicode, nerd, ascii",
            raw.symbol_preset
        ))
    })?;
    let vim_mode_display = VimModeDisplay::parse(&raw.vim_mode_display).ok_or_else(|| {
        bad(format!(
            "tui.vim_mode_display `{}` is not one of text, icon, none",
            raw.vim_mode_display
        ))
    })?;
    let theme = match raw.theme {
        None => ThemeConfig::default(),
        Some(TuiThemeField::Legacy(name)) => ThemeConfig::legacy(name),
        Some(TuiThemeField::Structured(tbl)) => {
            let defaults = ThemeConfig::default();
            let current = match tbl.current.as_deref() {
                None => defaults.current,
                Some(raw_cur) => ThemeMode::parse(raw_cur).ok_or_else(|| {
                    bad(format!(
                        "tui.theme.current `{raw_cur}` is not one of auto, dark, light"
                    ))
                })?,
            };
            ThemeConfig {
                dark: tbl.dark.unwrap_or(defaults.dark),
                light: tbl.light.unwrap_or(defaults.light),
                current,
            }
        }
    };
    let sl_preset = StatuslinePreset::parse(&raw.statusline.preset).ok_or_else(|| {
        bad(format!(
            "tui.statusline.preset `{}` is not one of default, minimal, compact, full, nerd, ascii, custom",
            raw.statusline.preset
        ))
    })?;
    let sl_sep = SeparatorStyle::parse(&raw.statusline.separator).ok_or_else(|| {
        bad(format!(
            "tui.statusline.separator `{}` is not one of powerline, powerline-thin, slash, pipe, block, none, ascii",
            raw.statusline.separator
        ))
    })?;
    let statusline = StatuslineConfig {
        preset: sl_preset,
        separator: sl_sep,
        left_segments: raw.statusline.left_segments,
        right_segments: raw.statusline.right_segments,
        session_accent: raw.statusline.session_accent,
        transparent: raw.statusline.transparent,
        show_hook_status: raw.statusline.show_hook_status,
    };
    Ok(LoweredTui {
        notify: Notify {
            enabled: raw.notify,
            after: Duration::from_secs(raw.notify_after_secs),
            title: raw.notify_title.unwrap_or_else(|| "hrn".to_owned()),
        },
        pinned_agents,
        subagent_live_preview: raw.subagent_live_preview,
        mouse: raw.mouse,
        appearance: AppearanceConfig {
            theme,
            themes_dir: None,
            symbol_preset,
            color_blind_mode: raw.color_blind_mode,
            statusline,
            profile: None,
            vim_mode: raw.vim_mode,
            vim_mode_display,
            recall_cleared_drafts: raw.recall_cleared_drafts,
            hide_thinking: raw.hide_thinking,
            keymap_paths: Vec::new(),
        },
    })
}
