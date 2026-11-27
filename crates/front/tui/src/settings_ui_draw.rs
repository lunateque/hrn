//! Compact, centered `omp`-style renderer for the interactive `/settings` overlay.

use crate::port::SettingItem;
use crate::settings_catalog::{self, ALL_TABS, SettingMeta};
use crate::settings_picker::ChoicePickerState;
use crate::settings_picker_draw::{
    build_footer_hint, build_inspector_meta, draw_edit_bar, styled_value_span,
};
use crate::settings_ui::{SettingsRow, SettingsUi};
use crate::theme;
use crate::wrap;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

const MAX_PANEL_WIDTH: u16 = 92;
const MAX_PANEL_HEIGHT: u16 = 30;
const MIN_PANEL_HEIGHT: u16 = 14;

pub(crate) fn to_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

fn short_source_tag(source: &str) -> &'static str {
    let lower = source.to_ascii_lowercase();
    for (prefix, tag) in [
        ("default", ""),
        ("project", "project"),
        ("global", "global"),
        ("env", "env"),
        ("session", "session"),
        ("cli", "cli"),
    ] {
        if lower.starts_with(prefix) {
            return tag;
        }
    }
    "custom"
}

pub(crate) fn centered_panel(area: Rect, list_rows: usize, inspector_h: u16) -> Rect {
    let panel_w = area.width.min(MAX_PANEL_WIDTH);
    let max_h = if area.height > 16 {
        area.height.saturating_sub(2).min(MAX_PANEL_HEIGHT)
    } else {
        area.height
    };
    let min_h = MIN_PANEL_HEIGHT.min(max_h);
    let desired_list = to_u16(list_rows.max(3));
    let panel_h = (2 + 3 + desired_list + inspector_h).clamp(min_h, max_h);
    let px = area.x + area.width.saturating_sub(panel_w) / 2;
    let py = area.y + area.height.saturating_sub(panel_h) / 2;
    Rect::new(px, py, panel_w, panel_h)
}

pub(crate) fn content_rect(panel: Rect) -> Option<Rect> {
    let inner = Block::bordered()
        .border_type(BorderType::Rounded)
        .inner(panel);
    let pad = u16::from(inner.width >= 26);
    let content = Rect::new(
        inner.x + pad,
        inner.y,
        inner.width.saturating_sub(pad * 2),
        inner.height,
    );
    (content.height >= 6 && content.width >= 12).then_some(content)
}

fn wrap_description(desc: &str, width: usize) -> Vec<Line<'static>> {
    let clean = wrap::sanitize(desc);
    let mut lines = wrap::wrap_text(&clean, theme::fg(theme::TEXT), width, &[], &[]);
    if lines.len() > 2 {
        let mut second = lines.get(1).map_or_else(String::new, ToString::to_string);
        if wrap::str_width(&second) >= width {
            second = wrap::truncate(&second, width).into_owned();
        } else {
            second = format!("{}…", second.trim_end());
        }
        lines.truncate(1);
        lines.push(Line::from(Span::styled(second, theme::fg(theme::TEXT))));
    }
    lines
}

impl SettingsUi {
    /// Draw the `/settings` overlay centered inside `area`, returning the active input cursor.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        if area.width < 24 || area.height < 8 {
            return None;
        }
        Clear.render(area, buf);
        if let Some(picker) = self.choice_picker() {
            return self.draw_choice_picker(picker, area, buf);
        }
        let rows = self.visible_rows();
        let approx_content_w = usize::from(area.width.min(MAX_PANEL_WIDTH).saturating_sub(4));
        let desc_lines = self.current_item().map_or_else(Vec::new, |item| {
            wrap_description(&item.description, approx_content_w.max(12))
        });
        let inspector_h = if area.height >= 12 {
            to_u16(3 + desc_lines.len().clamp(1, 2))
        } else {
            2
        };
        let panel = centered_panel(area, rows.len(), inspector_h);
        self.frame_block(None).render(panel, buf);
        let content = content_rect(panel)?;

        let item_count = rows
            .iter()
            .filter(|r| matches!(r, SettingsRow::Item(_)))
            .count();
        self.draw_tabs(content.x, content.y, content.width, buf);
        let search_cursor = self.draw_search(
            content.x,
            content.y + 1,
            content.width,
            item_count,
            self.items().len(),
            buf,
        );

        let actual_insp_h = inspector_h.min(content.height.saturating_sub(4));
        let list_y = content.y + 3;
        let list_h = content.height.saturating_sub(3 + actual_insp_h);
        let visible_h = usize::from(list_h);
        let top = self.sync_scroll_top(&rows, visible_h);
        let scroll_badge = format_scroll_badge(top, visible_h, rows.len());
        draw_rule_with_badge(
            content.x,
            content.y + 2,
            content.width,
            scroll_badge.as_deref(),
            buf,
        );

        let list_area = Rect::new(content.x, list_y, content.width, list_h);
        self.draw_list(&rows, top, list_area, buf);

        let insp_area = Rect::new(content.x, list_y + list_h, content.width, actual_insp_h);
        let edit_cursor = self.draw_inspector(&desc_lines, insp_area, buf);

        if self.editing().is_some() {
            edit_cursor
        } else {
            Some(search_cursor)
        }
    }

    pub(crate) fn frame_block(&self, picker: Option<&ChoicePickerState>) -> Block<'static> {
        let mut title_spans = vec![Span::styled(" settings ", theme::bold())];
        if let Some(p) = picker {
            title_spans.extend([
                Span::styled("· ", theme::dim()),
                Span::styled(
                    format!("{} ", p.title),
                    theme::accent().add_modifier(Modifier::BOLD),
                ),
            ]);
        } else if self.modified_count() > 0 {
            title_spans.extend([
                Span::styled("· ", theme::dim()),
                Span::styled(
                    format!("{} modified ", self.modified_count()),
                    theme::fg(theme::GOLD),
                ),
            ]);
        }
        let legend = if let Some(p) = picker {
            let nav = if p.preview_theme { "preview" } else { "move" };
            hint_line(&[
                ("enter", "apply"),
                ("↑/↓", nav),
                ("ctrl+u", "clear"),
                ("esc", "cancel"),
            ])
        } else if self.editing().is_some() {
            hint_line(&[("enter", "save"), ("ctrl+u", "clear"), ("esc", "cancel")])
        } else {
            hint_line(&[
                ("enter", "open/toggle"),
                ("←/→", "cycle"),
                ("tab", "section"),
                ("del", "reset"),
                ("esc", "close"),
            ])
        };
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Line::from(title_spans))
            .title_bottom(legend)
    }

    fn draw_tabs(&self, x: u16, y: u16, width: u16, buf: &mut Buffer) {
        let searching = !self.query().trim().is_empty();
        let mut spans: Vec<Span<'static>> = Vec::new();
        for tab in ALL_TABS {
            let active = !searching && tab == self.tab();
            let style = if active {
                theme::focus()
                    .bg(theme::SELECTED_BG)
                    .add_modifier(Modifier::BOLD)
            } else if searching {
                theme::dim()
            } else {
                theme::muted()
            };
            spans.push(Span::styled(format!(" {} ", tab.label()), style));
        }
        if searching {
            let used: usize = spans.iter().map(Span::width).sum();
            let badge = "searching all";
            let w = usize::from(width);
            if w >= used + badge.len() + 2 {
                spans.push(Span::raw(" ".repeat(w - used - badge.len())));
                spans.push(Span::styled(badge, theme::accent()));
            }
        }
        buf.set_line(x, y, &Line::from(spans), width);
    }

    fn draw_search(
        &self,
        x: u16,
        y: u16,
        width: u16,
        shown: usize,
        total: usize,
        buf: &mut Buffer,
    ) -> Position {
        let w = usize::from(width);
        let searching = !self.query().trim().is_empty();
        let count_text = if searching && w < 80 {
            format!("searching all · {shown}/{total}")
        } else {
            format!("{shown}/{total}")
        };
        let count_w = wrap::str_width(&count_text);
        let prompt = "❯ ";
        let prompt_w = wrap::str_width(prompt);
        let query_max = w.saturating_sub(prompt_w + count_w + 2);
        let query_disp = wrap::truncate(self.query(), query_max).into_owned();
        let query_w = wrap::str_width(&query_disp);
        let (mid_span, used_mid) = if self.query().is_empty() {
            let ph = wrap::truncate("type to search settings…", query_max).into_owned();
            let pw = wrap::str_width(&ph);
            (Span::styled(ph, theme::dim()), pw)
        } else {
            (Span::styled(query_disp, theme::fg(theme::TEXT)), query_w)
        };
        let pad = w.saturating_sub(prompt_w + used_mid + count_w);
        let line = Line::from(vec![
            Span::styled(prompt, theme::accent().add_modifier(Modifier::BOLD)),
            mid_span,
            Span::raw(" ".repeat(pad)),
            Span::styled(count_text, theme::dim()),
        ]);
        buf.set_line(x, y, &line, width);
        let cursor_offset = prompt_w + if self.query().is_empty() { 0 } else { query_w };
        let cx = x + to_u16(cursor_offset.min(w.saturating_sub(1)));
        Position::new(cx, y)
    }

    fn draw_list(&self, rows: &[SettingsRow], first: usize, area: Rect, buf: &mut Buffer) {
        let visible_h = usize::from(area.height);
        if visible_h == 0 {
            return;
        }
        if rows.is_empty() {
            let line = Line::from(Span::styled(
                "  no settings match the current filter",
                theme::dim(),
            ));
            buf.set_line(area.x, area.y, &line, area.width);
            return;
        }
        let searching = !self.query().trim().is_empty();
        for (offset, row) in rows.iter().skip(first).take(visible_h).enumerate() {
            let abs = first + offset;
            let y = area.y + to_u16(offset);
            match row {
                SettingsRow::Section(name) => draw_section_row(name, area.x, y, area.width, buf),
                SettingsRow::Item(idx) => {
                    if let Some(item) = self.items().get(*idx) {
                        let meta = settings_catalog::classify(item);
                        let is_sel = abs == self.selected();
                        draw_setting_row(
                            item,
                            &meta,
                            is_sel,
                            searching,
                            Rect::new(area.x, y, area.width, 1),
                            buf,
                        );
                    }
                }
            }
        }
    }

    fn draw_inspector(
        &self,
        desc_lines: &[Line<'static>],
        area: Rect,
        buf: &mut Buffer,
    ) -> Option<Position> {
        if area.height == 0 {
            return None;
        }
        draw_rule_with_badge(area.x, area.y, area.width, None, buf);
        if area.height < 2 {
            return None;
        }
        let w = usize::from(area.width);
        let item = self.current_item();
        let mut cursor_y = area.y + 1;
        let footer_y = area.y + area.height - 1;
        let max_desc_rows = usize::from(area.height.saturating_sub(3)).min(2);
        if item.is_some() {
            for line in desc_lines.iter().take(max_desc_rows) {
                if cursor_y < footer_y {
                    buf.set_line(area.x, cursor_y, line, area.width);
                    cursor_y += 1;
                }
            }
        }
        if let Some(item) = item
            && cursor_y < footer_y
        {
            let meta_line = build_inspector_meta(item, w);
            buf.set_line(area.x, cursor_y, &meta_line, area.width);
        }
        if let Some(edit) = self.editing() {
            return Some(draw_edit_bar(edit, area.x, footer_y, area.width, buf));
        }
        if let Some((msg, is_err)) = self.status() {
            let (icon, style) = if is_err {
                ("! ", theme::fg(theme::ERROR).add_modifier(Modifier::BOLD))
            } else {
                ("✓ ", theme::fg(theme::SUCCESS).add_modifier(Modifier::BOLD))
            };
            let line = Line::from(vec![
                Span::styled(icon, style),
                Span::styled(
                    wrap::truncate(&wrap::sanitize(msg), w.saturating_sub(2)).into_owned(),
                    style,
                ),
            ]);
            buf.set_line(area.x, footer_y, &line, area.width);
            return None;
        }
        let hint_line = item.map_or_else(
            || {
                Line::from(Span::styled(
                    wrap::truncate("esc clear search  ·  tab switch category", w).into_owned(),
                    theme::dim(),
                ))
            },
            |it| build_footer_hint(it, w),
        );
        buf.set_line(area.x, footer_y, &hint_line, area.width);
        None
    }
}

fn hint_line(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            theme::accent().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}"), theme::muted()));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

pub(crate) fn format_scroll_badge(top: usize, visible_h: usize, total: usize) -> Option<String> {
    if total <= visible_h || visible_h == 0 {
        return None;
    }
    let above = top;
    let below = total.saturating_sub(top + visible_h);
    match (above > 0, below > 0) {
        (true, true) => Some(format!("↑ {above} · ↓ {below}")),
        (true, false) => Some(format!("↑ {above}")),
        (false, true) => Some(format!("↓ {below}")),
        (false, false) => None,
    }
}

pub(crate) fn draw_rule_with_badge(
    x: u16,
    y: u16,
    width: u16,
    badge: Option<&str>,
    buf: &mut Buffer,
) {
    let w = usize::from(width);
    let line = match badge {
        Some(text) if w > wrap::str_width(text) + 4 => {
            let label = format!(" {text} ");
            let lw = wrap::str_width(&label);
            let left_w = w.saturating_sub(lw + 2);
            Line::from(vec![
                Span::styled("─".repeat(left_w), theme::fg(theme::BORDER_MUTED)),
                Span::styled(label, theme::dim()),
                Span::styled("──", theme::fg(theme::BORDER_MUTED)),
            ])
        }
        _ => Line::from(Span::styled("─".repeat(w), theme::fg(theme::BORDER_MUTED))),
    };
    buf.set_line(x, y, &line, width);
}

fn draw_section_row(name: &str, x: u16, y: u16, width: u16, buf: &mut Buffer) {
    let w = usize::from(width);
    let label = format!("{name} ");
    let lw = wrap::str_width(&label);
    let rule = "─".repeat(w.saturating_sub(lw));
    let line = Line::from(vec![
        Span::styled(label, theme::accent().add_modifier(Modifier::BOLD)),
        Span::styled(rule, theme::fg(theme::BORDER_MUTED)),
    ]);
    buf.set_line(x, y, &line, width);
}

fn draw_setting_row(
    item: &SettingItem,
    meta: &SettingMeta,
    is_sel: bool,
    searching: bool,
    row: Rect,
    buf: &mut Buffer,
) {
    let w = usize::from(row.width);
    let mark = if is_sel { "▸ " } else { "  " };
    let tag = if item.modified {
        short_source_tag(&item.source)
    } else {
        ""
    };
    let tag_str = if tag.is_empty() {
        String::new()
    } else {
        format!("[{tag}]")
    };
    let tag_w = if tag_str.is_empty() {
        0
    } else {
        2 + wrap::str_width(&tag_str)
    };
    let mod_w = if item.modified { 3 } else { 0 };
    let val_max = (w * 46 / 100)
        .max(14)
        .min(w.saturating_sub(14 + mod_w + tag_w));
    let val_span = styled_value_span(item, is_sel, val_max);
    let val_w = val_span.width();

    let label_budget = w.saturating_sub(2 + mod_w + 2 + val_w + tag_w);
    let label_disp = wrap::truncate(&meta.label, label_budget).into_owned();
    let label_w = wrap::str_width(&label_disp);
    let sec_hint = if searching {
        let candidate = format!(" · {}", meta.section);
        let cw = wrap::str_width(&candidate);
        (label_w + cw + 2 <= label_budget).then_some((candidate, cw))
    } else {
        None
    };
    let sec_w = sec_hint.as_ref().map_or(0, |(_, cw)| *cw);
    let label_style = if is_sel {
        theme::focus().add_modifier(Modifier::BOLD)
    } else {
        theme::fg(theme::TEXT)
    };
    let gap = w.saturating_sub(2 + label_w + sec_w + mod_w + val_w + tag_w);

    let mut spans = vec![
        Span::styled(mark, theme::focus()),
        Span::styled(label_disp, label_style),
    ];
    if let Some((sec, _)) = sec_hint {
        spans.push(Span::styled(sec, theme::dim()));
    }
    if item.modified {
        spans.push(Span::styled("  •", theme::fg(theme::GOLD)));
    }
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(val_span);
    if !tag_str.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(tag_str, theme::dim()));
    }
    if is_sel {
        for span in &mut spans {
            span.style = span.style.bg(theme::SELECTED_BG);
        }
    }
    buf.set_line(row.x, row.y, &Line::from(spans), row.width);
}
