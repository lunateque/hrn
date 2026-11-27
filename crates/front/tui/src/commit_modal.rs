//! Interactive `/commit` confirmation modal: previews attributed files/hunks and lets the user
//! edit the generated commit message before staging and committing.

use crate::diff;
use crate::port_git::CommitPreview;
use crate::theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// Result of feeding a key event into [`CommitModal`].
pub(crate) enum CommitModalAction {
    /// Keep the modal open.
    Stay,
    /// User aborted `/commit` without modifying git state.
    Cancel,
    /// User confirmed staging and committing with the edited message and changelog flag.
    Confirm { message: String, changelog: bool },
}

/// Full-window confirmation view for `/commit`.
pub(crate) struct CommitModal {
    preview: CommitPreview,
    message: String,
    cursor: usize,
    changelog: bool,
    scroll: usize,
    error: Option<String>,
}

impl CommitModal {
    pub(crate) fn new(preview: CommitPreview) -> Self {
        let message = preview.suggested_message.clone();
        let cursor = message.len();
        let changelog = preview.changelog_enabled && preview.has_changelog_file;
        Self {
            preview,
            message,
            cursor,
            changelog,
            scroll: 0,
            error: None,
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> CommitModalAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => CommitModalAction::Cancel,
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => self.confirm(),
            KeyCode::Char('s' | 'S') if ctrl => self.confirm(),
            KeyCode::Tab if self.preview.has_changelog_file => {
                self.changelog = !self.changelog;
                CommitModalAction::Stay
            }
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                CommitModalAction::Stay
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                CommitModalAction::Stay
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(12);
                CommitModalAction::Stay
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(12);
                CommitModalAction::Stay
            }
            KeyCode::Left => {
                self.cursor = prev_char_boundary(&self.message, self.cursor);
                CommitModalAction::Stay
            }
            KeyCode::Right => {
                self.cursor = next_char_boundary(&self.message, self.cursor);
                CommitModalAction::Stay
            }
            KeyCode::Home => {
                self.cursor = 0;
                CommitModalAction::Stay
            }
            KeyCode::End => {
                self.cursor = self.message.len();
                CommitModalAction::Stay
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let prev = prev_char_boundary(&self.message, self.cursor);
                    self.message.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                    self.error = None;
                }
                CommitModalAction::Stay
            }
            KeyCode::Delete => {
                if self.cursor < self.message.len() {
                    let next = next_char_boundary(&self.message, self.cursor);
                    self.message.replace_range(self.cursor..next, "");
                    self.error = None;
                }
                CommitModalAction::Stay
            }
            KeyCode::Char('u' | 'U') if ctrl => {
                self.message.clear();
                self.cursor = 0;
                CommitModalAction::Stay
            }
            KeyCode::Char(c) if !ctrl && !alt && !c.is_control() => {
                self.message.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.error = None;
                CommitModalAction::Stay
            }
            _ => CommitModalAction::Stay,
        }
    }

    fn confirm(&mut self) -> CommitModalAction {
        let trimmed = self.message.trim();
        if trimmed.is_empty() {
            self.error = Some("commit message cannot be empty".to_owned());
            return CommitModalAction::Stay;
        }
        CommitModalAction::Confirm {
            message: trimmed.to_owned(),
            changelog: self.changelog,
        }
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        if area.height < 6 || area.width < 20 {
            return None;
        }
        let mut y = area.y;
        let width = area.width;
        let title = format!(
            " commit · {} file(s) · {} hunk(s) kept · {} skipped ",
            self.preview.files.len(),
            self.preview.hunks_kept,
            self.preview.hunks_skipped,
        );
        buf.set_line(
            area.x,
            y,
            &Line::from(Span::styled(
                title,
                theme::focus().add_modifier(Modifier::BOLD),
            )),
            width,
        );
        y += 1;

        let file_list = self
            .preview
            .files
            .iter()
            .map(|f| wrap::sanitize(f).into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        buf.set_line(
            area.x,
            y,
            &Line::from(vec![
                Span::styled(" files: ", theme::dim()),
                Span::styled(file_list, theme::fg(theme::TEXT)),
            ]),
            width,
        );
        y += 1;

        let footer_rows: u16 = if self.error.is_some() { 4 } else { 3 };
        let diff_end = area.bottom().saturating_sub(footer_rows);
        let diff_height = usize::from(diff_end.saturating_sub(y));
        if diff_height > 0 {
            let (lines, _) = diff::render(
                &self.preview.diff_preview,
                usize::from(width),
                true,
                None,
                diff::Limit::rows(500),
            );
            let max_scroll = lines.len().saturating_sub(diff_height);
            let start = self.scroll.min(max_scroll);
            for (i, line) in lines.iter().skip(start).take(diff_height).enumerate() {
                let row_y = y + u16::try_from(i).unwrap_or(0);
                if row_y < diff_end {
                    buf.set_line(area.x, row_y, line, width);
                }
            }
        }

        Some(self.draw_footer(area, diff_end, width, buf))
    }

    fn draw_footer(&self, area: Rect, msg_y: u16, width: u16, buf: &mut Buffer) -> Position {
        let prefix = " message: ";
        let safe_msg = wrap::sanitize(&self.message);
        buf.set_line(
            area.x,
            msg_y,
            &Line::from(vec![
                Span::styled(prefix, theme::focus().add_modifier(Modifier::BOLD)),
                Span::styled(safe_msg.as_ref(), theme::fg(theme::TEXT)),
            ]),
            width,
        );

        let mut next_y = msg_y + 1;
        if let Some(err) = &self.error {
            buf.set_line(
                area.x,
                next_y,
                &Line::from(Span::styled(
                    format!(" error: {err}"),
                    theme::fg(theme::ERROR),
                )),
                width,
            );
            next_y += 1;
        }
        let changelog_hint = if self.preview.has_changelog_file {
            if self.changelog {
                " [x] CHANGELOG.md (Tab)"
            } else {
                " [ ] CHANGELOG.md (Tab)"
            }
        } else {
            ""
        };
        buf.set_line(
            area.x,
            next_y,
            &Line::from(Span::styled(
                format!(" Enter/Ctrl+S commit · Esc cancel · ↑/↓ scroll{changelog_hint}"),
                theme::dim(),
            )),
            width,
        );

        let cursor_prefix_len = u16::try_from(wrap::str_width(prefix)).unwrap_or(0);
        let before_cursor = self.message.get(..self.cursor).unwrap_or("");
        let cursor_offset = u16::try_from(wrap::str_width(before_cursor)).unwrap_or(0);
        let cx = area
            .x
            .saturating_add(cursor_prefix_len)
            .saturating_add(cursor_offset)
            .min(area.right().saturating_sub(1));
        Position::new(cx, msg_y)
    }
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let clamped = idx.min(s.len());
    s.get(..clamped)
        .and_then(|head| head.char_indices().next_back().map(|(i, _)| i))
        .unwrap_or(0)
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let clamped = idx.min(s.len());
    s.get(clamped..)
        .and_then(|tail| tail.chars().next().map(|c| clamped + c.len_utf8()))
        .unwrap_or(s.len())
}
