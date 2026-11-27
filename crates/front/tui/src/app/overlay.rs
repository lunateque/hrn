//! The floating panels: the approval prompt under the band and the slash-command popup over
//! the live rows. Both use the overlay panel look: a rounded frame with a bold title in the
//! top rule, `├───┤` dividers and an action bar in the bottom rule.

use super::App;
use crate::approvals::{ApprovalDock, choices_for, render as render_approval};
use crate::card::{Frame, Section};
use crate::theme;
use crate::toolfmt::{self, Preview};
use crate::wrap;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// Completion rows shown at once.
const POPUP_ROWS: usize = 10;

impl App {
    /// The approval panel for the oldest pending request, `width` columns, at most `max_rows`.
    pub(super) fn approval_lines(&self, width: usize, max_rows: usize) -> Vec<Line<'static>> {
        let Some(ask) = self.approvals.front() else {
            return Vec::new();
        };
        let request = &ask.request;
        let has_draft = !self.editor.is_blank();
        let choices = choices_for(request, self.settings.mode, has_draft);
        let fallback = ApprovalDock::open(self.now(), &choices, has_draft);
        let dock = self.approval_dock.as_ref().unwrap_or(&fallback);
        let preview = self
            .transcript
            .tool(&request.id)
            .map_or(Preview::None, |card| {
                toolfmt::preview(&card.name, &card.arguments)
            });
        render_approval(
            request,
            preview,
            dock,
            &choices,
            self.approvals.len(),
            &self.keymap,
            width,
            max_rows,
        )
    }

    /// The completion popup as rows at most `width` columns and `max_rows` rows; empty when
    /// nothing completes or there is no room.
    pub(super) fn popup_card(&self, width: usize, max_rows: usize) -> Vec<Line<'static>> {
        if self.login.is_some()
            || self.logout.is_some()
            || self.setup.is_some()
            || self.model_hub.is_some()
            || self.settings_ui.is_some()
            || self.agent_hub.is_some()
            || self.agents_hub.is_some()
            || self.local_picker.is_some()
            || self.link_hints.is_some()
            || self.picker.is_some()
            || self.tree.is_some()
            || self.btw.is_some()
            || self.image_modal.is_some()
            || self.remote_qr.is_some()
            || !self.approvals.is_empty()
            || self.active_ask.is_some()
            || max_rows < 3
        {
            return Vec::new();
        }
        let choices = self.completions();
        let count = choices.len();
        if count == 0 {
            return Vec::new();
        }
        let rows = count.min(POPUP_ROWS).min(max_rows - 2);
        let selected = self.popup.selected.min(count - 1);
        let first = (selected + 1).saturating_sub(rows);
        let label_width = choices
            .iter()
            .map(|c| wrap::str_width(&c.label))
            .max()
            .unwrap_or(0);
        let help_width = choices
            .iter()
            .map(|c| wrap::str_width(c.help.as_ref()))
            .max()
            .unwrap_or(0);
        let width = (label_width + help_width + 10).min(width);
        let frame = Frame {
            width,
            border: theme::BORDER,
            bg: None,
            pad: 1,
        };
        let inner = frame.inner();
        let lines: Vec<Line<'static>> = choices
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(i, choice)| {
                let on = i == selected;
                let label_style = if on {
                    theme::focus().add_modifier(Modifier::BOLD)
                } else {
                    theme::plain()
                };
                let mut spans = vec![
                    Span::styled(if on { "❯ " } else { "  " }, theme::focus()),
                    Span::styled(format!("{:<label_width$}", choice.label), label_style),
                    Span::raw("  "),
                    Span::styled(choice.help.as_ref().to_owned(), theme::muted()),
                ];
                if on {
                    let used: usize = spans.iter().map(Span::width).sum();
                    spans.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
                    for span in &mut spans {
                        span.style = span.style.bg(theme::SELECTED_BG);
                    }
                }
                Line::from(spans)
            })
            .collect();
        let header = vec![Span::styled(
            "Commands",
            theme::accent().add_modifier(Modifier::BOLD),
        )];
        let footer = if count > rows {
            vec![Span::styled(
                format!("{}/{count}", selected + 1),
                theme::muted(),
            )]
        } else {
            Vec::new()
        };
        frame.render(header, vec![Section::plain(lines)], footer)
    }
}
