//! The colour slots a theme file fills in. Widgets paint with a slot's marker colour (an `Rgb`
//! value no theme uses); the real colour is looked up in the installed palette when a cell is
//! written to the terminal ([`resolve`]). A theme is a TOML file of `[colors]` entries; the
//! built-in ones are embedded and a user theme lists only what it changes on top of a `base`.

use super::discovery::{ThemeOrigin, resolve_spec, themes_dir};
use ratatui::style::Color;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

macro_rules! slots {
    ($($variant:ident $name:literal),+ $(,)?) => {
        /// A themable colour.
        #[derive(Clone, Copy)]
        pub(crate) enum Slot { $($variant),+ }

        /// The token each slot has in theme files, in slot order.
        const NAMES: &[&str] = &[$($name),+];
    };
}

slots! {
    Accent "accent", Focus "focus", SelectedBg "selected_bg", Border "border",
    BorderMuted "border_muted", Success "success", Error "error", Warning "warning",
    Muted "muted", Dim "dim", Text "text", Surface "surface", Gold "gold",
    AccentDim "accent_dim", Yellow "yellow", LinkUrl "link_url",
    Heading "heading", InlineCode "inline_code", Link "link",
    Comment "comment", Keyword "keyword", Function "function", Variable "variable",
    String "string", Number "number", Type "type", Operator "operator",
    Punctuation "punctuation",
}

const COUNT: usize = NAMES.len();
/// Virtual marker index for diff-added colour (follows `Success`, shifted in color-blind mode).
#[expect(
    clippy::cast_possible_truncation,
    reason = "COUNT is 32, well within u8"
)]
const DIFF_ADDED_SLOT: u8 = COUNT as u8;
/// The marker's first two channels; the third is the slot index.
const MARK: (u8, u8) = (0xFE, 0xED);

/// Virtual colour marker used by [`super::diff_added`].
pub(crate) const DIFF_ADDED_COLOR: Color = Color::Rgb(MARK.0, MARK.1, DIFF_ADDED_SLOT);

/// The theme used when the config names none.
pub(crate) const DEFAULT: &str = "titanium";
/// Maximum inheritance depth for user themes referencing another `base`.
const MAX_BASE_DEPTH: usize = 4;

/// The themes shipped inside the binary.
const BUILT_IN: [(&str, &str); 3] = [
    ("titanium", include_str!("../../themes/titanium.toml")),
    (
        "titanium-vivid",
        include_str!("../../themes/titanium-vivid.toml"),
    ),
    (
        "titanium-light",
        include_str!("../../themes/titanium-light.toml"),
    ),
];

impl Slot {
    /// The colour widgets paint with; [`resolve`] turns it into the theme's colour.
    pub(crate) const fn color(self) -> Color {
        Color::Rgb(MARK.0, MARK.1, self as u8)
    }
}

/// Every slot's colour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Palette([Color; COUNT]);

impl Palette {
    /// Read a slot's resolved colour directly from this palette.
    pub(crate) fn get(&self, slot: Slot) -> Color {
        self.0.get(slot as usize).copied().unwrap_or(Color::Reset)
    }

    /// Resolved diff-added colour: `Success` normally, or shifted to a deuteranopia-safe
    /// blue/cyan hue when `color_blind` is enabled.
    pub(crate) fn diff_added_color(&self, color_blind: bool) -> Color {
        let base = self.get(Slot::Success);
        if color_blind {
            shift_color_blind(base)
        } else {
            base
        }
    }
}

/// Shift a green diff-added colour into a deuteranopia-safe blue/cyan hue while preserving
/// light-vs-dark luminance balance.
pub(crate) fn shift_color_blind(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => {
            let nr = r.min(g) / 2;
            let ng = u8::try_from(u16::from(g) * 3 / 4).unwrap_or(g);
            let nb = g.max(b).saturating_add(30);
            Color::Rgb(nr, ng, nb)
        }
        Color::Green | Color::LightGreen => Color::Cyan,
        other => other,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    /// The built-in or user theme a user theme starts from (default `titanium`).
    base: Option<String>,
    colors: BTreeMap<String, String>,
}

/// Installed palette together with its display name and optional on-disk source path.
pub(crate) struct LoadedTheme {
    pub(crate) palette: Palette,
    pub(crate) name: String,
    pub(crate) source_path: Option<PathBuf>,
}

static ACTIVE: RwLock<Option<LoadedTheme>> = RwLock::new(None);
static COLOR_BLIND: AtomicBool = AtomicBool::new(false);

/// Enable or disable color-blind diff-added hue shifting (`[tui] color_blind_mode`).
pub fn set_color_blind_mode(enabled: bool) {
    COLOR_BLIND.store(enabled, Ordering::Relaxed);
}

/// Whether color-blind diff-added hue shifting is currently enabled.
#[must_use]
pub fn color_blind_mode() -> bool {
    COLOR_BLIND.load(Ordering::Relaxed)
}

/// Install a loaded theme as the active colour palette (supports live switching).
pub(crate) fn install_loaded(loaded: LoadedTheme) {
    *ACTIVE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(loaded);
}

/// Built-in theme names shipped in the binary.
pub(crate) fn built_in_names() -> Vec<String> {
    BUILT_IN
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect()
}

/// Active theme's name (`"titanium"` before anything is installed).
#[must_use]
pub fn active_theme_name() -> String {
    ACTIVE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map_or_else(|| DEFAULT.to_owned(), |t| t.name.clone())
}

/// On-disk `.toml` path of the active custom theme (`None` for built-in themes).
#[must_use]
pub fn active_theme_path() -> Option<PathBuf> {
    ACTIVE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|t| t.source_path.clone())
}

/// A slot marker becomes the installed theme's colour; every other colour passes through.
pub(crate) fn resolve(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, slot) if (r, g) == MARK && slot == DIFF_ADDED_SLOT => ACTIVE
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map_or(color, |active| {
                active.palette.diff_added_color(color_blind_mode())
            }),
        Color::Rgb(r, g, slot) if (r, g) == MARK => ACTIVE
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|active| active.palette.0.get(usize::from(slot)))
            .copied()
            .unwrap_or(color),
        other => other,
    }
}

/// An image pixel's colour; a pixel that happens to equal a slot marker is nudged
/// so the theme never recolours it.
pub(crate) fn pixel(r: u8, g: u8, b: u8) -> Color {
    let b = if (r, g) == MARK && usize::from(b) <= COUNT {
        b + 0x40
    } else {
        b
    };
    Color::Rgb(r, g, b)
}

/// Load `spec` using an explicit themes directory; `None` selects the default theme.
pub(crate) fn load_in(spec: Option<&str>, dir: Option<&Path>) -> Result<LoadedTheme, String> {
    load_in_depth(spec, dir, 0)
}

fn load_in_depth(
    spec: Option<&str>,
    dir: Option<&Path>,
    depth: usize,
) -> Result<LoadedTheme, String> {
    if depth > MAX_BASE_DEPTH {
        return Err("theme base inheritance depth exceeded (possible cycle)".to_owned());
    }
    let trimmed = spec
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT);
    match resolve_spec(trimmed, dir)? {
        ThemeOrigin::BuiltIn { name, source } => Ok(LoadedTheme {
            palette: parse_complete(&name, source)?,
            name,
            source_path: None,
        }),
        ThemeOrigin::UserNamed { name, path } => {
            let source = std::fs::read_to_string(&path)
                .map_err(|err| format!("theme `{name}` ({}): {err}", path.display()))?;
            let palette = parse_overlay(&name, &source, dir, depth)?;
            Ok(LoadedTheme {
                palette,
                name,
                source_path: Some(path),
            })
        }
        ThemeOrigin::FilePath(path) => {
            let display = path.display().to_string();
            let source = std::fs::read_to_string(&path)
                .map_err(|err| format!("theme `{display}`: {err}"))?;
            let palette = parse_overlay(&display, &source, dir, depth)?;
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .map_or(display, str::to_owned);
            Ok(LoadedTheme {
                palette,
                name,
                source_path: Some(path),
            })
        }
    }
}

/// Install `spec` (using `dir`), or fall back to the default theme when it fails;
/// the failure reason is returned for display.
pub(crate) fn install_or_default_in(spec: Option<&str>, dir: Option<&Path>) -> Option<String> {
    match load_in(spec, dir) {
        Ok(loaded) => {
            install_loaded(loaded);
            None
        }
        Err(err) => match load_in(None, dir) {
            Ok(default_theme) => {
                install_loaded(default_theme);
                Some(err)
            }
            Err(default_err) => Some(format!(
                "{err}; the default theme failed too: {default_err}"
            )),
        },
    }
}

/// Install `spec`, or the default theme when it fails; the failure is returned for display.
pub(crate) fn install_or_default(spec: Option<&str>) -> Option<String> {
    let dir = themes_dir();
    install_or_default_in(spec, dir.as_deref())
}

/// Reload a custom theme file from `path`, replacing the installed palette only when valid.
/// On read or parse error the previously installed (last-good) palette stays untouched.
pub(crate) fn reload_from_path(path: &Path, dir: Option<&Path>) -> Result<(), String> {
    let display = path.display().to_string();
    let source =
        std::fs::read_to_string(path).map_err(|err| format!("reload theme `{display}`: {err}"))?;
    let palette = parse_overlay(&display, &source, dir, 0)?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map_or(display, str::to_owned);
    install_loaded(LoadedTheme {
        palette,
        name,
        source_path: Some(path.to_path_buf()),
    });
    Ok(())
}

/// Re-read the currently active custom theme file from disk (no-op `Ok(false)` for built-ins).
/// Keeps the last-good palette intact on any error.
///
/// # Errors
/// Returns an error string when the active custom theme file cannot be read or parsed.
pub fn reload_active_theme() -> Result<bool, String> {
    let Some(path) = active_theme_path() else {
        return Ok(false);
    };
    let dir = themes_dir();
    reload_from_path(&path, dir.as_deref()).map(|()| true)
}

pub(super) fn built_in(name: &str) -> Option<&'static str> {
    BUILT_IN
        .iter()
        .find_map(|(known, source)| (*known == name).then_some(*source))
}

fn parse_file(what: &str, source: &str) -> Result<ThemeFile, String> {
    toml::from_str(source).map_err(|err| format!("theme `{what}`: {err}"))
}

fn hex(what: &str, token: &str, value: &str) -> Result<Color, String> {
    let bad = || format!("theme `{what}`: `{token}` must be a `#rrggbb` colour, got `{value}`");
    let digits = value
        .strip_prefix('#')
        .filter(|d| d.len() == 6)
        .ok_or_else(bad)?;
    let channel = |at: usize| {
        digits
            .get(at..at + 2)
            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            .ok_or_else(bad)
    };
    Ok(Color::Rgb(channel(0)?, channel(2)?, channel(4)?))
}

/// Write the file's colours into `slots`; an unknown token is an error.
fn apply(
    what: &str,
    colors: &BTreeMap<String, String>,
    slots: &mut [Option<Color>; COUNT],
) -> Result<(), String> {
    for (token, value) in colors {
        let index = NAMES
            .iter()
            .position(|name| name == token)
            .ok_or_else(|| format!("theme `{what}`: unknown token `{token}`"))?;
        if let Some(slot) = slots.get_mut(index) {
            *slot = Some(hex(what, token, value)?);
        }
    }
    Ok(())
}

/// A built-in theme: every token must be present.
fn parse_complete(what: &str, source: &str) -> Result<Palette, String> {
    let file = parse_file(what, source)?;
    let mut slots = [None; COUNT];
    apply(what, &file.colors, &mut slots)?;
    finish(what, slots)
}

/// A user theme: the base's colours with the file's on top.
fn parse_overlay(
    what: &str,
    source: &str,
    dir: Option<&Path>,
    depth: usize,
) -> Result<Palette, String> {
    let file = parse_file(what, source)?;
    let base = file.base.as_deref().unwrap_or(DEFAULT);
    let mut slots = load_in_depth(Some(base), dir, depth + 1)
        .map_err(|err| format!("theme `{what}`: base {err}"))?
        .palette
        .0
        .map(Some);
    apply(what, &file.colors, &mut slots)?;
    finish(what, slots)
}

fn finish(what: &str, slots: [Option<Color>; COUNT]) -> Result<Palette, String> {
    let mut colors = [Color::Reset; COUNT];
    for ((color, slot), name) in colors.iter_mut().zip(slots).zip(NAMES) {
        *color = slot.ok_or_else(|| format!("theme `{what}`: missing token `{name}`"))?;
    }
    Ok(Palette(colors))
}
