//! The status band, driven by `[tui.statusline]` presets (`data/statusline.presets.toml`) and
//! `[tui] symbol_preset` (`data/symbols.toml`). When the row is too narrow it sheds in priority
//! order: title's tail, right group, path's head, then left segments by drop priority while
//! keeping `brand` and `model`. Everything is measured with `unicode-width`.

use crate::git;
use crate::port_theme::{StatuslineConfig, StatuslinePreset, SymbolPreset};
use crate::status_presets::{self, SeparatorGlyphs};
use crate::status_segments::{
    Prioritized, Segment, accent_style, build_segments, truncate_ellipsis,
};
use crate::symbols::{self, SymbolSet};
use crate::theme;
use crate::wrap;
use hrn_kernel::{PermissionMode, ThinkingLevel, Tier};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Duration;

/// Longest path shown, and the shortest one squeezing leaves.
const PATH_MAX: usize = 40;
const PATH_MIN: usize = 12;
/// The title keeps at least this many columns before the right group is dropped.
const TITLE_MIN: usize = 8;
/// The title never takes more than this, so the gauge keeps room.
const TITLE_MAX: usize = 40;
/// Columns the gauge needs to be worth drawing.
const GAUGE_MIN: usize = 4;
/// The band stops one column short of the edge: a row that fills the last column leaves the
/// console's deferred-wrap pending, and Windows resolves it on the next relative move, shifting
/// every live row below the band by one.
const BAND_GUARD: usize = 1;

/// What the agent is doing right now, as the bar should show it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Activity {
    Idle,
    Working,
    Compacting,
    Approval,
    Question,
}

/// Everything the bar renders, borrowed from the app.
pub(crate) struct Snapshot<'a> {
    pub(crate) activity: Activity,
    /// True while background speculative compaction is running (`CompactionSpeculating`).
    pub(crate) speculating_compact: bool,
    pub(crate) elapsed: Option<Duration>,
    pub(crate) frame: usize,
    pub(crate) model: &'a str,
    pub(crate) thinking: ThinkingLevel,
    /// Active session service tier (`/fast`, `/fast ultra`), shown beside the model or as `fast`.
    pub(crate) fast_tier: Tier,
    pub(crate) mode: PermissionMode,
    pub(crate) advisor: bool,
    /// Ids of the background agents, jobs and services still running.
    pub(crate) running: &'a [String],
    /// Lifecycle counts (`running`/`idle`/`parked`) across non-main agents in the session tree.
    pub(crate) agent_counts: crate::port::AgentCounts,
    /// Currently focused child agent id, when viewing/steering a subagent session.
    pub(crate) focused_agent: Option<&'a str>,
    /// Tokens in front of the model on its latest call: the live context size.
    pub(crate) prompt_tokens: u64,
    pub(crate) context_window: u64,
    /// Tokens sent and received over the session.
    pub(crate) tokens: (u64, u64),
    /// What the session has cost in millionths of a dollar; `None` when nothing was priced.
    pub(crate) cost: Option<u64>,
    /// Reasoning tokens over the session; zero when the provider reports none.
    pub(crate) reasoning_tokens: u64,
    pub(crate) cwd: &'a str,
    pub(crate) branch: Option<&'a str>,
    pub(crate) worktree: Option<git::Status>,
    pub(crate) title: Option<&'a str>,
    /// The live goal-mode objective (`None`: goal mode off), shown as `◉ objective (n/m)`.
    pub(crate) goal: Option<&'a hrn_kernel::Goal>,
    /// Transient notice (e.g. `"press ctrl+c again to quit"`) that replaces the title.
    pub(crate) flash: Option<&'a str>,
    /// Active profile tag shown beside the brand mark (`None` for the default profile).
    pub(crate) profile: Option<&'a str>,
    /// Active vim mode indicator (`None` when Vim mode is off or hidden).
    pub(crate) vim_mode: Option<&'a str>,
    /// How the Vim mode badge renders (`text` | `icon` | `none`).
    pub(crate) vim_display: crate::port_theme::VimModeDisplay,
    /// Optional hook status badge shown when `[tui.statusline] show_hook_status = true`.
    pub(crate) hook_status: Option<&'a str>,
}

fn width_of(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn paint_bg(span: Span<'static>, _transparent: bool) -> Span<'static> {
    span
}

fn window_label(window: u64) -> String {
    if window >= 1_000_000 {
        format!("{}M", (window + 500_000) / 1_000_000)
    } else if window >= 1000 {
        format!("{}k", (window + 500) / 1000)
    } else {
        window.to_string()
    }
}

fn join_segments(segs: &[Segment], sep: &SeparatorGlyphs, transparent: bool) -> Vec<Span<'static>> {
    if segs.is_empty() {
        return Vec::new();
    }
    let mut spans = vec![Span::raw(" ")];
    for (i, seg) in segs.iter().enumerate() {
        if i > 0 {
            if sep.inner.is_empty() {
                spans.push(Span::raw(" "));
            } else {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(sep.inner.clone(), theme::border()));
                spans.push(Span::raw(" "));
            }
        }
        spans.extend(seg.iter().cloned());
    }
    spans.push(Span::raw(" "));
    spans
        .into_iter()
        .map(|s| paint_bg(s, transparent))
        .collect()
}

fn render_right_group(
    text: Option<&(String, bool)>,
    extra: &[Segment],
    sep: &SeparatorGlyphs,
    cfg: &StatuslineConfig,
) -> Option<Vec<Span<'static>>> {
    let mut segs: Vec<Segment> = extra.to_vec();
    if let Some((content, flash)) = text {
        let style = if *flash {
            accent_style(cfg.session_accent)
        } else {
            theme::muted().add_modifier(Modifier::ITALIC)
        };
        segs.push(vec![Span::styled(content.clone(), style)]);
    }
    if segs.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    if !cfg.transparent && !sep.right_cap.is_empty() {
        out.push(Span::styled(
            sep.right_cap.clone(),
            theme::fg(theme::SURFACE),
        ));
    }
    out.extend(join_segments(&segs, sep, cfg.transparent));
    Some(out)
}

fn gauge(
    snap: &Snapshot<'_>,
    width: usize,
    syms: &SymbolSet,
    session_accent: bool,
) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let fill = syms.gauge_char();
    let active = accent_style(session_accent);
    let window = snap.context_window;
    let mut cells: Vec<(char, Style)> = vec![(fill, theme::border()); width];
    if window == 0 {
        for cell in &mut cells {
            cell.1 = active;
        }
    } else {
        let percent = snap.prompt_tokens.saturating_mul(100) / window.max(1);
        let percent = usize::try_from(percent.min(100)).unwrap_or(100);
        let used = (percent * width).div_ceil(100).min(width);
        cells.iter_mut().take(used).for_each(|c| c.1 = active);
        let size = format!(" {} ", window_label(window));
        let size_at = width.saturating_sub(size.chars().count());
        let label = format!(" {percent}% ");
        let label_at = used
            .max(1)
            .min(size_at.saturating_sub(label.chars().count()));
        if width >= label.chars().count() + size.chars().count() + 2 {
            for (at, text, style) in [
                (label_at, label, active),
                (size_at, size, theme::fg(theme::ACCENT_DIM)),
            ] {
                for (i, c) in text.chars().enumerate() {
                    if let Some(cell) = cells.get_mut(at + i) {
                        *cell = (c, if c == ' ' { Style::new() } else { style });
                    }
                }
            }
        }
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (c, style) in cells {
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push(c),
            _ => spans.push(Span::styled(c.to_string(), style)),
        }
    }
    spans
}

fn right_text(
    snap: &Snapshot<'_>,
    right_names: &[String],
    syms: &SymbolSet,
) -> Option<(String, bool)> {
    let wants_title = right_names
        .iter()
        .any(|s| s.eq_ignore_ascii_case("title") || s.eq_ignore_ascii_case("flash"));
    match (snap.flash, snap.title) {
        (Some(flash), _) => Some((wrap::sanitize(flash).into_owned(), true)),
        (None, Some(title)) if wants_title && !title.trim().is_empty() => Some((
            truncate_ellipsis(&wrap::sanitize(title), TITLE_MAX, &syms.ellipsis),
            false,
        )),
        _ => None,
    }
}

fn shed_lowest_priority(left: &mut Vec<Prioritized>) -> bool {
    let victim = left
        .iter()
        .enumerate()
        .filter(|(_, seg)| seg.drop_priority > 1)
        .max_by_key(|(idx, seg)| (seg.drop_priority, *idx))
        .map(|(idx, _)| idx);
    match victim {
        Some(idx) => {
            left.remove(idx);
            true
        }
        None => false,
    }
}

fn clip_spans(spans: Vec<Span<'static>>, max: usize, ellipsis: &str) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= max {
        return spans;
    }
    let ell_w = wrap::str_width(ellipsis);
    let budget = max.saturating_sub(ell_w);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let mut style = spans.first().map(|s| s.style).unwrap_or_default();
    'spans: for span in spans {
        style = span.style;
        let mut text = String::new();
        for c in span.content.chars() {
            let w = wrap::char_width(c);
            if used + w > budget {
                if !text.is_empty() {
                    out.push(Span::styled(text, style));
                }
                break 'spans;
            }
            used += w;
            text.push(c);
        }
        out.push(Span::styled(text, style));
    }
    if max >= ell_w && !ellipsis.is_empty() {
        out.push(Span::styled(ellipsis.to_owned(), style));
    }
    out
}

/// Build the status band for `snap` honoring `cfg` and `symbol_preset`, never wider than
/// `width - BAND_GUARD` columns.
pub(crate) fn bar_with(
    snap: &Snapshot<'_>,
    width: usize,
    cfg: &StatuslineConfig,
    symbol_preset: SymbolPreset,
) -> Line<'static> {
    let width = width.saturating_sub(BAND_GUARD);
    let ascii_only = symbol_preset == SymbolPreset::Ascii || cfg.preset == StatuslinePreset::Ascii;
    let syms = if ascii_only {
        symbols::for_preset(SymbolPreset::Ascii)
    } else {
        symbols::for_preset(symbol_preset)
    };
    let sep = status_presets::resolve_separator(cfg.separator, ascii_only);
    let (left_names, right_names, has_gauge) = status_presets::resolve_segments(cfg);
    let has_fast = left_names.iter().any(|s| s.eq_ignore_ascii_case("fast"))
        || right_names.iter().any(|s| s.eq_ignore_ascii_case("fast"));
    let has_path = left_names.iter().any(|s| s.eq_ignore_ascii_case("path"));
    let extra_names: Vec<String> = right_names
        .iter()
        .filter(|s| !s.eq_ignore_ascii_case("title") && !s.eq_ignore_ascii_case("flash"))
        .cloned()
        .collect();
    let mut extra_right: Vec<Segment> =
        build_segments(&extra_names, snap, PATH_MAX, syms, cfg, has_fast, false)
            .into_iter()
            .map(|p| p.spans)
            .collect();
    let mut right = right_text(snap, &right_names, syms);
    let mut path_max = PATH_MAX;
    let mut left = build_segments(&left_names, snap, path_max, syms, cfg, has_fast, true);

    loop {
        let left_raw: Vec<Segment> = left.iter().map(|p| p.spans.clone()).collect();
        let mut spans = join_segments(&left_raw, sep, cfg.transparent);
        if !left_raw.is_empty() && !cfg.transparent && !sep.left_cap.is_empty() {
            spans.push(Span::styled(
                sep.left_cap.clone(),
                theme::fg(theme::SURFACE),
            ));
        }
        let right_spans = render_right_group(right.as_ref(), &extra_right, sep, cfg);
        let right_w = right_spans.as_deref().map_or(0, width_of);
        let fixed = width_of(&spans) + right_w;
        let needed_gap = if has_gauge {
            GAUGE_MIN
        } else {
            usize::from(right_w > 0)
        };
        if fixed + needed_gap <= width {
            if has_gauge {
                spans.extend(gauge(snap, width - fixed, syms, cfg.session_accent));
            } else if right_w > 0 {
                spans.push(Span::raw(" ".repeat(width - fixed)));
            }
            spans.extend(right_spans.unwrap_or_default());
            return Line::from(spans);
        }
        let over = fixed + needed_gap - width;
        if let Some((text, _)) = right.as_mut()
            && wrap::str_width(text) > TITLE_MIN
        {
            let target = (wrap::str_width(text).saturating_sub(over)).max(TITLE_MIN);
            *text = truncate_ellipsis(text, target, &syms.ellipsis);
        } else if right.is_some() || !extra_right.is_empty() {
            right = None;
            extra_right.clear();
        } else if has_path && path_max > PATH_MIN {
            path_max = path_max.saturating_sub(over).max(PATH_MIN);
            left = build_segments(&left_names, snap, path_max, syms, cfg, has_fast, true);
        } else if !shed_lowest_priority(&mut left) {
            let mut line = Line::from(clip_spans(spans, width, &syms.ellipsis));
            let pad_style = if cfg.transparent {
                Style::new()
            } else {
                theme::surface()
            };
            wrap::pad(&mut line, width, pad_style);
            return line;
        }
    }
}
