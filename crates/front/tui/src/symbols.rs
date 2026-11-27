//! Embedded glyph tables (`data/symbols.toml`) for `[tui] symbol_preset = "unicode"|"nerd"|"ascii"`.
//! Stored behind an `RwLock` active preset so live switches repaint immediately without restarts,
//! and `ascii` mode guarantees zero non-ASCII or powerline characters.

use crate::port_theme::SymbolPreset;
use hrn_kernel::ThinkingLevel;
use serde::Deserialize;
use std::sync::{OnceLock, RwLock};

const SYMBOLS_TOML: &str = include_str!("../data/symbols.toml");

/// One complete set of UI glyphs loaded from `data/symbols.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SymbolSet {
    pub(crate) brand: String,
    pub(crate) question: String,
    pub(crate) approval: String,
    pub(crate) thinking: [String; 8],
    pub(crate) spinner: Vec<String>,
    pub(crate) pulse: Vec<String>,
    pub(crate) dot: String,
    pub(crate) fast: String,
    pub(crate) goal: String,
    pub(crate) focused: String,
    pub(crate) gear: String,
    pub(crate) folder: String,
    pub(crate) branch: String,
    pub(crate) tokens_up: String,
    pub(crate) tokens_down: String,
    pub(crate) reasoning: String,
    pub(crate) gauge_fill: String,
    pub(crate) ellipsis: String,
    pub(crate) gutter: String,
    pub(crate) camera: String,
    pub(crate) todo_active: String,
    pub(crate) divider: String,
    pub(crate) notice_info: String,
    pub(crate) notice_warn: String,
    pub(crate) notice_error: String,
    pub(crate) hook: String,
    pub(crate) vim_icon: String,
}

impl SymbolSet {
    /// Thinking effort glyph matching `level`.
    pub(crate) fn thinking_glyph(&self, level: ThinkingLevel) -> &str {
        let idx = match level {
            ThinkingLevel::Auto => 0,
            ThinkingLevel::Off => 1,
            ThinkingLevel::Minimal => 2,
            ThinkingLevel::Low => 3,
            ThinkingLevel::Medium => 4,
            ThinkingLevel::High => 5,
            ThinkingLevel::Xhigh => 6,
            ThinkingLevel::Max => 7,
        };
        self.thinking.get(idx).map_or("-", String::as_str)
    }

    /// Spinner frame for animation tick `frame`.
    pub(crate) fn spinner_frame(&self, frame: usize) -> &str {
        if self.spinner.is_empty() {
            return "-";
        }
        self.spinner
            .get(frame % self.spinner.len())
            .map_or("-", String::as_str)
    }

    /// Thinking pulse frame for animation tick `frame`.
    pub(crate) fn pulse_frame(&self, frame: usize) -> &str {
        if self.pulse.is_empty() {
            return "*";
        }
        self.pulse
            .get(frame % self.pulse.len())
            .map_or("*", String::as_str)
    }

    /// Single fill character for the context gauge bar.
    pub(crate) fn gauge_char(&self) -> char {
        self.gauge_fill.chars().next().unwrap_or('-')
    }

    fn fallback_ascii() -> Self {
        Self {
            brand: "hrn".to_owned(),
            question: "?".to_owned(),
            approval: "?".to_owned(),
            thinking: ["?", "-", ".", "1", "2", "3", "4", "*"].map(str::to_owned),
            spinner: ["|", "/", "-", "\\"].map(str::to_owned).to_vec(),
            pulse: ["*", "+", "o", "."].map(str::to_owned).to_vec(),
            dot: "|".to_owned(),
            fast: "*".to_owned(),
            goal: "@".to_owned(),
            focused: "->".to_owned(),
            gear: "#".to_owned(),
            folder: String::new(),
            branch: "git".to_owned(),
            tokens_up: "^".to_owned(),
            tokens_down: "v".to_owned(),
            reasoning: "~".to_owned(),
            gauge_fill: "-".to_owned(),
            ellipsis: "...".to_owned(),
            gutter: "> ".to_owned(),
            camera: "[img]".to_owned(),
            todo_active: "*".to_owned(),
            divider: "--".to_owned(),
            notice_info: "* ".to_owned(),
            notice_warn: "! ".to_owned(),
            notice_error: "x ".to_owned(),
            hook: "hk".to_owned(),
            vim_icon: "V".to_owned(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolCatalog {
    unicode: SymbolSet,
    nerd: SymbolSet,
    ascii: SymbolSet,
}

static CATALOG: OnceLock<SymbolCatalog> = OnceLock::new();
static ACTIVE: RwLock<SymbolPreset> = RwLock::new(SymbolPreset::Unicode);

fn catalog() -> &'static SymbolCatalog {
    CATALOG.get_or_init(|| {
        toml::from_str(SYMBOLS_TOML).unwrap_or_else(|_| SymbolCatalog {
            unicode: SymbolSet::fallback_ascii(),
            nerd: SymbolSet::fallback_ascii(),
            ascii: SymbolSet::fallback_ascii(),
        })
    })
}

/// Set the process-wide active symbol preset (supports live reload and switching).
pub fn set_preset(preset: SymbolPreset) {
    *ACTIVE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = preset;
}

/// Currently active symbol preset.
#[must_use]
pub fn preset() -> SymbolPreset {
    *ACTIVE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Look up the embedded symbol table for an explicit `preset`.
pub(crate) fn for_preset(preset: SymbolPreset) -> &'static SymbolSet {
    let cat = catalog();
    match preset {
        SymbolPreset::Unicode => &cat.unicode,
        SymbolPreset::Nerd => &cat.nerd,
        SymbolPreset::Ascii => &cat.ascii,
    }
}

/// Look up the currently active symbol table.
pub(crate) fn current() -> &'static SymbolSet {
    for_preset(preset())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_symbols_toml_parses_all_presets() {
        let parsed: SymbolCatalog = toml::from_str(SYMBOLS_TOML).expect("valid symbols.toml");
        assert_eq!(parsed.unicode.brand, "hrn");
        assert_eq!(parsed.unicode.thinking.len(), 8);
        assert!(!parsed.unicode.spinner.is_empty());
        assert!(!parsed.nerd.spinner.is_empty());
        assert!(!parsed.ascii.spinner.is_empty());
    }

    #[test]
    fn ascii_symbol_preset_contains_zero_non_ascii_glyphs() {
        let ascii = for_preset(SymbolPreset::Ascii);
        let singles = [
            &ascii.brand,
            &ascii.question,
            &ascii.approval,
            &ascii.dot,
            &ascii.fast,
            &ascii.goal,
            &ascii.focused,
            &ascii.gear,
            &ascii.folder,
            &ascii.branch,
            &ascii.tokens_up,
            &ascii.tokens_down,
            &ascii.reasoning,
            &ascii.gauge_fill,
            &ascii.ellipsis,
            &ascii.gutter,
            &ascii.camera,
            &ascii.todo_active,
            &ascii.divider,
            &ascii.notice_info,
            &ascii.notice_warn,
            &ascii.notice_error,
            &ascii.hook,
            &ascii.vim_icon,
        ];
        for s in singles {
            assert!(s.is_ascii(), "non-ASCII symbol in ascii preset: {s:?}");
        }
        for s in ascii
            .thinking
            .iter()
            .chain(&ascii.spinner)
            .chain(&ascii.pulse)
        {
            assert!(s.is_ascii(), "non-ASCII frame in ascii preset: {s:?}");
        }
    }
}
