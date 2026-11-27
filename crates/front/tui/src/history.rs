//! `Ctrl+R`: search the session's prompt history and put a match back into the editor.

use crate::theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

pub(crate) enum Action {
    Stay,
    Close,
    /// Put this entry back in the editor.
    Use(String),
}

pub(crate) struct HistorySearch {
    entries: Vec<String>,
    query: String,
    selected: usize,
}

impl HistorySearch {
    /// A search over `entries`, newest first.
    pub(crate) fn new(entries: Vec<String>) -> Self {
        let mut entries = entries;
        entries.reverse();
        Self {
            entries,
            query: String::new(),
            selected: 0,
        }
    }

    fn visible(&self) -> Vec<(usize, &str)> {
        let needle = self.query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, text)| needle.is_empty() || text.to_lowercase().contains(&needle))
            .map(|(i, text)| (i, text.as_str()))
            .collect()
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Action {
        let count = self.visible().len();
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Action::Close;
            }
            KeyCode::Enter => {
                return self
                    .visible()
                    .get(self.selected.min(count.saturating_sub(1)))
                    .map_or(Action::Close, |(_, text)| Action::Use((*text).to_owned()));
            }
            KeyCode::Down => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        Action::Stay
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 12 || area.height < 4 {
            return;
        }
        Clear.render(area, buf);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(" Search prompt history ", theme::bold()))
            .title_bottom(Line::from(vec![
                Span::styled("enter", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" use · ", theme::muted()),
                Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" cancel ", theme::muted()),
            ]));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height < 2 {
            return;
        }
        let query = Line::from(vec![
            Span::styled("› ", theme::accent()),
            if self.query.is_empty() {
                Span::styled("type to search", theme::dim())
            } else {
                Span::raw(self.query.clone())
            },
        ]);
        buf.set_line(inner.x, inner.y, &query, inner.width);
        let visible = self.visible();
        let rows = usize::from(inner.height - 2);
        let first = (self.selected + 1).saturating_sub(rows.max(1));
        for (i, (_, text)) in visible.iter().enumerate().skip(first).take(rows) {
            let room = usize::from(inner.width).saturating_sub(2);
            let mut line = Line::from(vec![
                Span::raw(if i == self.selected { "▸ " } else { "  " }),
                Span::raw(wrap::truncate(&wrap::sanitize(text), room).into_owned()),
            ]);
            if i == self.selected {
                line = line.style(theme::focus().add_modifier(Modifier::BOLD));
            }
            let y = inner.y + 1 + u16::try_from(i - first).unwrap_or(0);
            buf.set_line(inner.x, y, &line, inner.width);
        }
        if visible.is_empty() {
            let none = Line::from(Span::styled("no matching prompts", theme::dim()));
            buf.set_line(
                inner.x + 2,
                inner.y + 1,
                &none,
                inner.width.saturating_sub(2),
            );
        }
    }
}
