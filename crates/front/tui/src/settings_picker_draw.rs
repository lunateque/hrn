//! Renderer for the searchable enum/theme choice picker submenu inside `/settings`.

use crate::port::{SettingItem, SettingKind};
use crate::settings_picker::ChoicePickerState;
use crate::settings_ui::{EditState, SettingsUi};
use crate::settings_ui_draw::{
    centered_panel, content_rect, draw_rule_with_badge, format_scroll_badge, to_u16,
};
use crate::theme;
use crate::wrap;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

impl SettingsUi {
    pub(crate) fn draw_choice_picker(
        &self,
        picker: &ChoicePickerState,
        area: Rect,
        buf: &mut Buffer,
    ) -> Option<Position> {
        let filtered = picker.filtered_choices();
        let inspector_h: u16 = if area.height >= 12 { 3 } else { 2 };
        let panel = centered_panel(area, filtered.len().max(4), inspector_h);
        self.frame_block(Some(picker)).render(panel, buf);
        let content = content_rect(panel)?;

        draw_picker_header(picker, content.x, content.y, content.width, buf);
        let filter_cursor = draw_picker_filter(
            picker,
            content.x,
            content.y + 1,
            content.width,
            filtered.len(),
            buf,
        );

        let actual_insp_h = inspector_h.min(content.height.saturating_sub(4));
        let list_y = content.y + 3;
        let list_h = content.height.saturating_sub(3 + actual_insp_h);
        let visible_h = usize::from(list_h);
        let top = self.sync_choice_scroll_top(filtered.len(), picker.selected, visible_h);
        let badge = format_scroll_badge(top, visible_h, filtered.len());
        draw_rule_with_badge(
            content.x,
            content.y + 2,
            content.width,
            badge.as_deref(),
            buf,
        );

        let list_area = Rect::new(content.x, list_y, content.width, list_h);
        draw_picker_choices(picker, &filtered, top, list_area, buf);

        let insp_area = Rect::new(content.x, list_y + list_h, content.width, actual_insp_h);
        draw_picker_footer(picker, insp_area, buf);
        Some(filter_cursor)
    }
}

fn draw_picker_header(picker: &ChoicePickerState, x: u16, y: u16, width: u16, buf: &mut Buffer) {
    let w = usize::from(width);
    let left = format!("◂ {}", picker.title);
    let right = if picker.preview_theme {
        format!("live preview · {} choices", picker.choices.len())
    } else {
        format!("{} choices", picker.choices.len())
    };
    let right_w = wrap::str_width(&right);
    let left_disp = wrap::truncate(&left, w.saturating_sub(right_w + 2)).into_owned();
    let left_w = wrap::str_width(&left_disp);
    let pad = w.saturating_sub(left_w + right_w);
    let right_style = if picker.preview_theme {
        theme::fg(theme::GOLD)
    } else {
        theme::dim()
    };
    let line = Line::from(vec![
        Span::styled(left_disp, theme::accent().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, right_style),
    ]);
    buf.set_line(x, y, &line, width);
}

fn draw_picker_filter(
    picker: &ChoicePickerState,
    x: u16,
    y: u16,
    width: u16,
    shown: usize,
    buf: &mut Buffer,
) -> Position {
    let w = usize::from(width);
    let count_text = format!("{shown}/{}", picker.choices.len());
    let count_w = wrap::str_width(&count_text);
    let prompt = "❯ ";
    let prompt_w = wrap::str_width(prompt);
    let query_max = w.saturating_sub(prompt_w + count_w + 2);
    let query_disp = wrap::truncate(&picker.query, query_max).into_owned();
    let query_w = wrap::str_width(&query_disp);
    let (mid, used_mid) = if picker.query.is_empty() {
        let ph = wrap::truncate("filter choices…", query_max).into_owned();
        let pw = wrap::str_width(&ph);
        (Span::styled(ph, theme::dim()), pw)
    } else {
        (Span::styled(query_disp, theme::fg(theme::TEXT)), query_w)
    };
    let pad = w.saturating_sub(prompt_w + used_mid + count_w);
    let line = Line::from(vec![
        Span::styled(prompt, theme::accent().add_modifier(Modifier::BOLD)),
        mid,
        Span::raw(" ".repeat(pad)),
        Span::styled(count_text, theme::dim()),
    ]);
    buf.set_line(x, y, &line, width);
    let offset = prompt_w + if picker.query.is_empty() { 0 } else { query_w };
    Position::new(x + to_u16(offset.min(w.saturating_sub(1))), y)
}

fn draw_picker_choices(
    picker: &ChoicePickerState,
    filtered: &[&str],
    first: usize,
    area: Rect,
    buf: &mut Buffer,
) {
    let visible_h = usize::from(area.height);
    if visible_h == 0 {
        return;
    }
    if filtered.is_empty() {
        let line = Line::from(Span::styled("  no choices match the filter", theme::dim()));
        buf.set_line(area.x, area.y, &line, area.width);
        return;
    }
    let w = usize::from(area.width);
    for (offset, choice) in filtered.iter().skip(first).take(visible_h).enumerate() {
        let abs = first + offset;
        let y = area.y + to_u16(offset);
        let is_sel = abs == picker.selected;
        let is_cur = choice.eq_ignore_ascii_case(&picker.current);
        let is_def = choice.eq_ignore_ascii_case(&picker.default_value);
        let mark = if is_sel { "▸ " } else { "  " };
        let bullet = if is_cur { "● " } else { "  " };
        let badge = match (is_cur, is_def) {
            (true, true) => "current · default",
            (true, false) => "current",
            (false, true) => "default",
            (false, false) => "",
        };
        let badge_w = if badge.is_empty() {
            0
        } else {
            2 + wrap::str_width(badge)
        };
        let choice_disp = wrap::truncate(choice, w.saturating_sub(4 + badge_w)).into_owned();
        let choice_w = wrap::str_width(&choice_disp);
        let pad = w.saturating_sub(4 + choice_w + badge_w);
        let name_style = if is_sel {
            theme::focus().add_modifier(Modifier::BOLD)
        } else if is_cur {
            theme::accent().add_modifier(Modifier::BOLD)
        } else {
            theme::fg(theme::TEXT)
        };
        let bullet_style = if is_cur {
            theme::fg(theme::SUCCESS).add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        };
        let mut spans = vec![
            Span::styled(mark, theme::focus()),
            Span::styled(bullet, bullet_style),
            Span::styled(choice_disp, name_style),
            Span::raw(" ".repeat(pad)),
        ];
        if !badge.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(badge, theme::dim()));
        }
        if is_sel {
            for span in &mut spans {
                span.style = span.style.bg(theme::SELECTED_BG);
            }
        }
        buf.set_line(area.x, y, &Line::from(spans), area.width);
    }
}

fn draw_picker_footer(picker: &ChoicePickerState, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    draw_rule_with_badge(area.x, area.y, area.width, None, buf);
    let w = usize::from(area.width);
    if area.height >= 2 {
        let desc = Line::from(Span::styled(
            wrap::truncate(&wrap::sanitize(&picker.description), w).into_owned(),
            theme::fg(theme::TEXT),
        ));
        buf.set_line(area.x, area.y + 1, &desc, area.width);
    }
    if area.height >= 3 {
        let meta = if picker.preview_theme {
            format!(
                "{} · arrow keys preview live · esc restores previous theme",
                picker.key
            )
        } else {
            format!("{} · default: {}", picker.key, picker.default_value)
        };
        let line = Line::from(Span::styled(
            wrap::truncate(&meta, w).into_owned(),
            theme::dim(),
        ));
        buf.set_line(area.x, area.y + 2, &line, area.width);
    }
}

pub(crate) fn styled_value_span(item: &SettingItem, is_sel: bool, max_w: usize) -> Span<'static> {
    if item.secret {
        return Span::styled(
            wrap::truncate(&item.value, max_w).into_owned(),
            theme::dim(),
        );
    }
    match item.kind {
        SettingKind::Bool if item.value.eq_ignore_ascii_case("true") => Span::styled(
            "● on",
            theme::fg(theme::SUCCESS).add_modifier(Modifier::BOLD),
        ),
        SettingKind::Bool => Span::styled("○ off", theme::dim()),
        SettingKind::Enum if !item.values.is_empty() => {
            let text = if is_sel {
                format!("◂ {} ▸", item.value)
            } else {
                format!("{} ▸", item.value)
            };
            let style = if is_sel {
                theme::accent().add_modifier(Modifier::BOLD)
            } else {
                theme::fg(theme::TEXT)
            };
            Span::styled(wrap::truncate(&text, max_w).into_owned(), style)
        }
        _ => {
            let clean = wrap::sanitize(&item.value);
            let style = if matches!(item.value.as_str(), "(unset)" | "[]" | "{}" | "") {
                theme::dim()
            } else if is_sel {
                theme::fg(theme::TEXT)
            } else {
                theme::muted()
            };
            Span::styled(wrap::truncate(&clean, max_w).into_owned(), style)
        }
    }
}

pub(crate) fn build_inspector_meta(item: &SettingItem, max_w: usize) -> Line<'static> {
    let mut spans = vec![
        Span::styled(item.key.clone(), theme::dim()),
        Span::styled(" · default: ", theme::dim()),
        Span::styled(
            wrap::truncate(&wrap::sanitize(&item.default), 24).into_owned(),
            theme::muted(),
        ),
    ];
    if item.modified || !item.source.eq_ignore_ascii_case("default") {
        let style = if item.modified {
            theme::fg(theme::GOLD)
        } else {
            theme::dim()
        };
        spans.push(Span::styled(" · ", theme::dim()));
        spans.push(Span::styled(item.source.clone(), style));
    }
    if let Some(env) = &item.env {
        let used: usize = spans.iter().map(Span::width).sum();
        if used + 3 + env.len() <= max_w {
            spans.push(Span::styled(" · ", theme::dim()));
            spans.push(Span::styled(env.clone(), theme::inline_code()));
        }
    }
    Line::from(spans)
}

pub(crate) fn draw_edit_bar(
    edit: &EditState,
    x: u16,
    y: u16,
    width: u16,
    buf: &mut Buffer,
) -> Position {
    let w = usize::from(width);
    let prefix = format!("Edit {} ({}): ", edit.label, edit.kind.label());
    let prefix_w = wrap::str_width(&prefix);
    let draft_max = w.saturating_sub(prefix_w + 1);
    let draft_disp = wrap::truncate(&edit.draft, draft_max).into_owned();
    let draft_w = wrap::str_width(&draft_disp);
    let line = Line::from(vec![
        Span::styled(prefix, theme::accent().add_modifier(Modifier::BOLD)),
        Span::styled(draft_disp, theme::fg(theme::TEXT)),
    ]);
    buf.set_line(x, y, &line, width);
    let cx = x + to_u16((prefix_w + draft_w).min(w.saturating_sub(1)));
    Position::new(cx, y)
}

pub(crate) fn build_footer_hint(item: &SettingItem, max_w: usize) -> Line<'static> {
    let hint = match item.kind {
        SettingKind::Bool => "enter / ←→ toggle · del reset to default".to_owned(),
        SettingKind::Enum if item.values.len() >= 2 => {
            format!(
                "enter pick from {} choices · ←/→ cycle · del reset",
                item.values.len()
            )
        }
        SettingKind::Array => "enter edit comma-separated values · del reset to default".to_owned(),
        SettingKind::Table => "enter edit inline TOML table · del reset to default".to_owned(),
        _ => "enter edit value · del reset to default".to_owned(),
    };
    Line::from(Span::styled(
        wrap::truncate(&hint, max_w).into_owned(),
        theme::dim(),
    ))
}
