//! Pinned Subagents jump block rendered above the editor prompt when background subagents exist.
//!
//! Respects `[tui] pinned_agents = "collapsed" | "full" | "off"` and
//! `[tui] subagent_live_preview = true` (which adds a second preview line showing the active tool
//! and elapsed duration once a running tool call exceeds 5 seconds).

use crate::hub::format_duration_ms;
use crate::port::{AgentCounts, AgentHubEntry, AgentHubStatus, PinnedAgentsMode};
use crate::theme;
use crate::wrap;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use std::fmt::Write as _;

/// Threshold before the live-preview second line appears under a running subagent (`5s`).
pub(crate) const LIVE_PREVIEW_THRESHOLD_MS: i64 = 5_000;
/// Maximum subagent entries shown in `collapsed` mode.
const COLLAPSED_MAX_ROWS: usize = 3;
/// Maximum subagent entries shown in `full` mode.
const FULL_MAX_ROWS: usize = 12;

/// Rendered jump block lines plus per-row click hitboxes (`(line_index, agent_id)`).
pub(crate) struct PinnedBlock {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) hitboxes: Vec<(usize, String)>,
}

/// Build the pinned Subagents jump block for `entries` (excluding `is_main`).
#[must_use]
pub(crate) fn render(
    entries: &[AgentHubEntry],
    mode: PinnedAgentsMode,
    live_preview: bool,
    focused_agent: Option<&str>,
    now_ms: i64,
    width: usize,
) -> PinnedBlock {
    if mode == PinnedAgentsMode::Off || width < 16 {
        return PinnedBlock {
            lines: Vec::new(),
            hitboxes: Vec::new(),
        };
    }
    let subs: Vec<&AgentHubEntry> = entries.iter().filter(|e| !e.is_main).collect();
    if subs.is_empty() {
        return PinnedBlock {
            lines: Vec::new(),
            hitboxes: Vec::new(),
        };
    }

    let counts = AgentCounts::from_entries(entries);
    let mut header = String::from(" ── Subagents");
    let mut parts = Vec::new();
    if counts.running > 0 {
        parts.push(format!("{} running", counts.running));
    }
    if counts.idle > 0 {
        parts.push(format!("{} idle", counts.idle));
    }
    if counts.parked > 0 {
        parts.push(format!("{} parked", counts.parked));
    }
    if !parts.is_empty() {
        let _ = write!(header, " ({})", parts.join(" · "));
    }
    let _ = write!(header, "  [Alt+A]");

    let mut lines = vec![Line::from(Span::styled(
        wrap::truncate(&header, width).into_owned(),
        theme::dim(),
    ))];
    let mut hitboxes = Vec::new();

    // Prioritize running agents first, then idle, then others.
    let mut ordered = subs;
    ordered.sort_by_key(|e| match e.status {
        AgentHubStatus::Running => 0u8,
        AgentHubStatus::Idle => 1,
        AgentHubStatus::Parked => 2,
        AgentHubStatus::Aborted => 3,
        AgentHubStatus::Failed => 4,
    });

    let cap = match mode {
        PinnedAgentsMode::Collapsed => COLLAPSED_MAX_ROWS,
        PinnedAgentsMode::Full => FULL_MAX_ROWS,
        PinnedAgentsMode::Off => 0,
    };
    let omitted = ordered.len().saturating_sub(cap);

    for entry in ordered.into_iter().take(cap) {
        let line_idx = lines.len();
        hitboxes.push((line_idx, entry.id.clone()));
        lines.push(agent_row(
            entry,
            mode == PinnedAgentsMode::Full,
            focused_agent == Some(entry.id.as_str()),
            now_ms,
            width,
        ));
        if live_preview
            && entry.status == AgentHubStatus::Running
            && let Some(preview) = live_tool_preview(entry, now_ms, width)
        {
            hitboxes.push((lines.len(), entry.id.clone()));
            lines.push(preview);
        }
    }
    if omitted > 0 {
        lines.push(Line::from(Span::styled(
            format!("   … +{omitted} more (Alt+A)"),
            theme::dim(),
        )));
    }

    PinnedBlock { lines, hitboxes }
}

fn agent_row(
    entry: &AgentHubEntry,
    full: bool,
    focused: bool,
    now_ms: i64,
    width: usize,
) -> Line<'static> {
    let (glyph, color) = match entry.status {
        AgentHubStatus::Running => ("●", theme::FOCUS),
        AgentHubStatus::Idle => ("○", theme::SUCCESS),
        AgentHubStatus::Parked => ("◌", theme::MUTED),
        AgentHubStatus::Aborted => ("✕", theme::WARNING),
        AgentHubStatus::Failed => ("!", theme::ERROR),
    };
    let end_ms = if entry.finished_ms > 0 {
        entry.finished_ms
    } else {
        now_ms
    };
    let age = format_duration_ms(end_ms.saturating_sub(entry.started_ms));
    let mut meta = format!(" {}", entry.status.as_str());
    if full {
        let model = entry.model.as_deref().unwrap_or("—");
        let _ = write!(meta, " · {}·{model}·{age}", entry.role);
    } else {
        let _ = write!(meta, " · {age}");
    }
    if entry.unread > 0 {
        let _ = write!(meta, " [{}]", entry.unread);
    }
    let summary = if entry.activity.is_empty() {
        entry.task.as_str()
    } else {
        entry.activity.as_str()
    };
    if !summary.is_empty() {
        let _ = write!(meta, " — {}", wrap::sanitize(summary));
    }
    let prefix = if focused { " ▸ " } else { "   " };
    let used = 3 + 2 + wrap::str_width(&entry.id);
    let meta_trunc = wrap::truncate(&meta, width.saturating_sub(used)).into_owned();
    Line::from(vec![
        Span::styled(prefix, theme::focus()),
        Span::styled(
            format!("{glyph} "),
            theme::fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            entry.id.clone(),
            if focused {
                theme::focus().add_modifier(Modifier::BOLD)
            } else {
                theme::fg(theme::TEXT)
            },
        ),
        Span::styled(meta_trunc, theme::muted()),
    ])
}

/// Second preview line rendered under a running subagent once its active tool exceeds 5 seconds.
fn live_tool_preview(entry: &AgentHubEntry, now_ms: i64, width: usize) -> Option<Line<'static>> {
    let tool = entry.current_tool.as_deref()?;
    let started = entry.current_tool_started_ms?;
    let elapsed_ms = now_ms.saturating_sub(started);
    if elapsed_ms <= LIVE_PREVIEW_THRESHOLD_MS {
        return None;
    }
    let elapsed = format_duration_ms(elapsed_ms);
    let mut text = format!("     ↳ {tool} ({elapsed})");
    if let Some(args) = &entry.current_tool_args
        && !args.is_empty()
    {
        let _ = write!(text, " {}", wrap::sanitize(args));
    }
    Some(Line::from(Span::styled(
        wrap::truncate(&text, width).into_owned(),
        theme::accent(),
    )))
}
