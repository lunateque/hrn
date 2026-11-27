//! Rendering for [`crate::modelhub::ModelHub`]: tabbed header, live search bar, provider filter
//! chips, and layout orchestration delegating row/inspector painting to [`crate::modelhub_draw_rows`].

use crate::modelhub::{HubKind, HubTab, ModelHub, RoleRow, SubMode};
use crate::modelhub_draw_rows::{
    draw_model_inspector, draw_model_rows, draw_preset_inspector, draw_preset_rows,
    draw_role_inspector, draw_role_rows,
};
use crate::port::ModelInfo;
use crate::theme;
use crate::wrap;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

/// Number of scrollable list rows available inside `area` for the current hub state.
pub(crate) fn list_rows(hub: &ModelHub, area: Rect) -> usize {
    let Some(inner) = inner_padded(area) else {
        return 10;
    };
    let h = usize::from(inner.height);
    let chrome = header_rows(hub) + footer_rows(hub);
    h.saturating_sub(chrome).max(1)
}

/// Render [`ModelHub`] into `area`, returning the terminal cursor position inside active inputs.
pub(crate) fn draw(hub: &ModelHub, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let inner = inner_padded(area)?;
    let (title, hint) = frame_chrome(hub);
    Clear.render(area, buf);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(Span::styled(format!(" {title} "), theme::bold()))
        .title_bottom(hint);
    block.render(area, buf);

    let cursor = match &hub.sub {
        SubMode::SavePresetName { draft } => draw_save_preset(draft, inner, buf),
        SubMode::PickModelForRole {
            role,
            query,
            provider_idx,
            selected,
            top,
        } => draw_pick_for_role(hub, role, query, *provider_idx, *selected, *top, inner, buf),
        SubMode::Main => draw_main(hub, inner, buf),
    };
    Some(cursor)
}

fn inner_padded(area: Rect) -> Option<Rect> {
    if area.width < 20 || area.height < 8 {
        return None;
    }
    let block = Block::bordered().border_type(BorderType::Rounded);
    let raw = block.inner(area);
    let pad = u16::from(raw.width >= 24);
    Some(Rect::new(
        raw.x + pad,
        raw.y,
        raw.width.saturating_sub(pad * 2),
        raw.height,
    ))
}

fn header_rows(hub: &ModelHub) -> usize {
    match &hub.sub {
        SubMode::SavePresetName { .. } => 1,
        SubMode::PickModelForRole { .. } => {
            if hub.providers.len() > 1 {
                3
            } else {
                2
            }
        }
        SubMode::Main => {
            let tabs = usize::from(hub.kind == HubKind::RolesHub);
            let chips = usize::from(hub.uses_provider_filter());
            tabs + 1 + chips + 1 // tabs + search + chips + rule
        }
    }
}

fn footer_rows(hub: &ModelHub) -> usize {
    match &hub.sub {
        SubMode::SavePresetName { .. } => 0,
        SubMode::PickModelForRole { .. } | SubMode::Main => 3, // rule + 2 inspector lines
    }
}

fn frame_chrome(hub: &ModelHub) -> (String, Line<'static>) {
    match &hub.sub {
        SubMode::SavePresetName { .. } => (
            "save model preset".to_owned(),
            hint_line(&[("enter", "save"), ("esc", "cancel")]),
        ),
        SubMode::PickModelForRole { role, .. } => (
            format!("assign model -> {role}"),
            hint_line(&[("enter", "assign"), ("←/→", "provider"), ("esc", "back")]),
        ),
        SubMode::Main => match hub.kind {
            HubKind::TemporaryPicker => (
                "switch model (session-only)".to_owned(),
                hint_line(&[("enter", "switch"), ("←/→", "provider"), ("esc", "close")]),
            ),
            HubKind::PresetPicker => (
                "model presets".to_owned(),
                hint_line(&[("enter", "apply preset"), ("esc", "close")]),
            ),
            HubKind::RolesHub => {
                let title = if hub.session_only {
                    "models (session-only)".to_owned()
                } else {
                    "models & roles".to_owned()
                };
                let hint = match hub.tab {
                    HubTab::Models if hub.session_only => hint_line(&[
                        ("tab", "tabs"),
                        ("←/→", "provider"),
                        ("enter", "switch"),
                        ("esc", "close"),
                    ]),
                    HubTab::Models => hint_line(&[
                        ("tab", "tabs"),
                        ("←/→", "provider"),
                        ("enter", "set default"),
                        ("ctrl+t", "temp"),
                        ("esc", "close"),
                    ]),
                    HubTab::Roles => hint_line(&[
                        ("tab", "tabs"),
                        ("enter", "pick model"),
                        ("←/→", ":level"),
                        ("ctrl+s", "save preset"),
                        ("esc", "close"),
                    ]),
                    HubTab::Presets => hint_line(&[
                        ("tab", "tabs"),
                        ("enter", "apply"),
                        ("ctrl+s", "save preset"),
                        ("esc", "close"),
                    ]),
                };
                (title, hint)
            }
        },
    }
}

fn draw_save_preset(draft: &str, inner: Rect, buf: &mut Buffer) -> Position {
    let prompt = "Preset name: ";
    let line = Line::from(vec![
        Span::styled(prompt, theme::accent().add_modifier(Modifier::BOLD)),
        Span::styled(draft.to_owned(), theme::fg(theme::TEXT)),
    ]);
    buf.set_line(inner.x, inner.y + 1, &line, inner.width);
    let helper = Line::from(Span::styled(
        "Saves all current [roles] as a named [modelPresets.<name>] bundle.",
        theme::muted(),
    ));
    if inner.height >= 4 {
        buf.set_line(inner.x, inner.y + 3, &helper, inner.width);
    }
    let cx = inner.x
        + u16::try_from(wrap::str_width(prompt) + wrap::str_width(draft))
            .unwrap_or(0)
            .min(inner.width.saturating_sub(1));
    Position::new(cx, inner.y + 1)
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal helper rendering PickModelForRole sub-view"
)]
fn draw_pick_for_role(
    hub: &ModelHub,
    role: &str,
    query: &str,
    provider_idx: usize,
    selected: usize,
    top: usize,
    inner: Rect,
    buf: &mut Buffer,
) -> Position {
    let models = hub.filtered_models(query, provider_idx);
    let mut y = inner.y;
    let cursor = draw_search_bar(
        y,
        inner,
        buf,
        query,
        &format!("search model for `{role}`…"),
        models.len(),
        hub.models.len(),
    );
    y += 1;
    if hub.providers.len() > 1 {
        draw_provider_chips(y, inner, buf, &hub.providers, provider_idx);
        y += 1;
    }
    draw_rule(y, inner, buf);
    y += 1;

    let current_target = hub
        .roles
        .iter()
        .find(|r| r.role == role)
        .map(|r| hrn_kernel::split_level_suffix(&r.target).0);
    let list_h = usize::from(inner.bottom().saturating_sub(y).saturating_sub(3));
    draw_model_rows(
        &models,
        current_target.unwrap_or(&hub.current_model),
        selected,
        top,
        y,
        list_h,
        inner,
        buf,
    );
    let insp_y = inner.bottom().saturating_sub(3);
    draw_model_inspector(models.get(selected).copied(), None, insp_y, inner, buf);
    cursor
}

fn draw_main(hub: &ModelHub, inner: Rect, buf: &mut Buffer) -> Position {
    let mut y = inner.y;
    if hub.kind == HubKind::RolesHub {
        draw_tabs(hub, y, inner, buf);
        y += 1;
    }
    let (shown, total, placeholder) = match (hub.kind, hub.tab) {
        (HubKind::TemporaryPicker, _) | (HubKind::RolesHub, HubTab::Models) => (
            hub.filtered_models(&hub.query, hub.provider_idx).len(),
            hub.models.len(),
            "type to search models (name, provider, role, 200k)…",
        ),
        (HubKind::RolesHub, HubTab::Roles) => (
            hub.filtered_role_rows()
                .iter()
                .filter(|r| r.is_selectable())
                .count(),
            hub.roles.len(),
            "type to filter roles or targets…",
        ),
        (HubKind::PresetPicker, _) | (HubKind::RolesHub, HubTab::Presets) => (
            hub.filtered_presets().len(),
            hub.presets.len(),
            "type to filter presets…",
        ),
    };
    let cursor = draw_search_bar(y, inner, buf, &hub.query, placeholder, shown, total);
    y += 1;
    if hub.uses_provider_filter() {
        draw_provider_chips(y, inner, buf, &hub.providers, hub.provider_idx);
        y += 1;
    }
    draw_rule(y, inner, buf);
    y += 1;

    let list_h = usize::from(inner.bottom().saturating_sub(y).saturating_sub(3));
    let insp_y = inner.bottom().saturating_sub(3);

    match (hub.kind, hub.tab) {
        (HubKind::TemporaryPicker, _) | (HubKind::RolesHub, HubTab::Models) => {
            let models = hub.filtered_models(&hub.query, hub.provider_idx);
            draw_model_rows(
                &models,
                &hub.current_model,
                hub.selected,
                hub.top,
                y,
                list_h,
                inner,
                buf,
            );
            draw_model_inspector(
                models.get(hub.selected).copied(),
                hub.status.as_ref(),
                insp_y,
                inner,
                buf,
            );
        }
        (HubKind::RolesHub, HubTab::Roles) => {
            let rows = hub.filtered_role_rows();
            draw_role_rows(hub, &rows, y, list_h, inner, buf);
            let sel_role = match rows.get(hub.selected) {
                Some(RoleRow::Item(r)) => Some(r),
                _ => None,
            };
            draw_role_inspector(hub, sel_role, insp_y, inner, buf);
        }
        (HubKind::PresetPicker, _) | (HubKind::RolesHub, HubTab::Presets) => {
            let presets = hub.filtered_presets();
            draw_preset_rows(&presets, hub.selected, hub.top, y, list_h, inner, buf);
            draw_preset_inspector(
                presets.get(hub.selected).copied(),
                hub.status.as_ref(),
                insp_y,
                inner,
                buf,
            );
        }
    }
    cursor
}

fn draw_tabs(hub: &ModelHub, y: u16, inner: Rect, buf: &mut Buffer) {
    let tabs = [
        (HubTab::Models, format!(" Models ({}) ", hub.models.len())),
        (HubTab::Roles, format!(" Roles ({}) ", hub.roles.len())),
        (
            HubTab::Presets,
            format!(" Presets ({}) ", hub.presets.len()),
        ),
    ];
    let mut spans = Vec::new();
    for (idx, (tab, label)) in tabs.into_iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if hub.tab == tab {
            theme::focus()
                .bg(theme::SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::muted()
        };
        spans.push(Span::styled(label, style));
    }
    let active_short = hub.find_model(&hub.current_model).map_or_else(
        || ModelInfo::short_name_from_alias(&hub.current_model),
        |m| m.name.clone(),
    );
    let badge = format!("● {active_short} · {}", hub.current_thinking.as_str());
    let used: usize = spans.iter().map(Span::width).sum();
    let badge_w = wrap::str_width(&badge);
    let w = usize::from(inner.width);
    if w > used + badge_w + 2 {
        spans.push(Span::raw(" ".repeat(w - used - badge_w)));
        spans.push(Span::styled(badge, theme::fg(theme::SUCCESS)));
    }
    buf.set_line(inner.x, y, &Line::from(spans), inner.width);
}

fn draw_search_bar(
    y: u16,
    inner: Rect,
    buf: &mut Buffer,
    query: &str,
    placeholder: &str,
    shown: usize,
    total: usize,
) -> Position {
    let count = format!("{shown}/{total}");
    let w = usize::from(inner.width);
    let count_w = wrap::str_width(&count);
    let prompt = "❯ ";
    let max_q = w.saturating_sub(2 + count_w + 2);
    let q_disp = if query.is_empty() {
        wrap::truncate(placeholder, max_q).into_owned()
    } else {
        wrap::truncate(query, max_q).into_owned()
    };
    let q_style = if query.is_empty() {
        theme::dim()
    } else {
        theme::fg(theme::TEXT)
    };
    let mut spans = vec![
        Span::styled(prompt, theme::accent().add_modifier(Modifier::BOLD)),
        Span::styled(q_disp.clone(), q_style),
    ];
    let used: usize = spans.iter().map(Span::width).sum();
    if w > used + count_w {
        spans.push(Span::raw(" ".repeat(w - used - count_w)));
        spans.push(Span::styled(count, theme::dim()));
    }
    buf.set_line(inner.x, y, &Line::from(spans), inner.width);
    let cursor_off = if query.is_empty() {
        2
    } else {
        2 + wrap::str_width(&q_disp)
    };
    let cx = inner.x
        + u16::try_from(cursor_off)
            .unwrap_or(0)
            .min(inner.width.saturating_sub(1));
    Position::new(cx, y)
}

fn draw_provider_chips(y: u16, inner: Rect, buf: &mut Buffer, providers: &[String], active: usize) {
    let w = usize::from(inner.width);
    let mut spans = vec![Span::styled("provider ", theme::dim())];
    let mut used = 9usize;
    for (idx, name) in providers.iter().enumerate() {
        let chip = format!(" {name} ");
        let cw = wrap::str_width(&chip);
        if used + cw + 1 > w {
            if used + 2 <= w {
                spans.push(Span::styled(" …", theme::dim()));
            }
            break;
        }
        let style = if idx == active {
            theme::accent()
                .bg(theme::SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::muted()
        };
        spans.push(Span::styled(chip, style));
        spans.push(Span::raw(" "));
        used += cw + 1;
    }
    buf.set_line(inner.x, y, &Line::from(spans), inner.width);
}

pub(crate) fn draw_rule(y: u16, inner: Rect, buf: &mut Buffer) {
    let line = Line::from(Span::styled(
        "─".repeat(usize::from(inner.width)),
        theme::border(),
    ));
    buf.set_line(inner.x, y, &line, inner.width);
}

pub(crate) fn paint_row(
    x: u16,
    y: u16,
    width: u16,
    mut spans: Vec<Span<'static>>,
    selected: bool,
    buf: &mut Buffer,
) {
    if selected {
        let total: usize = spans.iter().map(Span::width).sum();
        let w = usize::from(width);
        if w > total {
            spans.push(Span::raw(" ".repeat(w - total)));
        }
        for span in &mut spans {
            span.style = span.style.bg(theme::SELECTED_BG);
        }
    }
    buf.set_line(x, y, &Line::from(spans), width);
}

fn hint_line(pairs: &[(&'static str, &'static str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (idx, (key, desc)) in pairs.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        spans.push(Span::styled(
            *key,
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {desc}"), theme::muted()));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}
