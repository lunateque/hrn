//! The theme: semantic colour constants and the styles built from them. The constants are slot
//! markers (see `palette`); the installed theme file decides the real colours when a frame is
//! written out. Colours are truecolor; on terminals without 24-bit color the written cells are
//! mapped to the nearest xterm-256 entries ([`paint`]). Body text keeps the terminal's default
//! foreground.

mod detect;
mod discovery;
mod palette;

pub use discovery::{
    ThemeEntry, ThemeWatcher, available_theme_names, available_theme_names_in, available_themes,
    discover_in, is_known_theme, set_themes_dir, themes_dir,
};
pub use palette::{
    active_theme_name, active_theme_path, color_blind_mode, reload_active_theme,
    set_color_blind_mode,
};

pub(crate) use detect::probe_terminal_tone;
pub(crate) use palette::{install_or_default, pixel, resolve};

use crate::port_theme::ThemeConfig;
use palette::Slot;
use ratatui::style::{Color, Modifier, Style};

/// Headings, bullets, links, the model name and the borders of live cards.
pub(crate) const ACCENT: Color = Slot::Accent.color();
/// The status band's highlights, key hints and the active selection's text.
pub(crate) const FOCUS: Color = Slot::Focus.color();
/// Card frames, code fences, rules, quote bars and the unused part of the context gauge.
pub(crate) const BORDER: Color = Slot::Border.color();
/// The frame of a finished edit card.
pub(crate) const BORDER_MUTED: Color = Slot::BorderMuted.color();
pub(crate) const SUCCESS: Color = Slot::Success.color();
pub(crate) const ERROR: Color = Slot::Error.color();
pub(crate) const WARNING: Color = Slot::Warning.color();
/// Secondary text: thinking, tool output, quotes, diff context.
pub(crate) const MUTED: Color = Slot::Muted.color();
/// Tertiary text: hints, meta, line numbers.
pub(crate) const DIM: Color = Slot::Dim.color();
/// Bright body text, used where the default foreground would not be explicit enough.
pub(crate) const TEXT: Color = Slot::Text.color();
/// The active selection's row background.
pub(crate) const SELECTED_BG: Color = Slot::SelectedBg.color();
/// The ` (url)` after a markdown link label, and bare URLs.
pub(crate) const LINK_URL: Color = Slot::LinkUrl.color();
/// The user message, tool card and status band background.
pub(crate) const SURFACE: Color = Slot::Surface.color();
/// Labels of custom notices and tips, the cost.
pub(crate) const GOLD: Color = Slot::Gold.color();
/// The dimmed accent of the context window label in the gauge.
pub(crate) const ACCENT_DIM: Color = Slot::AccentDim.color();
/// Table headers.
pub(crate) const YELLOW: Color = Slot::Yellow.color();
pub(crate) const HEADING: Color = Slot::Heading.color();
pub(crate) const INLINE_CODE: Color = Slot::InlineCode.color();
pub(crate) const LINK: Color = Slot::Link.color();
pub(crate) const SYN_COMMENT: Color = Slot::Comment.color();
pub(crate) const SYN_KEYWORD: Color = Slot::Keyword.color();
pub(crate) const SYN_FUNCTION: Color = Slot::Function.color();
pub(crate) const SYN_VARIABLE: Color = Slot::Variable.color();
pub(crate) const SYN_STRING: Color = Slot::String.color();
pub(crate) const SYN_NUMBER: Color = Slot::Number.color();
pub(crate) const SYN_TYPE: Color = Slot::Type.color();
pub(crate) const SYN_OPERATOR: Color = Slot::Operator.color();
pub(crate) const SYN_PUNCTUATION: Color = Slot::Punctuation.color();
/// Diff-added colour marker (tracks `SUCCESS`, or shifts to blue/cyan in color-blind mode).
pub(crate) const DIFF_ADDED: Color = palette::DIFF_ADDED_COLOR;
/// Install the theme selected by `config` (probing terminal tone when `current == Auto`).
pub(crate) fn install_config(config: &ThemeConfig) -> Option<String> {
    let tone = probe_terminal_tone();
    let spec = config.resolve_name(tone);
    install_or_default(Some(spec))
}

pub(crate) fn plain() -> Style {
    Style::new()
}

pub(crate) fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

/// The band / bubble / card background (kept for callers; paints no background so the
/// terminal theme shows through).
pub(crate) fn surface() -> Style {
    Style::new()
}

pub(crate) fn muted() -> Style {
    fg(MUTED)
}

pub(crate) fn dim() -> Style {
    fg(DIM)
}

pub(crate) fn border() -> Style {
    fg(BORDER)
}

pub(crate) fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

pub(crate) fn accent() -> Style {
    fg(ACCENT)
}

/// The status band's highlight.
pub(crate) fn focus() -> Style {
    fg(FOCUS)
}

/// The label half of a key hint (`esc` is accent, `to interrupt` is this).
pub(crate) fn hint() -> Style {
    dim().add_modifier(Modifier::ITALIC)
}

pub(crate) fn inline_code() -> Style {
    fg(INLINE_CODE)
}

pub(crate) fn code_block() -> Style {
    fg(MUTED)
}

pub(crate) fn link() -> Style {
    fg(LINK).add_modifier(Modifier::UNDERLINED)
}

pub(crate) fn link_url() -> Style {
    fg(LINK_URL)
}

pub(crate) fn reasoning() -> Style {
    muted().add_modifier(Modifier::ITALIC)
}

pub(crate) fn heading(level: u8) -> Style {
    let style = fg(HEADING).add_modifier(Modifier::BOLD);
    if level == 1 {
        style.add_modifier(Modifier::UNDERLINED)
    } else {
        style
    }
}

pub(crate) fn diff_added() -> Style {
    fg(DIFF_ADDED)
}

pub(crate) fn diff_removed() -> Style {
    fg(ERROR)
}

pub(crate) fn diff_context() -> Style {
    fg(MUTED)
}

/// Whether the terminal renders 24-bit color: `COLORTERM` says so, or it is Windows Terminal,
/// which always does but does not advertise it.
pub(crate) fn truecolor() -> bool {
    truecolor_from(
        std::env::var("COLORTERM").ok().as_deref(),
        std::env::var_os("WT_SESSION").is_some(),
    )
}

fn truecolor_from(colorterm: Option<&str>, windows_terminal: bool) -> bool {
    windows_terminal
        || colorterm.is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("truecolor") || value.contains("24bit")
        })
}

/// The colour a cell is written with: the slot resolved through the theme, mapped to the nearest
/// xterm-256 entry when the terminal lacks 24-bit colour.
pub(crate) fn paint(color: Color, truecolor: bool) -> Color {
    let color = resolve(color);
    match color {
        Color::Rgb(r, g, b) if !truecolor => Color::Indexed(nearest_256(r, g, b)),
        other => other,
    }
}

/// The xterm-256 index closest to an RGB value: the nearer of the 6×6×6 cube entry and the gray
/// ramp entry (squared Euclidean distance).
fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let step = |v: u8| -> u8 {
        match v {
            0..=47 => 0,
            48..=114 => 1,
            _ => (v - 35) / 40,
        }
    };
    let level = |i: u8| LEVELS.get(usize::from(i)).copied().unwrap_or(255);
    let (ri, gi, bi) = (step(r), step(g), step(b));
    let cube = (level(ri), level(gi), level(bi));
    let average = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let gray_step = u8::try_from(average.saturating_sub(3) / 10)
        .unwrap_or(23)
        .min(23);
    let gray = 8 + gray_step * 10;
    let distance = |(cr, cg, cb): (u8, u8, u8)| -> u32 {
        let d = |a: u8, b: u8| u32::from(a.abs_diff(b)).pow(2);
        d(r, cr) + d(g, cg) + d(b, cb)
    };
    if distance((gray, gray, gray)) < distance(cube) {
        232 + gray_step
    } else {
        16 + 36 * ri + 6 * gi + bi
    }
}
