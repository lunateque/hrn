//! Per-segment renderers and drop-priority assignment for [`crate::status`].

use crate::git;
use crate::port_theme::StatuslineConfig;
use crate::render;
use crate::status::{Activity, Snapshot};
use crate::symbols::SymbolSet;
use crate::theme;
use crate::wrap;
use hrn_kernel::{PermissionMode, Tier};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use std::time::Duration;

pub(crate) type Segment = Vec<Span<'static>>;

pub(crate) struct Prioritized {
    /// Higher numbers are dropped first during squeezing (`<= 1` are never dropped).
    pub(crate) drop_priority: u8,
    pub(crate) spans: Segment,
}

pub(crate) fn prefix_icon(icon: &str, text: &str) -> String {
    if icon.is_empty() {
        text.to_owned()
    } else {
        format!("{icon} {text}")
    }
}

/// Truncate `text` to at most `max` display columns using `ellipsis`.
pub(crate) fn truncate_ellipsis(text: &str, max: usize, ellipsis: &str) -> String {
    if wrap::str_width(text) <= max {
        return text.to_owned();
    }
    let ell_w = wrap::str_width(ellipsis);
    if max <= ell_w {
        return ellipsis.chars().take(max).collect();
    }
    let budget = max - ell_w;
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = wrap::char_width(c);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push_str(ellipsis);
    out
}

pub(crate) fn accent_style(session_accent: bool) -> Style {
    if session_accent {
        theme::focus()
    } else {
        theme::fg(theme::TEXT)
    }
}

/// `12s`, `3m`, `4h`: the turn timer next to the working spinner.
fn timer(d: Duration) -> String {
    match d.as_secs() {
        s @ 0..60 => format!("{s}s"),
        s @ 60..3600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

fn brand(snap: &Snapshot<'_>, syms: &SymbolSet, session_accent: bool) -> Segment {
    let busy = accent_style(session_accent);
    let mut spans = match snap.activity {
        Activity::Idle => vec![Span::styled(syms.brand.clone(), theme::dim())],
        Activity::Working => {
            let clock = snap.elapsed.map(timer).unwrap_or_default();
            vec![Span::styled(
                format!("{} {clock}", syms.spinner_frame(snap.frame)),
                busy,
            )]
        }
        Activity::Compacting => vec![Span::styled(
            format!("{} compacting", syms.spinner_frame(snap.frame)),
            busy,
        )],
        Activity::Approval => vec![Span::styled(
            syms.approval.clone(),
            theme::fg(theme::WARNING).add_modifier(Modifier::BOLD),
        )],
        Activity::Question => vec![Span::styled(syms.question.clone(), busy)],
    };
    if let Some(profile) = snap.profile.map(str::trim).filter(|p| !p.is_empty()) {
        spans.push(Span::styled(format!(" [{profile}]"), theme::dim()));
    }
    spans
}

fn fast_label(tier: Tier, syms: &SymbolSet) -> Option<String> {
    match tier {
        Tier::Priority => Some(format!("{}fast", syms.fast)),
        Tier::Ultrafast => Some(format!("{}ultra", syms.fast)),
        _ => None,
    }
}

fn model(
    snap: &Snapshot<'_>,
    syms: &SymbolSet,
    session_accent: bool,
    inline_fast: bool,
) -> Segment {
    let glyph = syms.thinking_glyph(snap.thinking);
    let name = truncate_ellipsis(snap.model, 28, &syms.ellipsis);
    let level = snap.thinking.as_str();
    let dot = &syms.dot;
    let mut spans = vec![Span::styled(
        format!("{glyph} {name} {dot} {level}"),
        accent_style(session_accent),
    )];
    if snap.advisor {
        spans.push(Span::styled(format!(" {dot} adv"), theme::muted()));
    }
    if inline_fast && let Some(label) = fast_label(snap.fast_tier, syms) {
        spans.push(Span::styled(
            format!(" {dot} {label}"),
            theme::fg(theme::GOLD),
        ));
    }
    spans
}

fn mode(mode: PermissionMode) -> Option<Segment> {
    let (label, color) = match mode {
        PermissionMode::Plan => ("plan", theme::FOCUS),
        PermissionMode::AskCached => ("ask-cached", theme::FOCUS),
        PermissionMode::AutoEdit => ("auto-edit", theme::WARNING),
        PermissionMode::Yolo => ("yolo", theme::ERROR),
        PermissionMode::Default => return None,
    };
    Some(vec![Span::styled(label, theme::fg(color))])
}

fn goal(snap: &Snapshot<'_>, syms: &SymbolSet) -> Option<Segment> {
    let goal = snap.goal?;
    let mut text = prefix_icon(
        &syms.goal,
        &truncate_ellipsis(&goal.objective, 32, &syms.ellipsis),
    );
    if let Some(budget) = goal.token_budget {
        let _ = std::fmt::Write::write_fmt(
            &mut text,
            format_args!(" ({}k/{budget}k)", goal.tokens_used / 1000),
        );
    }
    let color = match goal.status {
        hrn_kernel::GoalStatus::Active => theme::SUCCESS,
        hrn_kernel::GoalStatus::BudgetLimited => theme::WARNING,
        _ => theme::TEXT,
    };
    Some(vec![Span::styled(text, theme::fg(color))])
}

fn focused(agent: Option<&str>, syms: &SymbolSet) -> Option<Segment> {
    let id = agent?;
    let shown = truncate_ellipsis(id, 20, &syms.ellipsis);
    Some(vec![Span::styled(
        prefix_icon(&syms.focused, &format!("agent:{shown}")),
        theme::accent().add_modifier(Modifier::BOLD),
    )])
}

fn running(ids: &[String], counts: crate::port::AgentCounts, syms: &SymbolSet) -> Option<Segment> {
    let sep = format!(" {} ", syms.dot);
    if counts.idle > 0 || counts.parked > 0 {
        let mut parts = Vec::new();
        let active = counts.running.max(ids.len());
        if active > 0 {
            parts.push(format!("{active} running"));
        }
        if counts.idle > 0 {
            parts.push(format!("{} idle", counts.idle));
        }
        if counts.parked > 0 {
            parts.push(format!("{} parked", counts.parked));
        }
        let text = prefix_icon(&syms.gear, &parts.join(&sep));
        return Some(vec![Span::styled(
            truncate_ellipsis(&text, 40, &syms.ellipsis),
            theme::fg(theme::GOLD),
        )]);
    }
    if !ids.is_empty() {
        let text = prefix_icon(
            &syms.gear,
            &format!("{} running: {}", ids.len(), ids.join(", ")),
        );
        return Some(vec![Span::styled(
            truncate_ellipsis(&text, 36, &syms.ellipsis),
            theme::fg(theme::GOLD),
        )]);
    }
    if counts.running > 0 {
        return Some(vec![Span::styled(
            prefix_icon(&syms.gear, &format!("{} running", counts.running)),
            theme::fg(theme::GOLD),
        )]);
    }
    None
}

fn path(cwd: &str, max: usize, syms: &SymbolSet) -> Segment {
    let shown = if wrap::str_width(cwd) > max {
        let ell_w = wrap::str_width(&syms.ellipsis);
        let mut tail: Vec<char> = Vec::new();
        let mut used = ell_w;
        for c in cwd.chars().rev() {
            let w = wrap::char_width(c);
            if used + w > max {
                break;
            }
            used += w;
            tail.push(c);
        }
        tail.reverse();
        format!("{}{}", syms.ellipsis, tail.into_iter().collect::<String>())
    } else {
        cwd.to_owned()
    };
    vec![Span::styled(
        prefix_icon(&syms.folder, &shown),
        theme::fg(theme::TEXT),
    )]
}

fn branch(name: &str, status: Option<git::Status>, syms: &SymbolSet) -> Segment {
    let clean = status.is_none_or(git::Status::is_clean);
    let color = if clean {
        theme::SUCCESS
    } else {
        theme::WARNING
    };
    let mut spans = vec![Span::styled(
        prefix_icon(&syms.branch, name),
        theme::fg(color),
    )];
    if let Some(s) = status {
        for (count, mark, color) in [
            (s.unstaged, '*', theme::WARNING),
            (s.staged, '+', theme::SUCCESS),
            (s.untracked, '?', theme::MUTED),
        ] {
            if count > 0 {
                spans.push(Span::styled(format!(" {mark}{count}"), theme::fg(color)));
            }
        }
    }
    spans
}

fn usage(tokens: (u64, u64), reasoning: u64, syms: &SymbolSet) -> Option<Segment> {
    (tokens != (0, 0) || reasoning > 0).then(|| {
        let mut text = format!(
            "{}{} {}{}",
            syms.tokens_up,
            render::tokens(tokens.0),
            syms.tokens_down,
            render::tokens(tokens.1)
        );
        if reasoning > 0 {
            text.push(' ');
            text.push_str(&syms.reasoning);
            text.push_str(&render::tokens(reasoning));
        }
        vec![Span::styled(text, theme::fg(theme::GOLD))]
    })
}

fn cost(micros: u64) -> Segment {
    vec![Span::styled(render::money(micros), theme::fg(theme::GOLD))]
}

fn hook_badge(snap: &Snapshot<'_>, syms: &SymbolSet, enabled: bool) -> Option<Segment> {
    if !enabled {
        return None;
    }
    let text = snap.hook_status.map(str::trim).filter(|s| !s.is_empty())?;
    Some(vec![Span::styled(
        prefix_icon(&syms.hook, &truncate_ellipsis(text, 24, &syms.ellipsis)),
        theme::muted(),
    )])
}

fn vim_badge(snap: &Snapshot<'_>, syms: &SymbolSet, session_accent: bool) -> Option<Segment> {
    let label = snap.vim_mode.map(str::trim).filter(|s| !s.is_empty())?;
    let style = accent_style(session_accent).add_modifier(Modifier::BOLD);
    match snap.vim_display {
        crate::port_theme::VimModeDisplay::None => None,
        crate::port_theme::VimModeDisplay::Icon => {
            Some(vec![Span::styled(syms.vim_icon.clone(), style)])
        }
        crate::port_theme::VimModeDisplay::Text => Some(vec![Span::styled(
            prefix_icon(&syms.vim_icon, label),
            style,
        )]),
    }
}

/// Pulsing glyph shown in the status band while background speculative compaction runs.
fn speculating(active: bool, frame: usize, syms: &SymbolSet) -> Option<Segment> {
    active.then(|| {
        vec![Span::styled(
            format!("{} compact", syms.pulse_frame(frame / render::PULSE_EVERY)),
            theme::fg(theme::ACCENT_DIM),
        )]
    })
}

fn render_one(
    name: &str,
    snap: &Snapshot<'_>,
    path_max: usize,
    syms: &SymbolSet,
    cfg: &StatuslineConfig,
    has_fast_seg: bool,
) -> Option<Prioritized> {
    let (prio, spans) = match name.to_ascii_lowercase().as_str() {
        "brand" => (0, brand(snap, syms, cfg.session_accent)),
        "model" => (1, model(snap, syms, cfg.session_accent, !has_fast_seg)),
        "compact" => (2, speculating(snap.speculating_compact, snap.frame, syms)?),
        "fast" => (
            2,
            vec![Span::styled(
                fast_label(snap.fast_tier, syms)?,
                theme::fg(theme::GOLD),
            )],
        ),
        "vim" => (2, vim_badge(snap, syms, cfg.session_accent)?),
        "focused" => (3, focused(snap.focused_agent, syms)?),
        "goal" => (4, goal(snap, syms)?),
        "mode" => (5, mode(snap.mode)?),
        "agents" => (6, running(snap.running, snap.agent_counts, syms)?),
        "hook" => (7, hook_badge(snap, syms, cfg.show_hook_status)?),
        "path" => (8, path(snap.cwd, path_max, syms)),
        "branch" => (9, branch(snap.branch?, snap.worktree, syms)),
        "cost" => (10, cost(snap.cost?)),
        "tokens" => (11, usage(snap.tokens, snap.reasoning_tokens, syms)?),
        "reasoning" => (12, usage(snap.tokens, snap.reasoning_tokens, syms)?),
        _ => return None,
    };
    Some(Prioritized {
        drop_priority: prio,
        spans,
    })
}

/// Build the ordered list of segments for `names`, preserving drop priorities for squeezing.
pub(crate) fn build_segments(
    names: &[String],
    snap: &Snapshot<'_>,
    path_max: usize,
    syms: &SymbolSet,
    cfg: &StatuslineConfig,
    has_fast_seg: bool,
    auto_hook: bool,
) -> Vec<Prioritized> {
    let mut out = Vec::new();
    let has_focused_name = names.iter().any(|n| n.eq_ignore_ascii_case("focused"));
    let has_hook_name = names.iter().any(|n| n.eq_ignore_ascii_case("hook"));
    let has_compact_name = names.iter().any(|n| n.eq_ignore_ascii_case("compact"));
    let has_vim_name = names.iter().any(|n| n.eq_ignore_ascii_case("vim"));
    let auto_vim = auto_hook
        && !has_vim_name
        && !matches!(
            cfg.preset,
            crate::port_theme::StatuslinePreset::Minimal
                | crate::port_theme::StatuslinePreset::Custom
        );
    for raw in names {
        if raw.eq_ignore_ascii_case("agents")
            && !has_focused_name
            && let Some(spans) = focused(snap.focused_agent, syms)
        {
            out.push(Prioritized {
                drop_priority: 3,
                spans,
            });
        }
        if let Some(seg) = render_one(raw, snap, path_max, syms, cfg, has_fast_seg) {
            out.push(seg);
        }
        if raw.eq_ignore_ascii_case("model") {
            if auto_vim && let Some(spans) = vim_badge(snap, syms, cfg.session_accent) {
                out.push(Prioritized {
                    drop_priority: 2,
                    spans,
                });
            }
            if !has_compact_name
                && let Some(spans) = speculating(snap.speculating_compact, snap.frame, syms)
            {
                out.push(Prioritized {
                    drop_priority: 2,
                    spans,
                });
            }
        }
    }
    if auto_hook
        && !has_hook_name
        && let Some(spans) = hook_badge(snap, syms, cfg.show_hook_status)
    {
        out.push(Prioritized {
            drop_priority: 7,
            spans,
        });
    }
    out
}
