//! Rendering helpers for the `/login` modal (API-key form and in-TUI OAuth / device-code flow).

use crate::login::FormField;
use crate::port::LoginProviderChoice;
use crate::theme;
use crate::wrap;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Widget};

/// Strip ASCII/Unicode whitespace from pasted OAuth callback URLs or `code#state` strings so
/// terminal line-wrapping never corrupts long redirect URLs.
pub(crate) fn sanitize_oauth_input(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Render the `/login` provider picker modal.
pub(crate) fn draw_picker(
    area: Rect,
    buf: &mut Buffer,
    providers: &[LoginProviderChoice],
    selected: usize,
) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(Span::styled(" Login · Configure provider ", theme::bold()))
        .title_bottom(Line::from(vec![
            Span::styled(" ↑↓", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" move · ", theme::muted()),
            Span::styled("enter", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" login · ", theme::muted()),
            Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" back ", theme::muted()),
        ]));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 4 {
        return;
    }
    let intro = Line::from(Span::styled(
        "Choose a subscription (in-TUI OAuth/device code) or API-key provider:",
        theme::muted(),
    ));
    buf.set_line(inner.x + 1, inner.y, &intro, inner.width.saturating_sub(2));

    let rows = usize::from(inner.height.saturating_sub(3));
    let first = (selected + 1).saturating_sub(rows);
    let width = usize::from(inner.width.saturating_sub(2));

    for (i, p) in providers.iter().enumerate().skip(first).take(rows) {
        let is_sel = i == selected;
        let status_span = if p.configured {
            Span::styled("● active  ", theme::fg(theme::SUCCESS))
        } else {
            Span::styled("○ not set ", theme::dim())
        };
        let name_text = format!("{:<26}", p.name);
        let room = width.saturating_sub(14 + name_text.len());
        let hint = wrap::truncate(&p.hint, room).into_owned();
        let row_style = if is_sel {
            theme::focus().add_modifier(Modifier::BOLD)
        } else {
            theme::plain()
        };
        let line = Line::from(vec![
            Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
            status_span,
            Span::styled(name_text, row_style),
            Span::raw("  "),
            Span::styled(hint, theme::muted()),
        ]);
        let y = inner.y + 2 + u16::try_from(i - first).unwrap_or(0);
        if is_sel {
            buf.set_style(Rect::new(inner.x, y, inner.width, 1), theme::surface());
        }
        buf.set_line(inner.x + 1, y, &line, inner.width.saturating_sub(2));
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "form renderer takes area, buffer, provider metadata, fields, focus, secret toggle, status, error"
)]
pub(crate) fn draw_form(
    area: Rect,
    buf: &mut Buffer,
    provider: &LoginProviderChoice,
    fields: &[FormField],
    focused: usize,
    reveal_secret: bool,
    validating: Option<&str>,
    error: Option<&str>,
) -> Option<Position> {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .title(Span::styled(
            format!(" Login · {} ", provider.name),
            theme::bold(),
        ))
        .title_bottom(Line::from(vec![
            Span::styled(" enter", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" validate & save · ", theme::muted()),
            Span::styled("alt+v", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" paste · ", theme::muted()),
            Span::styled("ctrl+r", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" show/hide · ", theme::muted()),
            Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" back ", theme::muted()),
        ]));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 5 {
        return None;
    }
    let hint = Line::from(Span::styled(&provider.hint, theme::muted()));
    buf.set_line(inner.x + 1, inner.y, &hint, inner.width.saturating_sub(2));

    let mut y = inner.y + 2;
    let mut cursor_pos = None;
    let field_width = usize::from(inner.width.saturating_sub(6));
    for (idx, field) in fields.iter().enumerate() {
        if y + 2 > inner.bottom() {
            break;
        }
        if let Some(pos) = draw_form_field(
            inner,
            buf,
            y,
            field.label,
            field,
            idx == focused && validating.is_none(),
            reveal_secret,
            field_width,
        ) {
            cursor_pos = Some(pos);
        }
        y += 3;
    }

    if let Some(status) = validating
        && y < inner.bottom()
    {
        buf.set_line(
            inner.x + 1,
            y,
            &Line::from(Span::styled(format!("◐ {status}"), theme::accent())),
            inner.width.saturating_sub(2),
        );
    } else if let Some(err) = error
        && y < inner.bottom()
    {
        buf.set_line(
            inner.x + 1,
            y,
            &Line::from(Span::styled(format!("! {err}"), theme::fg(theme::ERROR))),
            inner.width.saturating_sub(2),
        );
    }
    cursor_pos
}

/// Render the in-TUI OAuth PKCE / RFC 8628 device-code flow screen.
#[expect(
    clippy::too_many_arguments,
    reason = "oauth flow renderer takes area, buffer, label, url, user code, message, input flag/value, error"
)]
pub(crate) fn draw_oauth_flow(
    area: Rect,
    buf: &mut Buffer,
    label: &str,
    url: Option<&str>,
    user_code: Option<&str>,
    message: &str,
    needs_code: bool,
    input: &str,
    error: Option<&str>,
) -> Option<Position> {
    let show_input = needs_code || (url.is_some() && user_code.is_none());
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .title(Span::styled(format!(" Login · {label} "), theme::bold()))
        .title_bottom(oauth_footer(show_input));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 5 {
        return None;
    }
    let width = usize::from(inner.width.saturating_sub(2));
    let mut y = inner.y;

    buf.set_line(
        inner.x + 1,
        y,
        &Line::from(Span::styled(
            wrap::truncate(message, width).into_owned(),
            theme::muted(),
        )),
        inner.width.saturating_sub(2),
    );
    y += 2;

    if let Some(code) = user_code
        && y < inner.bottom()
    {
        let line = Line::from(vec![
            Span::styled("Code: ", theme::muted()),
            Span::styled(code.to_owned(), theme::focus().add_modifier(Modifier::BOLD)),
        ]);
        buf.set_line(inner.x + 1, y, &line, inner.width.saturating_sub(2));
        y += 2;
    }

    if let Some(auth_url) = url
        && y < inner.bottom()
    {
        y = draw_oauth_url(inner, buf, y, auth_url, width, show_input);
    }

    let mut cursor = None;
    if show_input && y + 1 < inner.bottom() {
        cursor = draw_oauth_input(inner, buf, y, input);
        y += 3;
    }

    if let Some(err) = error
        && y < inner.bottom()
    {
        buf.set_line(
            inner.x + 1,
            y,
            &Line::from(Span::styled(
                format!("! {}", wrap::truncate(err, width.saturating_sub(2))),
                theme::fg(theme::ERROR),
            )),
            inner.width.saturating_sub(2),
        );
    }
    cursor
}

fn oauth_footer(show_input: bool) -> Line<'static> {
    let mut footer = Vec::new();
    if show_input {
        footer.extend([
            Span::styled(" enter", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" submit code · ", theme::muted()),
            Span::styled(
                "ctrl+v / alt+v",
                theme::accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled(" paste · ", theme::muted()),
        ]);
    }
    footer.extend([
        Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
        Span::styled(" cancel ", theme::muted()),
    ]);
    Line::from(footer)
}

fn draw_oauth_url(
    inner: Rect,
    buf: &mut Buffer,
    mut y: u16,
    auth_url: &str,
    width: usize,
    show_input: bool,
) -> u16 {
    buf.set_line(
        inner.x + 1,
        y,
        &Line::from(Span::styled("Authorization URL:", theme::muted())),
        inner.width.saturating_sub(2),
    );
    y += 1;
    let limit = inner
        .bottom()
        .saturating_sub(if show_input { 4 } else { 1 });
    for chunk in wrap_plain(auth_url, width.max(12)) {
        if y >= limit {
            break;
        }
        buf.set_line(
            inner.x + 1,
            y,
            &Line::from(Span::styled(chunk, theme::link())),
            inner.width.saturating_sub(2),
        );
        y += 1;
    }
    y + 1
}

fn draw_oauth_input(inner: Rect, buf: &mut Buffer, y: u16, input: &str) -> Option<Position> {
    let label_text = if input.is_empty() {
        "Paste redirect URL or authorization code".to_owned()
    } else {
        format!(
            "Paste redirect URL or authorization code ({} chars)",
            input.chars().count()
        )
    };
    let field = FormField {
        label: "Paste redirect URL or authorization code",
        placeholder: "http://localhost:.../callback?code=... (or code#state)".to_owned(),
        value: input.to_owned(),
        secret: false,
        required: true,
    };
    let field_width = usize::from(inner.width.saturating_sub(6));
    draw_form_field(inner, buf, y, &label_text, &field, true, true, field_width)
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in text.chars() {
        let w = wrap::char_width(ch).max(1);
        if cur_w + w > width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += w;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

#[expect(
    clippy::too_many_arguments,
    reason = "field renderer takes area, buffer, row, label, field metadata, focus, reveal flag, and width"
)]
fn draw_form_field(
    inner: Rect,
    buf: &mut Buffer,
    y: u16,
    label: &str,
    field: &FormField,
    is_focus: bool,
    reveal_secret: bool,
    field_width: usize,
) -> Option<Position> {
    let label_style = if is_focus {
        theme::focus().add_modifier(Modifier::BOLD)
    } else {
        theme::muted()
    };
    buf.set_line(
        inner.x + 1,
        y,
        &Line::from(Span::styled(label, label_style)),
        inner.width.saturating_sub(2),
    );
    let display_val = if field.value.is_empty() {
        None
    } else if field.secret && !reveal_secret {
        Some("•".repeat(field.value.chars().count()))
    } else {
        Some(field.value.clone())
    };
    let shown = display_val.as_deref().map_or_else(
        || Span::styled(field.placeholder.clone(), theme::dim()),
        |val| Span::styled(tail_fit(val, field_width), theme::plain()),
    );
    let row = Rect {
        x: inner.x + 1,
        y: y + 1,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    let cursor = if is_focus {
        buf.set_style(row, theme::surface());
        let cursor_offset = display_val
            .as_deref()
            .map_or(0, |v| wrap::str_width(&tail_fit(v, field_width)));
        let cx = row
            .x
            .saturating_add(2)
            .saturating_add(u16::try_from(cursor_offset).unwrap_or(0))
            .min(row.right().saturating_sub(1));
        Some(Position::new(cx, row.y))
    } else {
        None
    };
    let prefix_style = if is_focus {
        theme::focus()
    } else {
        theme::dim()
    };
    let line = Line::from(vec![
        Span::styled(if is_focus { "› " } else { "  " }, prefix_style),
        shown,
    ]);
    buf.set_line(row.x, row.y, &line, row.width);
    cursor
}

fn tail_fit(text: &str, max_width: usize) -> String {
    if wrap::str_width(text) <= max_width {
        return text.to_owned();
    }
    let budget = max_width.saturating_sub(1);
    let mut tail = Vec::new();
    let mut used = 0usize;
    for ch in text.chars().rev() {
        let w = wrap::char_width(ch);
        if used + w > budget {
            break;
        }
        used += w;
        tail.push(ch);
    }
    tail.reverse();
    let mut out = String::from('…');
    out.extend(tail);
    out
}
