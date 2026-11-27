//! Generic scrollable views the slash commands open: text pages (`/settings`, `/context`,
//! `/tools`, `/mcp`, `/jobs`, `/hotkeys`, `/todo`).

use crate::theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

/// Rows a view shows at once, below its frame (with 1-column horizontal inset when wide enough).
fn inner(area: Rect) -> Option<(Rect, usize)> {
    if area.width < 10 || area.height < 5 {
        return None;
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border());
    let raw = block.inner(area);
    let pad = u16::from(raw.width >= 24);
    let inset = Rect::new(
        raw.x + pad,
        raw.y,
        raw.width.saturating_sub(pad * 2),
        raw.height,
    );
    Some((inset, usize::from(inset.height)))
}

fn frame(area: Rect, buf: &mut Buffer, title: &str, hint: &str) {
    Clear.render(area, buf);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(Span::styled(format!(" {title} "), theme::bold()))
        .title_bottom(Line::from(vec![
            Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {hint} "), theme::muted()),
        ]));
    block.render(area, buf);
}

/// Keys a scrollable view answers: arrows, pgup/pgdn, home/end, esc/q. `true` consumed.
fn scroll_keys(key: KeyEvent, top: &mut usize, count: usize, rows: usize) -> Option<bool> {
    let last = count.saturating_sub(rows);
    match key.code {
        KeyCode::Up => *top = top.saturating_sub(1),
        KeyCode::Down => *top = (*top + 1).min(last),
        KeyCode::PageUp => *top = top.saturating_sub(rows),
        KeyCode::PageDown => *top = (*top + rows).min(last),
        KeyCode::Home => *top = 0,
        KeyCode::End => *top = last,
        KeyCode::Esc | KeyCode::Char('q') => return Some(false),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Some(false),
        _ => return None,
    }
    Some(true)
}

/// A fixed page of text with a scrollbar hint in the title.
pub(crate) struct TextView {
    title: String,
    lines: Vec<String>,
    top: usize,
}

/// Keys a view answers.
pub(crate) enum ViewAction {
    Stay,
    Close,
}

/// The open slash-command view.
pub(crate) enum View {
    Text(TextView),
}

impl View {
    pub(crate) fn key(&mut self, key: KeyEvent, area: Rect) -> ViewAction {
        let rows = inner(area).map_or(10, |(_, rows)| rows);
        match self {
            Self::Text(page) => page.key(key, rows),
        }
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        match self {
            Self::Text(page) => page.draw(area, buf),
        }
    }
}

impl TextView {
    pub(crate) fn new(title: impl Into<String>, text: &str) -> Self {
        let lines = text
            .lines()
            .map(|line| wrap::sanitize(line).into_owned())
            .collect();
        Self {
            title: title.into(),
            lines,
            top: 0,
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent, rows: usize) -> ViewAction {
        match scroll_keys(key, &mut self.top, self.lines.len(), rows) {
            Some(true) | None => ViewAction::Stay,
            Some(false) => ViewAction::Close,
        }
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        let Some((inner, rows)) = inner(area) else {
            return;
        };
        let shown = self.lines.len();
        let title = if shown > rows {
            format!("{} · {}/{}", self.title, self.top + 1, shown - rows + 1)
        } else {
            self.title.clone()
        };
        frame(area, buf, &title, "close");
        for (y, line) in self.lines.iter().skip(self.top).take(rows).enumerate() {
            let trimmed = line.trim();
            let style = if (trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() > 2)
                || trimmed.starts_with("── ")
                || trimmed.starts_with("## ")
            {
                theme::accent().add_modifier(Modifier::BOLD)
            } else if !trimmed.is_empty() && trimmed.chars().all(|c| c == '─' || c == '-') {
                theme::fg(theme::BORDER_MUTED)
            } else {
                theme::plain()
            };
            let row = Line::from(Span::styled(line.clone(), style));
            buf.set_line(
                inner.x,
                inner.y + u16::try_from(y).unwrap_or(u16::MAX),
                &row,
                inner.width,
            );
        }
    }
}

/// One selectable row rendered inside [`ListView`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ListRow {
    /// Leading status/badge column (e.g. `[on]` / `[off]`).
    pub(crate) badge: String,
    /// Primary identifier or title.
    pub(crate) primary: String,
    /// Secondary metadata (discovery source, model override, description).
    pub(crate) meta: String,
    /// Highlight badge in accent/success colour when true.
    pub(crate) active: bool,
    /// Render row dimmed when disabled/inactive.
    pub(crate) dim: bool,
}

/// Result of routing a navigation key into [`ListView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListNav {
    /// Selection moved or scroll updated.
    Moved,
    /// User pressed `Enter` on the selected row index.
    Select(usize),
    /// User pressed `Esc` or `q` to close.
    Close,
    /// Key was not a standard navigation key (caller may handle custom shortcuts).
    Unhandled,
}

/// Reusable scrollable selectable list widget shared by hubs and pickers (`/agents`, ...).
pub(crate) struct ListView {
    title: String,
    hint: String,
    rows: Vec<ListRow>,
    selected: usize,
    top: usize,
}

impl ListView {
    pub(crate) fn new(
        title: impl Into<String>,
        hint: impl Into<String>,
        rows: Vec<ListRow>,
    ) -> Self {
        Self {
            title: title.into(),
            hint: hint.into(),
            rows,
            selected: 0,
            top: 0,
        }
    }

    pub(crate) fn set_rows(&mut self, rows: Vec<ListRow>) {
        self.rows = rows;
        if self.rows.is_empty() {
            self.selected = 0;
            self.top = 0;
        } else if self.selected >= self.rows.len() {
            self.selected = self.rows.len() - 1;
        }
    }

    pub(crate) fn set_hint(&mut self, hint: impl Into<String>) {
        self.hint = hint.into();
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent, area: Rect) -> ListNav {
        let visible = inner(area).map_or(10, |(_, rows)| rows).max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return ListNav::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return ListNav::Close;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.rows.len() {
                    self.selected += 1;
                }
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(visible);
            }
            KeyCode::PageDown => {
                self.selected = (self.selected + visible).min(self.rows.len().saturating_sub(1));
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.rows.len().saturating_sub(1),
            KeyCode::Enter => {
                return if self.rows.is_empty() {
                    ListNav::Moved
                } else {
                    ListNav::Select(self.selected)
                };
            }
            _ => return ListNav::Unhandled,
        }
        self.ensure_visible(visible);
        ListNav::Moved
    }

    fn ensure_visible(&mut self, visible: usize) {
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + visible {
            self.top = self.selected + 1 - visible;
        }
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer, footer_line: Option<&str>) {
        let Some((inner_area, rows)) = inner(area) else {
            return;
        };
        let title = if self.rows.is_empty() {
            format!("{} (0)", self.title)
        } else {
            format!("{} ({}/{})", self.title, self.selected + 1, self.rows.len())
        };
        frame(area, buf, &title, &self.hint);
        let has_divider = footer_line.is_some() && rows >= 4;
        let list_rows = if has_divider {
            rows.saturating_sub(2)
        } else if footer_line.is_some() {
            rows.saturating_sub(1)
        } else {
            rows
        };
        let w = usize::from(inner_area.width);
        let badge_w = self
            .rows
            .iter()
            .map(|r| wrap::str_width(&r.badge))
            .max()
            .unwrap_or(0);
        let primary_w = self
            .rows
            .iter()
            .map(|r| wrap::str_width(&r.primary))
            .max()
            .unwrap_or(0)
            .min(24);
        for (idx, row) in self.rows.iter().skip(self.top).take(list_rows).enumerate() {
            let abs = self.top + idx;
            let is_sel = abs == self.selected;
            let mark = if is_sel { "▸ " } else { "  " };
            let badge_style = if row.dim {
                theme::dim()
            } else if row.active {
                theme::fg(theme::SUCCESS).add_modifier(Modifier::BOLD)
            } else {
                theme::muted()
            };
            let primary_style = if is_sel {
                theme::focus().add_modifier(Modifier::BOLD)
            } else if row.dim {
                theme::dim()
            } else {
                theme::fg(theme::TEXT)
            };
            let meta_style = if row.dim {
                theme::dim()
            } else {
                theme::muted()
            };
            let badge_pad = format!("{:<badge_w$}", row.badge);
            let prim_trunc = wrap::truncate(&row.primary, primary_w);
            let prim_pad = format!("{prim_trunc:<primary_w$}");
            let used = 2 + wrap::str_width(&badge_pad) + 1 + wrap::str_width(&prim_pad) + 2;
            let meta_max = w.saturating_sub(used);
            let meta_trunc = wrap::truncate(&wrap::sanitize(&row.meta), meta_max).into_owned();
            let mut spans = vec![
                Span::styled(mark, theme::focus()),
                Span::styled(badge_pad, badge_style),
                Span::raw(" "),
                Span::styled(prim_pad, primary_style),
                Span::raw("  "),
                Span::styled(meta_trunc, meta_style),
            ];
            if is_sel {
                let total_w: usize = spans.iter().map(Span::width).sum();
                if w > total_w {
                    spans.push(Span::raw(" ".repeat(w - total_w)));
                }
                for span in &mut spans {
                    span.style = span.style.bg(theme::SELECTED_BG);
                }
            }
            buf.set_line(
                inner_area.x,
                inner_area.y + u16::try_from(idx).unwrap_or(u16::MAX),
                &Line::from(spans),
                inner_area.width,
            );
        }
        if let Some(footer) = footer_line
            && rows > 0
        {
            if has_divider {
                let rule_y = inner_area.y + u16::try_from(rows - 2).unwrap_or(0);
                let rule = Line::from(Span::styled("─".repeat(w), theme::fg(theme::BORDER_MUTED)));
                buf.set_line(inner_area.x, rule_y, &rule, inner_area.width);
            }
            let y = inner_area.y + u16::try_from(rows - 1).unwrap_or(0);
            let line = Line::from(Span::styled(
                wrap::truncate(footer, w).into_owned(),
                theme::focus().add_modifier(Modifier::BOLD),
            ));
            buf.set_line(inner_area.x, y, &line, inner_area.width);
        }
    }
}
