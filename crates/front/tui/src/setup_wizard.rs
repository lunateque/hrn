//! `/setup`: 3-step first-run onboarding wizard (1. Login/Provider -> 2. Default Role -> 3. Theme).

use crate::port::SetupWizardInfo;
use crate::theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

/// Step in the 3-step `/setup` wizard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetupStep {
    Login,
    DefaultRole,
    Theme,
}

impl SetupStep {
    const fn number(self) -> u8 {
        match self {
            Self::Login => 1,
            Self::DefaultRole => 2,
            Self::Theme => 3,
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Login => "Provider Login",
            Self::DefaultRole => "Default Model Role",
            Self::Theme => "Appearance Theme",
        }
    }
}

pub(crate) enum SetupAction {
    Stay,
    Close,
    OpenLogin,
    SetDefaultRole(String),
    ApplyTheme(String),
    Finish,
}

pub(crate) struct SetupWizardView {
    step: SetupStep,
    info: SetupWizardInfo,
    selected: usize,
    error: Option<String>,
}

impl SetupWizardView {
    pub(crate) fn new(info: SetupWizardInfo) -> Self {
        Self {
            step: SetupStep::Login,
            info,
            selected: 0,
            error: None,
        }
    }

    pub(crate) fn refresh_and_advance(&mut self, info: SetupWizardInfo) {
        self.info = info;
        self.error = None;
        self.advance_to(SetupStep::DefaultRole);
    }

    pub(crate) fn refresh_info(&mut self, info: SetupWizardInfo) {
        self.info = info;
    }

    pub(crate) fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
    }

    pub(crate) fn advance_to(&mut self, step: SetupStep) {
        self.step = step;
        self.error = None;
        self.selected = match step {
            SetupStep::Login => 0,
            SetupStep::DefaultRole => self
                .info
                .current_default
                .as_deref()
                .and_then(|cur| self.info.model_aliases.iter().position(|a| a == cur))
                .unwrap_or(0),
            SetupStep::Theme => self
                .info
                .themes
                .iter()
                .position(|t| t == &self.info.current_theme)
                .unwrap_or(0),
        };
    }

    fn option_count(&self) -> usize {
        match self.step {
            SetupStep::Login => 2,
            SetupStep::DefaultRole => self.info.model_aliases.len() + 1,
            SetupStep::Theme => self.info.themes.len().max(1),
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> SetupAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return SetupAction::Close;
        }
        let count = self.option_count();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return SetupAction::Close,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('s') if self.step != SetupStep::Theme => {
                let next = match self.step {
                    SetupStep::Login => SetupStep::DefaultRole,
                    SetupStep::DefaultRole | SetupStep::Theme => SetupStep::Theme,
                };
                self.advance_to(next);
            }
            KeyCode::Enter => return self.confirm_selected(),
            _ => {}
        }
        SetupAction::Stay
    }

    fn confirm_selected(&mut self) -> SetupAction {
        match self.step {
            SetupStep::Login => {
                if self.selected == 0 {
                    SetupAction::OpenLogin
                } else {
                    self.advance_to(SetupStep::DefaultRole);
                    SetupAction::Stay
                }
            }
            SetupStep::DefaultRole => {
                if let Some(alias) = self.info.model_aliases.get(self.selected).cloned() {
                    SetupAction::SetDefaultRole(alias)
                } else {
                    self.advance_to(SetupStep::Theme);
                    SetupAction::Stay
                }
            }
            SetupStep::Theme => {
                if let Some(theme_name) = self.info.themes.get(self.selected).cloned() {
                    SetupAction::ApplyTheme(theme_name)
                } else {
                    SetupAction::Finish
                }
            }
        }
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 24 || area.height < 8 {
            return;
        }
        Clear.render(area, buf);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::accent())
            .title(Span::styled(
                format!(
                    " Setup · Step {}/3 · {} ",
                    self.step.number(),
                    self.step.title()
                ),
                theme::bold(),
            ))
            .title_bottom(Line::from(vec![
                Span::styled(" ↑↓", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" move · ", theme::muted()),
                Span::styled("enter", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" select · ", theme::muted()),
                Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" close ", theme::muted()),
            ]));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height < 5 {
            return;
        }

        let subtitle = self.subtitle();
        let width = usize::from(inner.width.saturating_sub(2));
        buf.set_line(
            inner.x + 1,
            inner.y,
            &Line::from(Span::styled(
                wrap::truncate(&subtitle, width).into_owned(),
                theme::muted(),
            )),
            inner.width.saturating_sub(2),
        );

        let items = self.rows();
        let room = usize::from(inner.height.saturating_sub(4));
        let first = (self.selected + 1).saturating_sub(room);
        for (i, (label, detail)) in items.iter().enumerate().skip(first).take(room) {
            let is_sel = i == self.selected;
            let row_style = if is_sel {
                theme::focus().add_modifier(Modifier::BOLD)
            } else {
                theme::plain()
            };
            let used = 2 + label.len() + 2;
            let note = wrap::truncate(detail, width.saturating_sub(used)).into_owned();
            let line = Line::from(vec![
                Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
                Span::styled(label.clone(), row_style),
                Span::raw("  "),
                Span::styled(note, theme::muted()),
            ]);
            let y = inner.y + 2 + u16::try_from(i - first).unwrap_or(0);
            if is_sel {
                buf.set_style(Rect::new(inner.x, y, inner.width, 1), theme::surface());
            }
            buf.set_line(inner.x + 1, y, &line, inner.width.saturating_sub(2));
        }

        if let Some(err) = &self.error {
            let y = inner.bottom().saturating_sub(1);
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
    }

    fn subtitle(&self) -> String {
        match self.step {
            SetupStep::Login => {
                if self.info.configured_providers.is_empty() {
                    "No providers authenticated yet. Connect a provider to start:".to_owned()
                } else {
                    format!(
                        "Active providers: {}. Add another or skip to choose a default model:",
                        self.info.configured_providers.join(", ")
                    )
                }
            }
            SetupStep::DefaultRole => {
                let cur = self.info.current_default.as_deref().unwrap_or("(not set)");
                format!("Pick the default model alias ([roles] default; current: {cur}):")
            }
            SetupStep::Theme => format!(
                "Pick a built-in colour theme ([tui] theme; current: {}):",
                self.info.current_theme
            ),
        }
    }

    fn rows(&self) -> Vec<(String, String)> {
        match self.step {
            SetupStep::Login => vec![
                (
                    "Configure / log in to a provider".to_owned(),
                    "Claude, ChatGPT, Kimi Code, or API key".to_owned(),
                ),
                (
                    "Skip login step".to_owned(),
                    "keep current provider credentials".to_owned(),
                ),
            ],
            SetupStep::DefaultRole => {
                let mut rows: Vec<(String, String)> = self
                    .info
                    .model_aliases
                    .iter()
                    .map(|alias| {
                        let note = if self.info.current_default.as_deref() == Some(alias.as_str()) {
                            "current default".to_owned()
                        } else {
                            String::new()
                        };
                        (alias.clone(), note)
                    })
                    .collect();
                rows.push((
                    "Skip default role".to_owned(),
                    "keep current [roles] default".to_owned(),
                ));
                rows
            }
            SetupStep::Theme => self
                .info
                .themes
                .iter()
                .map(|t| {
                    let note = if *t == self.info.current_theme {
                        "active".to_owned()
                    } else {
                        "built-in".to_owned()
                    };
                    (t.clone(), note)
                })
                .collect(),
        }
    }
}
