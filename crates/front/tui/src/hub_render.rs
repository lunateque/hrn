//! Rendering for the live Agent Hub overlay (`hub.rs`): header aggregates, roster rows,
//! split/narrow inspector pane, and Activity log tab.

use crate::hub::{
    AgentHub, HubTab, RosterRow, compute_aggregates, format_cost_usd, format_duration_ms,
    format_inspector_lines,
};
use crate::port::{AgentActivityItem, AgentActivityKind, AgentHubStatus};
use crate::render;
use crate::theme;
use crate::wrap;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};
use std::fmt::Write as _;

/// Minimum terminal width to draw the roster and inspector side-by-side.
const WIDE_SPLIT_MIN: u16 = 100;

impl AgentHub {
    /// Render the live Agent Hub into `area`.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer, now_ms: i64) {
        if area.width < 20 || area.height < 7 {
            return;
        }
        Clear.render(area, buf);
        let hint = match self.tab {
            HubTab::Agents => {
                "1/2 tabs · t tree/flat · / filter · Tab inspector · Enter focus · r revive · x abort · Esc close"
            }
            HubTab::Activity => {
                "1/2 tabs · f filter · s scope · / search · Space follow · Enter jump · Esc close"
            }
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(self.title_line())
            .title_bottom(Line::from(Span::styled(
                format!(
                    " {} ",
                    wrap::truncate(hint, usize::from(area.width).saturating_sub(4))
                ),
                theme::muted(),
            )));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height < 3 {
            return;
        }

        // Row 0: header aggregate bar.
        let header = self.aggregate_bar(usize::from(inner.width));
        buf.set_line(inner.x, inner.y, &header, inner.width);

        // Row 1: filter / toolbar line.
        let toolbar = self.toolbar_line(usize::from(inner.width));
        buf.set_line(inner.x, inner.y + 1, &toolbar, inner.width);

        let body = Rect::new(
            inner.x,
            inner.y + 2,
            inner.width,
            inner.height.saturating_sub(2),
        );
        match self.tab {
            HubTab::Agents => self.draw_agents_body(body, buf, now_ms),
            HubTab::Activity => self.draw_activity_body(body, buf, now_ms),
        }
    }

    fn title_line(&self) -> Line<'static> {
        let active = theme::focus().add_modifier(Modifier::BOLD);
        let idle = theme::muted();
        Line::from(vec![
            Span::styled(" Agent Hub ", theme::bold()),
            Span::styled(
                " [1] Agents ",
                if self.tab == HubTab::Agents {
                    active
                } else {
                    idle
                },
            ),
            Span::styled(
                " [2] Activity ",
                if self.tab == HubTab::Activity {
                    active
                } else {
                    idle
                },
            ),
        ])
    }

    fn aggregate_bar(&self, width: usize) -> Line<'static> {
        let agg = compute_aggregates(&self.snapshot.agents);
        let mut text = format!(
            " {} agents · {} running · {} idle · {} parked",
            agg.total, agg.running, agg.idle, agg.parked
        );
        if agg.aborted > 0 {
            let _ = write!(text, " · {} aborted", agg.aborted);
        }
        if agg.failed > 0 {
            let _ = write!(text, " · {} failed", agg.failed);
        }
        if agg.unread > 0 {
            let _ = write!(text, " · {} unread", agg.unread);
        }
        let _ = write!(
            text,
            "  |  {} req · {} tools · ↑{} ↓{}",
            agg.requests,
            agg.tool_calls,
            render::tokens(agg.input_tokens),
            render::tokens(agg.output_tokens),
        );
        if let Some(cost) = agg.cost_usd {
            let _ = write!(text, " · {}", format_cost_usd(cost));
        }
        Line::from(Span::styled(
            wrap::truncate(&text, width).into_owned(),
            theme::fg(theme::GOLD),
        ))
    }

    fn toolbar_line(&self, width: usize) -> Line<'static> {
        let text = match self.tab {
            HubTab::Agents => {
                let mode = if self.tree_mode { "tree" } else { "flat" };
                let filter_str = if self.filter_editing {
                    format!("/{}_", self.filter)
                } else if !self.filter.is_empty() {
                    format!("/{} (Esc clears)", self.filter)
                } else {
                    "/ filter".to_owned()
                };
                format!(" view: {mode} (t)  ·  {filter_str}")
            }
            HubTab::Activity => {
                let follow = if self.activity_follow { "on" } else { "off" };
                let search = if self.activity_search_editing {
                    format!("/{}_", self.activity_search)
                } else if !self.activity_search.is_empty() {
                    format!("/{} (Esc clears)", self.activity_search)
                } else {
                    "/ search".to_owned()
                };
                format!(
                    " filter: {} (f) · scope: {} (s) · follow: {follow} (Space) · {search}",
                    self.activity_filter.label(),
                    self.activity_scope.label()
                )
            }
        };
        Line::from(Span::styled(
            wrap::truncate(&text, width).into_owned(),
            theme::muted(),
        ))
    }

    fn draw_agents_body(&self, area: Rect, buf: &mut Buffer, now_ms: i64) {
        if area.width >= WIDE_SPLIT_MIN {
            let left_w = (area.width * 58) / 100;
            let left = Rect::new(area.x, area.y, left_w, area.height);
            let sep_x = area.x + left_w;
            let right = Rect::new(
                sep_x + 1,
                area.y,
                area.width.saturating_sub(left_w + 1),
                area.height,
            );
            self.draw_roster(left, buf, now_ms);
            for y in area.y..area.bottom() {
                buf.set_line(sep_x, y, &Line::from(Span::styled("│", theme::border())), 1);
            }
            self.draw_inspector(right, buf, now_ms);
        } else if self.show_inspector_narrow {
            self.draw_inspector(area, buf, now_ms);
        } else {
            self.draw_roster(area, buf, now_ms);
        }
    }

    fn draw_roster(&self, area: Rect, buf: &mut Buffer, now_ms: i64) {
        let rows = self.roster_rows();
        if rows.is_empty() {
            let msg = if self.filter.is_empty() {
                "  (no agents in this session yet)"
            } else {
                "  (no agents match filter)"
            };
            buf.set_line(
                area.x,
                area.y,
                &Line::from(Span::styled(msg, theme::muted())),
                area.width,
            );
            return;
        }
        let visible = usize::from(area.height);
        let w = usize::from(area.width);
        for (idx, row) in rows.iter().skip(self.roster_top).take(visible).enumerate() {
            let abs = self.roster_top + idx;
            let is_sel = abs == self.selected_roster;
            let line = format_roster_row(row, is_sel, self.tree_mode, now_ms, w);
            buf.set_line(
                area.x,
                area.y + u16::try_from(idx).unwrap_or(u16::MAX),
                &line,
                area.width,
            );
        }
    }

    fn draw_inspector(&self, area: Rect, buf: &mut Buffer, now_ms: i64) {
        let Some(entry) = self.selected_entry() else {
            buf.set_line(
                area.x,
                area.y,
                &Line::from(Span::styled(" (no agent selected)", theme::muted())),
                area.width,
            );
            return;
        };
        let lines = format_inspector_lines(entry, now_ms);
        let w = usize::from(area.width).saturating_sub(1);
        for (idx, text) in lines.iter().take(usize::from(area.height)).enumerate() {
            let style = if idx == 0 {
                theme::focus().add_modifier(Modifier::BOLD)
            } else if text.starts_with("retry:") {
                theme::fg(theme::WARNING)
            } else if text.starts_with("tool:") {
                theme::accent()
            } else {
                theme::fg(theme::TEXT)
            };
            let line = Line::from(Span::styled(
                format!(" {}", wrap::truncate(&wrap::sanitize(text), w)),
                style,
            ));
            buf.set_line(
                area.x,
                area.y + u16::try_from(idx).unwrap_or(u16::MAX),
                &line,
                area.width,
            );
        }
    }

    fn draw_activity_body(&self, area: Rect, buf: &mut Buffer, now_ms: i64) {
        let items = self.activity_rows();
        if items.is_empty() {
            buf.set_line(
                area.x,
                area.y,
                &Line::from(Span::styled(
                    "  (no matching activity entries)",
                    theme::muted(),
                )),
                area.width,
            );
            return;
        }
        let visible = usize::from(area.height);
        let w = usize::from(area.width);
        let top = if self.activity_follow {
            items.len().saturating_sub(visible)
        } else {
            self.activity_top.min(items.len().saturating_sub(1))
        };
        for (idx, item) in items.iter().skip(top).take(visible).enumerate() {
            let abs = top + idx;
            let is_sel = abs == self.selected_activity;
            let line = format_activity_row(item, is_sel, now_ms, w);
            buf.set_line(
                area.x,
                area.y + u16::try_from(idx).unwrap_or(u16::MAX),
                &line,
                area.width,
            );
        }
    }
}

fn status_badge(status: AgentHubStatus) -> (&'static str, Color) {
    match status {
        AgentHubStatus::Running => ("● run ", theme::FOCUS),
        AgentHubStatus::Idle => ("○ idle", theme::SUCCESS),
        AgentHubStatus::Parked => ("◌ park", theme::MUTED),
        AgentHubStatus::Aborted => ("✕ abrt", theme::WARNING),
        AgentHubStatus::Failed => ("! fail", theme::ERROR),
    }
}

fn format_roster_row(
    row: &RosterRow<'_>,
    is_sel: bool,
    tree_mode: bool,
    now_ms: i64,
    width: usize,
) -> Line<'static> {
    let entry = row.entry;
    let (badge, color) = status_badge(entry.status);
    let end_ms = if entry.finished_ms > 0 {
        entry.finished_ms
    } else {
        now_ms
    };
    let age = format_duration_ms(end_ms.saturating_sub(entry.started_ms));
    let mut tail = String::new();
    if !tree_mode && let Some(parent) = &entry.parent {
        let _ = write!(tail, " (←{parent})");
    }
    if entry.unread > 0 {
        let _ = write!(tail, " [{}]", entry.unread);
    }
    let model = entry.model.as_deref().unwrap_or("—");
    let _ = write!(tail, "  {}·{model}·{age}", entry.role);
    let act = if entry.activity.is_empty() {
        entry.task.as_str()
    } else {
        entry.activity.as_str()
    };
    if !act.is_empty() {
        let _ = write!(tail, "  {}", wrap::sanitize(act));
    }
    let has_usage = entry.requests > 0
        || entry.tool_calls > 0
        || entry.input_tokens > 0
        || entry.output_tokens > 0
        || entry.cost_usd.is_some();
    if has_usage {
        let _ = write!(
            tail,
            "  {}r/{}t ↑{}↓{}",
            entry.requests,
            entry.tool_calls,
            render::tokens(entry.input_tokens),
            render::tokens(entry.output_tokens)
        );
        if let Some(cost) = entry.cost_usd {
            let _ = write!(tail, " {}", format_cost_usd(cost));
        }
    }
    let mark = if is_sel { "▸ " } else { "  " };
    let id_col = format!("{}{}", row.tree_prefix, entry.id);
    let fixed_w = 2 + wrap::str_width(badge) + 1 + wrap::str_width(&id_col);
    let tail_trunc = wrap::truncate(&tail, width.saturating_sub(fixed_w)).into_owned();
    let id_style = if is_sel {
        theme::focus().add_modifier(Modifier::BOLD)
    } else if row.depth == 0 {
        theme::fg(theme::TEXT).add_modifier(Modifier::BOLD)
    } else {
        theme::fg(theme::TEXT)
    };
    let mut spans = vec![
        Span::styled(mark, theme::focus()),
        Span::styled(badge, theme::fg(color).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(row.tree_prefix.clone(), theme::border()),
        Span::styled(entry.id.clone(), id_style),
        Span::styled(tail_trunc, theme::muted()),
    ];
    if is_sel {
        pad_selection(&mut spans, width);
    }
    Line::from(spans)
}

fn format_activity_row(
    item: &AgentActivityItem,
    is_sel: bool,
    now_ms: i64,
    width: usize,
) -> Line<'static> {
    let kind_color = match item.kind {
        AgentActivityKind::Error => theme::ERROR,
        AgentActivityKind::Response => theme::SUCCESS,
        AgentActivityKind::Tool => theme::ACCENT,
        AgentActivityKind::Irc => theme::GOLD,
        AgentActivityKind::Lifecycle => theme::MUTED,
    };
    let age = if item.ts_ms > 0 {
        format_duration_ms(now_ms.saturating_sub(item.ts_ms))
    } else {
        "—".to_owned()
    };
    let mark = if is_sel { "▸ " } else { "  " };
    let head = format!("{age:>5} {:<4} {:<14} ", item.kind.as_str(), item.agent_id);
    let body = match &item.detail {
        Some(detail) if !detail.is_empty() => format!("{} — {detail}", item.summary),
        _ => item.summary.clone(),
    };
    let max_body = width.saturating_sub(2 + wrap::str_width(&head));
    let body_trunc = wrap::truncate(&wrap::sanitize(&body), max_body).into_owned();
    let mut spans = vec![
        Span::styled(mark, theme::focus()),
        Span::styled(format!("{age:>5} "), theme::dim()),
        Span::styled(
            format!("{:<4} ", item.kind.as_str()),
            Style::new().fg(kind_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<14} ", wrap::truncate(&item.agent_id, 14)),
            theme::fg(theme::TEXT),
        ),
        Span::styled(body_trunc, theme::muted()),
    ];
    if is_sel {
        pad_selection(&mut spans, width);
    }
    Line::from(spans)
}

fn pad_selection(spans: &mut Vec<Span<'static>>, width: usize) {
    let used: usize = spans.iter().map(Span::width).sum();
    if width > used {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    for span in spans.iter_mut() {
        span.style = span.style.bg(theme::SELECTED_BG);
    }
}
