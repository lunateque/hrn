//! `/logout`: interactive provider credential picker + inline confirmation modal.

use crate::port::LogoutProviderChoice;
use crate::theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

pub(crate) enum LogoutAction {
    Stay,
    Close,
    Confirm(String),
}

enum Stage {
    Pick,
    Confirm {
        choice: LogoutProviderChoice,
        single: bool,
    },
}

pub(crate) struct LogoutView {
    providers: Vec<LogoutProviderChoice>,
    selected: usize,
    stage: Stage,
}

impl LogoutView {
    pub(crate) fn new(providers: Vec<LogoutProviderChoice>) -> Self {
        Self {
            providers,
            selected: 0,
            stage: Stage::Pick,
        }
    }

    pub(crate) fn confirm_single(choice: LogoutProviderChoice) -> Self {
        Self {
            providers: vec![choice.clone()],
            selected: 0,
            stage: Stage::Confirm {
                choice,
                single: true,
            },
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> LogoutAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return LogoutAction::Close;
        }
        match &self.stage {
            Stage::Pick => self.pick_key(key),
            Stage::Confirm { choice, single } => {
                let id = choice.id.clone();
                let single = *single;
                match key.code {
                    KeyCode::Enter | KeyCode::Char('y' | 'Y') => LogoutAction::Confirm(id),
                    KeyCode::Esc | KeyCode::Char('n' | 'N' | 'q') => {
                        if single {
                            LogoutAction::Close
                        } else {
                            self.stage = Stage::Pick;
                            LogoutAction::Stay
                        }
                    }
                    _ => LogoutAction::Stay,
                }
            }
        }
    }

    fn pick_key(&mut self, key: KeyEvent) -> LogoutAction {
        let count = self.providers.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return LogoutAction::Close,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Enter => {
                let Some(choice) = self.providers.get(self.selected).cloned() else {
                    return LogoutAction::Close;
                };
                self.stage = Stage::Confirm {
                    choice,
                    single: false,
                };
            }
            _ => {}
        }
        LogoutAction::Stay
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 20 || area.height < 7 {
            return;
        }
        Clear.render(area, buf);
        match &self.stage {
            Stage::Pick => self.draw_picker(area, buf),
            Stage::Confirm { choice, .. } => draw_confirm(area, buf, choice),
        }
    }

    fn draw_picker(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(
                " Logout · Select credentials to remove ",
                theme::bold(),
            ))
            .title_bottom(Line::from(vec![
                Span::styled(" ↑↓", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" move · ", theme::muted()),
                Span::styled("enter", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" select · ", theme::muted()),
                Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" cancel ", theme::muted()),
            ]));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height < 4 {
            return;
        }
        buf.set_line(
            inner.x + 1,
            inner.y,
            &Line::from(Span::styled(
                "Choose a provider whose stored credentials should be deleted:",
                theme::muted(),
            )),
            inner.width.saturating_sub(2),
        );
        let rows = usize::from(inner.height.saturating_sub(3));
        let first = (self.selected + 1).saturating_sub(rows);
        let width = usize::from(inner.width.saturating_sub(2));
        for (i, p) in self.providers.iter().enumerate().skip(first).take(rows) {
            let is_sel = i == self.selected;
            let row_style = if is_sel {
                theme::focus().add_modifier(Modifier::BOLD)
            } else {
                theme::plain()
            };
            let name_col = format!("{:<20}", p.name);
            let kind_col = format!("[{:<12}]", p.kind);
            let used = 2 + name_col.len() + 1 + kind_col.len() + 2;
            let detail = wrap::truncate(&p.detail, width.saturating_sub(used)).into_owned();
            let line = Line::from(vec![
                Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
                Span::styled(name_col, row_style),
                Span::raw(" "),
                Span::styled(kind_col, theme::dim()),
                Span::raw("  "),
                Span::styled(detail, theme::muted()),
            ]);
            let y = inner.y + 2 + u16::try_from(i - first).unwrap_or(0);
            if is_sel {
                buf.set_style(Rect::new(inner.x, y, inner.width, 1), theme::surface());
            }
            buf.set_line(inner.x + 1, y, &line, inner.width.saturating_sub(2));
        }
    }
}

fn draw_confirm(area: Rect, buf: &mut Buffer, choice: &LogoutProviderChoice) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(theme::WARNING))
        .title(Span::styled(
            format!(" Logout · Confirm {} ", choice.name),
            theme::bold(),
        ))
        .title_bottom(Line::from(vec![
            Span::styled(" y / enter", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" remove credentials · ", theme::muted()),
            Span::styled("n / esc", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" cancel ", theme::muted()),
        ]));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 4 {
        return;
    }
    let width = usize::from(inner.width.saturating_sub(2));
    buf.set_line(
        inner.x + 1,
        inner.y + 1,
        &Line::from(Span::styled(
            format!(
                "Remove stored {} credentials for `{}`?",
                choice.kind, choice.id
            ),
            theme::plain().add_modifier(Modifier::BOLD),
        )),
        inner.width.saturating_sub(2),
    );
    buf.set_line(
        inner.x + 1,
        inner.y + 3,
        &Line::from(Span::styled(
            wrap::truncate(&choice.detail, width).into_owned(),
            theme::muted(),
        )),
        inner.width.saturating_sub(2),
    );
}
