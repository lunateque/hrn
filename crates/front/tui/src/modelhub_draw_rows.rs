//! Row tables and bottom inspector panes for [`crate::modelhub_draw`].

use crate::modelhub::{ModelHub, RoleRow};
use crate::modelhub_draw::{draw_rule, paint_row};
use crate::modelhub_ops::format_ctx_short;
use crate::port::{ModelInfo, RoleAssignment};
use crate::theme;
use crate::wrap;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

#[expect(
    clippy::too_many_arguments,
    reason = "renders paginated model table with aligned columns"
)]
pub(crate) fn draw_model_rows(
    models: &[&ModelInfo],
    active_alias: &str,
    selected: usize,
    top: usize,
    y_start: u16,
    rows: usize,
    inner: Rect,
    buf: &mut Buffer,
) {
    let w = usize::from(inner.width);
    if models.is_empty() {
        let line = Line::from(Span::styled("  No matching models", theme::dim()));
        buf.set_line(inner.x, y_start, &line, inner.width);
        return;
    }
    let prov_w = models
        .iter()
        .map(|m| wrap::str_width(&m.provider))
        .max()
        .unwrap_or(8)
        .clamp(6, 18);
    let name_w = models
        .iter()
        .map(|m| wrap::str_width(&m.name))
        .max()
        .unwrap_or(16)
        .clamp(14, w.saturating_sub(prov_w + 20).clamp(14, 36));

    for (off, model) in models.iter().skip(top).take(rows).enumerate() {
        let abs = top + off;
        let is_sel = abs == selected;
        let is_active = model.alias == active_alias;
        let mark = if is_sel { "▸ " } else { "  " };
        let dot = if is_active { "● " } else { "  " };
        let name_col = format!("{:<name_w$}", wrap::truncate(&model.name, name_w));
        let prov_col = format!("{:<prov_w$}", wrap::truncate(&model.provider, prov_w));
        let ctx_raw = format_ctx_short(model.context_window);
        let ctx_col = if ctx_raw.is_empty() {
            "    —".to_owned()
        } else {
            format!("{ctx_raw:>5}")
        };
        let reason_col = if model.reasoning { " ✦" } else { "  " };

        let name_style = if is_sel {
            theme::focus().add_modifier(Modifier::BOLD)
        } else if is_active {
            theme::fg(theme::TEXT).add_modifier(Modifier::BOLD)
        } else {
            theme::fg(theme::TEXT)
        };
        let mut spans = vec![
            Span::styled(mark, theme::focus().add_modifier(Modifier::BOLD)),
            Span::styled(dot, theme::fg(theme::SUCCESS).add_modifier(Modifier::BOLD)),
            Span::styled(name_col, name_style),
            Span::raw("  "),
            Span::styled(prov_col, theme::muted()),
            Span::raw("  "),
            Span::styled(ctx_col, theme::dim()),
            Span::styled(reason_col, theme::fg(theme::GOLD)),
        ];
        if !model.roles.is_empty() {
            let used: usize = spans.iter().map(Span::width).sum();
            let room = w.saturating_sub(used + 2);
            if room >= 5 {
                let badges = format_role_badges(&model.roles, room);
                spans.push(Span::raw("  "));
                spans.push(Span::styled(badges, theme::accent()));
            }
        }
        paint_row(
            inner.x,
            y_start + u16::try_from(off).unwrap_or(0),
            inner.width,
            spans,
            is_sel,
            buf,
        );
    }
}

pub(crate) fn draw_role_rows(
    hub: &ModelHub,
    rows: &[RoleRow],
    y_start: u16,
    height: usize,
    inner: Rect,
    buf: &mut Buffer,
) {
    let w = usize::from(inner.width);
    if rows.is_empty() {
        let line = Line::from(Span::styled("  No matching roles", theme::dim()));
        buf.set_line(inner.x, y_start, &line, inner.width);
        return;
    }
    let role_w = rows
        .iter()
        .filter_map(|r| match r {
            RoleRow::Item(a) => Some(wrap::str_width(&a.role)),
            RoleRow::Header(_) => None,
        })
        .max()
        .unwrap_or(10)
        .clamp(8, 16);

    for (off, row) in rows.iter().skip(hub.top).take(height).enumerate() {
        let abs = hub.top + off;
        let is_sel = abs == hub.selected;
        let y = y_start + u16::try_from(off).unwrap_or(0);
        match row {
            RoleRow::Header(label) => {
                let line = Line::from(vec![
                    Span::styled(
                        format!("  {label} "),
                        theme::accent().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "─".repeat(w.saturating_sub(wrap::str_width(label) + 3)),
                        theme::border(),
                    ),
                ]);
                buf.set_line(inner.x, y, &line, inner.width);
            }
            RoleRow::Item(assign) => {
                let (base, suffix) = hrn_kernel::split_level_suffix(assign.target.trim());
                let (short, prov) = match hub.find_model(base) {
                    Some(m) => (m.name.clone(), m.provider.clone()),
                    None => (
                        ModelInfo::short_name_from_alias(base),
                        ModelInfo::provider_from_alias(base),
                    ),
                };
                let level_tag = match suffix {
                    hrn_kernel::LevelSuffix::None => String::new(),
                    hrn_kernel::LevelSuffix::Inherit => ":inherit".to_owned(),
                    hrn_kernel::LevelSuffix::Level(lvl) => format!(":{}", lvl.as_str()),
                };
                let role_pad = format!("{:<role_w$}", wrap::truncate(&assign.role, role_w));
                let role_style = if is_sel {
                    theme::focus().add_modifier(Modifier::BOLD)
                } else if assign.is_standard {
                    theme::accent()
                } else {
                    theme::fg(theme::TEXT)
                };
                let avail_model = w.saturating_sub(role_w + 10 + level_tag.len() + prov.len());
                let model_disp = wrap::truncate(&short, avail_model.max(12)).into_owned();
                let mut spans = vec![
                    Span::styled(
                        if is_sel { "▸ " } else { "  " },
                        theme::focus().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(role_pad, role_style),
                    Span::styled(" → ", theme::dim()),
                    Span::styled(model_disp, theme::fg(theme::TEXT)),
                ];
                if !level_tag.is_empty() {
                    spans.push(Span::styled(level_tag, theme::fg(theme::GOLD)));
                }
                let used: usize = spans.iter().map(Span::width).sum();
                if w > used + prov.len() + 2 {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(prov, theme::muted()));
                }
                paint_row(inner.x, y, inner.width, spans, is_sel, buf);
            }
        }
    }
}

pub(crate) fn draw_preset_rows(
    presets: &[&str],
    selected: usize,
    top: usize,
    y_start: u16,
    height: usize,
    inner: Rect,
    buf: &mut Buffer,
) {
    if presets.is_empty() {
        let line = Line::from(Span::styled(
            "  No saved presets · press Ctrl+S to save current roles as a preset",
            theme::dim(),
        ));
        buf.set_line(inner.x, y_start, &line, inner.width);
        return;
    }
    let w = usize::from(inner.width);
    for (off, name) in presets.iter().skip(top).take(height).enumerate() {
        let abs = top + off;
        let is_sel = abs == selected;
        let style = if is_sel {
            theme::focus().add_modifier(Modifier::BOLD)
        } else {
            theme::fg(theme::TEXT)
        };
        let spans = vec![
            Span::styled(
                if is_sel { "▸ " } else { "  " },
                theme::focus().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                wrap::truncate(name, w.saturating_sub(4)).into_owned(),
                style,
            ),
        ];
        paint_row(
            inner.x,
            y_start + u16::try_from(off).unwrap_or(0),
            inner.width,
            spans,
            is_sel,
            buf,
        );
    }
}

pub(crate) fn draw_model_inspector(
    model: Option<&ModelInfo>,
    status: Option<&(String, bool)>,
    y: u16,
    inner: Rect,
    buf: &mut Buffer,
) {
    draw_rule(y, inner, buf);
    let w = usize::from(inner.width);
    let Some(m) = model else {
        if let Some((msg, is_err)) = status {
            draw_status_line(y + 1, inner, buf, msg, *is_err);
        }
        return;
    };
    let line1 = if m.id != m.alias && !m.alias.ends_with(&m.id) {
        format!("{}  (id: {})", m.alias, m.id)
    } else {
        m.alias.clone()
    };
    buf.set_line(
        inner.x,
        y + 1,
        &Line::from(Span::styled(
            wrap::truncate(&line1, w).into_owned(),
            theme::fg(theme::TEXT),
        )),
        inner.width,
    );
    if let Some((msg, is_err)) = status {
        draw_status_line(y + 2, inner, buf, msg, *is_err);
        return;
    }
    let mut parts = Vec::new();
    let ctx = format_ctx_short(m.context_window);
    if !ctx.is_empty() {
        if let Some(ext) = m.max_context_window.filter(|&e| e > m.context_window) {
            parts.push(format!("ctx {ctx} (ext {})", format_ctx_short(ext)));
        } else {
            parts.push(format!("ctx {ctx}"));
        }
    }
    if m.reasoning {
        let lvl = m
            .thinking
            .map(|t| format!(" (default {})", t.as_str()))
            .unwrap_or_default();
        parts.push(format!("✦ reasoning{lvl}"));
    }
    if let Some(p) = m.price {
        parts.push(format!("${:.2}/${:.2} per 1M", p.input, p.output));
    }
    if !m.roles.is_empty() {
        parts.push(format!("roles: {}", m.roles.join(", ")));
    }
    let meta = if parts.is_empty() {
        format!("provider: {}", m.provider)
    } else {
        parts.join(" · ")
    };
    buf.set_line(
        inner.x,
        y + 2,
        &Line::from(Span::styled(
            wrap::truncate(&meta, w).into_owned(),
            theme::muted(),
        )),
        inner.width,
    );
}

pub(crate) fn draw_role_inspector(
    hub: &ModelHub,
    role: Option<&RoleAssignment>,
    y: u16,
    inner: Rect,
    buf: &mut Buffer,
) {
    draw_rule(y, inner, buf);
    let w = usize::from(inner.width);
    if let Some(r) = role {
        let kind = if r.is_standard {
            "core role"
        } else {
            "custom role"
        };
        let line1 = format!("{} → {}  ({kind})", r.role, r.target);
        buf.set_line(
            inner.x,
            y + 1,
            &Line::from(Span::styled(
                wrap::truncate(&line1, w).into_owned(),
                theme::fg(theme::TEXT),
            )),
            inner.width,
        );
    }
    if let Some((msg, is_err)) = &hub.status {
        draw_status_line(y + 2, inner, buf, msg, *is_err);
    } else {
        let hint = if hub.session_only {
            "session-only mode · press enter to switch this subagent's model temporarily"
        } else {
            "enter pick model · ←/→ cycle :level suffix · ctrl+s save roles as preset"
        };
        buf.set_line(
            inner.x,
            y + 2,
            &Line::from(Span::styled(
                wrap::truncate(hint, w).into_owned(),
                theme::muted(),
            )),
            inner.width,
        );
    }
}

pub(crate) fn draw_preset_inspector(
    preset: Option<&str>,
    status: Option<&(String, bool)>,
    y: u16,
    inner: Rect,
    buf: &mut Buffer,
) {
    draw_rule(y, inner, buf);
    let w = usize::from(inner.width);
    if let Some(name) = preset {
        let text = format!("preset `{name}` · press enter to apply role bundle");
        buf.set_line(
            inner.x,
            y + 1,
            &Line::from(Span::styled(
                wrap::truncate(&text, w).into_owned(),
                theme::fg(theme::TEXT),
            )),
            inner.width,
        );
    }
    if let Some((msg, is_err)) = status {
        draw_status_line(y + 2, inner, buf, msg, *is_err);
    }
}

fn draw_status_line(y: u16, inner: Rect, buf: &mut Buffer, msg: &str, is_err: bool) {
    let prefix = if is_err { "! " } else { "✓ " };
    let style = if is_err {
        theme::fg(theme::ERROR).add_modifier(Modifier::BOLD)
    } else {
        theme::fg(theme::SUCCESS).add_modifier(Modifier::BOLD)
    };
    let text = wrap::truncate(&format!("{prefix}{msg}"), usize::from(inner.width)).into_owned();
    buf.set_line(
        inner.x,
        y,
        &Line::from(Span::styled(text, style)),
        inner.width,
    );
}

fn format_role_badges(roles: &[String], max_width: usize) -> String {
    let mut out = String::new();
    for (idx, role) in roles.iter().enumerate() {
        let badge = format!("[{role}]");
        let sep = usize::from(!out.is_empty());
        let remaining = roles.len().saturating_sub(idx + 1);
        let overflow_reserve = if remaining > 0 { 4 } else { 0 };
        if wrap::str_width(&out) + sep + wrap::str_width(&badge) + overflow_reserve > max_width {
            let left = roles.len() - idx;
            let suffix = format!("+{left}");
            if out.is_empty() {
                return wrap::truncate(&badge, max_width).into_owned();
            }
            if wrap::str_width(&out) + 1 + suffix.len() <= max_width {
                out.push(' ');
                out.push_str(&suffix);
            }
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&badge);
    }
    out
}
