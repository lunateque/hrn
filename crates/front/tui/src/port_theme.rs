//! Appearance, theme, symbol preset, and statusline configuration types shared between the TUI
//! and `bins/harness`.

use std::path::PathBuf;

/// Detected terminal background tone for `[tui.theme] current = "auto"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalTone {
    /// Dark terminal background (default fallback).
    #[default]
    Dark,
    /// Light terminal background.
    Light,
}

/// How `[tui.theme] current` chooses between the `dark` and `light` theme slots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeMode {
    /// Probe terminal luminance (OSC-11, then `COLORFGBG`, fallback dark).
    #[default]
    Auto,
    /// Always select the `dark` theme slot.
    Dark,
    /// Always select the `light` theme slot.
    Light,
}

impl ThemeMode {
    /// Parse `"auto"`, `"dark"`, or `"light"` (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }

    /// Canonical lowercase config string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

/// Resolved `[tui.theme]` (or legacy `[tui] theme = "..."`) configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeConfig {
    /// Theme name or file path used for dark terminals.
    pub dark: String,
    /// Theme name or file path used for light terminals.
    pub light: String,
    /// Selection mode (`auto`, `dark`, or `light`).
    pub current: ThemeMode,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            dark: "titanium".to_owned(),
            light: "titanium-light".to_owned(),
            current: ThemeMode::Auto,
        }
    }
}

impl ThemeConfig {
    /// Map legacy `[tui] theme = "<name>"` onto both dark and light slots.
    #[must_use]
    pub fn legacy(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            dark: name.clone(),
            light: name,
            current: ThemeMode::Auto,
        }
    }

    /// Choose the effective theme spec for `detected` terminal background tone.
    #[must_use]
    pub fn resolve_name(&self, detected: TerminalTone) -> &str {
        match self.current {
            ThemeMode::Dark => &self.dark,
            ThemeMode::Light => &self.light,
            ThemeMode::Auto => match detected {
                TerminalTone::Dark => &self.dark,
                TerminalTone::Light => &self.light,
            },
        }
    }

    /// Concise summary label for `/settings` and onboarding UI.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.dark == self.light {
            self.dark.clone()
        } else {
            format!(
                "{} (dark: {}, light: {})",
                self.current.as_str(),
                self.dark,
                self.light
            )
        }
    }
}

/// Glyph set used across the status band, spinners, notices, and prompt (`[tui] symbol_preset`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SymbolPreset {
    /// Standard Unicode glyphs and emojis (default).
    #[default]
    Unicode,
    /// Nerd Font glyphs for patched terminal fonts.
    Nerd,
    /// Strict 7-bit ASCII glyphs with zero Unicode or powerline characters.
    Ascii,
}

impl SymbolPreset {
    /// Parse `"unicode"`, `"nerd"`, or `"ascii"` (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "unicode" => Some(Self::Unicode),
            "nerd" => Some(Self::Nerd),
            "ascii" => Some(Self::Ascii),
            _ => None,
        }
    }

    /// Canonical lowercase config string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unicode => "unicode",
            Self::Nerd => "nerd",
            Self::Ascii => "ascii",
        }
    }
}

/// Named statusline segment layout (`[tui.statusline] preset`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatuslinePreset {
    /// Full default band with model, path, branch, usage, gauge, and title.
    #[default]
    Default,
    /// Minimal band keeping only brand, model, and branch.
    Minimal,
    /// Compact band with model, mode, path, branch, tokens, and gauge.
    Compact,
    /// Every segment enabled, including fast tier, vim, goal, agents, hook, cost, reasoning.
    Full,
    /// Nerd-font segment layout.
    Nerd,
    /// Strict ASCII layout.
    Ascii,
    /// User-supplied `left_segments` and `right_segments`.
    Custom,
}

impl StatuslinePreset {
    /// Parse a preset name (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "minimal" => Some(Self::Minimal),
            "compact" => Some(Self::Compact),
            "full" => Some(Self::Full),
            "nerd" => Some(Self::Nerd),
            "ascii" => Some(Self::Ascii),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }

    /// Canonical lowercase config key matching `data/statusline.presets.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Minimal => "minimal",
            Self::Compact => "compact",
            Self::Full => "full",
            Self::Nerd => "nerd",
            Self::Ascii => "ascii",
            Self::Custom => "custom",
        }
    }
}

/// Separator style between and around statusline segments (`[tui.statusline] separator`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SeparatorStyle {
    /// Filled powerline caps (`▶` / `◀`) and `>` inner separators.
    #[default]
    Powerline,
    /// Thin powerline chevrons (`` / ``).
    PowerlineThin,
    /// Forward slashes (`/`).
    Slash,
    /// Vertical bars (`|`).
    Pipe,
    /// Half-block caps (`▌` / `▐`) and box vertical bars.
    Block,
    /// Whitespace only, no separator glyphs.
    None,
    /// Pure ASCII separators with no powerline caps.
    Ascii,
}

impl SeparatorStyle {
    /// Parse a separator style name (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "powerline" => Some(Self::Powerline),
            "powerline-thin" | "powerline_thin" => Some(Self::PowerlineThin),
            "slash" => Some(Self::Slash),
            "pipe" => Some(Self::Pipe),
            "block" => Some(Self::Block),
            "none" => Some(Self::None),
            "ascii" => Some(Self::Ascii),
            _ => None,
        }
    }

    /// Canonical lowercase config key matching `data/statusline.presets.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Powerline => "powerline",
            Self::PowerlineThin => "powerline-thin",
            Self::Slash => "slash",
            Self::Pipe => "pipe",
            Self::Block => "block",
            Self::None => "none",
            Self::Ascii => "ascii",
        }
    }
}

/// Statusline configuration (`[tui.statusline]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatuslineConfig {
    /// Named segment preset (`default`, `minimal`, `compact`, `full`, `nerd`, `ascii`, `custom`).
    pub preset: StatuslinePreset,
    /// Separator style (`powerline`, `powerline-thin`, `slash`, `pipe`, `block`, `none`, `ascii`).
    pub separator: SeparatorStyle,
    /// Explicit left segment list (used for `custom` or to override preset defaults).
    pub left_segments: Vec<String>,
    /// Explicit right segment list (used for `custom` or to override preset defaults).
    pub right_segments: Vec<String>,
    /// Highlight active session elements (model, gauge) with the theme accent/focus colour.
    pub session_accent: bool,
    /// Omit background surface fill on all statusline cells.
    pub transparent: bool,
    /// Render active hook status when available.
    pub show_hook_status: bool,
}

impl Default for StatuslineConfig {
    fn default() -> Self {
        Self {
            preset: StatuslinePreset::Default,
            separator: SeparatorStyle::Powerline,
            left_segments: Vec::new(),
            right_segments: Vec::new(),
            session_accent: true,
            transparent: false,
            show_hook_status: false,
        }
    }
}

/// How the Vim mode badge renders in the status band (`[tui] vim_mode_display`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VimModeDisplay {
    /// Full mode label (`NORMAL`, `INSERT`, `VISUAL`, `V-LINE`, `REPLACE`) with icon.
    #[default]
    Text,
    /// Compact icon-only indicator (pure ASCII in the ASCII preset).
    Icon,
    /// Hide the Vim mode badge in the status band.
    None,
}

impl VimModeDisplay {
    /// Parse `"text"`, `"icon"`, or `"none"` (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "icon" => Some(Self::Icon),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// Canonical lowercase config string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Icon => "icon",
            Self::None => "none",
        }
    }
}

/// Bundled TUI appearance configuration passed at startup and on live config reload.
#[expect(
    clippy::struct_excessive_bools,
    reason = "four orthogonal TUI appearance display and editing flags"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppearanceConfig {
    /// Dark/light theme slots and selection mode.
    pub theme: ThemeConfig,
    /// Profile-aware user themes directory (`<data>/themes`), when available.
    pub themes_dir: Option<PathBuf>,
    /// Active glyph preset (`unicode`, `nerd`, `ascii`).
    pub symbol_preset: SymbolPreset,
    /// Shift diff-added colour to a deuteranopia-safe blue/cyan hue.
    pub color_blind_mode: bool,
    /// Statusline preset, separator, and segment options.
    pub statusline: StatuslineConfig,
    /// Active config profile tag shown beside the brand mark (`None` for the default profile).
    pub profile: Option<String>,
    /// Enable modal Vim editing in the prompt composer (`[tui] vim_mode`).
    pub vim_mode: bool,
    /// How the Vim badge renders in the status band (`[tui] vim_mode_display`).
    pub vim_mode_display: VimModeDisplay,
    /// Recall `Ctrl+C`-cleared drafts before prompt history on `Up` (`[tui] recall_cleared_drafts`).
    pub recall_cleared_drafts: bool,
    /// Hide reasoning / thinking blocks by default (`[tui] hide_thinking`, `hideThinkingBlock`).
    pub hide_thinking: bool,
    /// Ordered `keybindings.toml` remap layers (`default_profile < active_profile`).
    pub keymap_paths: Vec<PathBuf>,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: ThemeConfig::default(),
            themes_dir: None,
            symbol_preset: SymbolPreset::default(),
            color_blind_mode: false,
            statusline: StatuslineConfig::default(),
            profile: None,
            vim_mode: false,
            vim_mode_display: VimModeDisplay::Text,
            recall_cleared_drafts: true,
            hide_thinking: false,
            keymap_paths: Vec::new(),
        }
    }
}
