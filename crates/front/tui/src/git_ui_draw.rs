//! Rendering for [`GitWorkbench`] (split status sidebar, diff pane, and commit composer footer).

use super::{Focus, GitWorkbench};
use crate::diff;
use crate::highlight;
use crate::theme;
use crate::wrap;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::path::Path;

impl GitWorkbench {
    /// Render the split status sidebar, diff pane, and optional commit composer into `buf`.
    pub fn draw(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        if area.height < 4 || area.width < 24 {
            return None;
        }
        let header_y = area.y;
        let staged = self.entries.iter().filter(|e| e.is_staged()).count();
        let unstaged = self.entries.iter().filter(|e| e.is_unstaged()).count();
        let title = match &self.revision {
            Some(rev) => format!(
                " git · rev {} · {} file(s) ",
                wrap::sanitize(rev),
                self.entries.len()
            ),
            None => format!(
                " git · {} · {} staged · {} unstaged ",
                self.branch, staged, unstaged
            ),
        };
        buf.set_line(
            area.x,
            header_y,
            &Line::from(Span::styled(
                title,
                theme::focus().add_modifier(Modifier::BOLD),
            )),
            area.width,
        );

        let footer_rows: u16 = if self.focus == Focus::Commit { 2 } else { 1 };
        let body_y = header_y + 1;
        let body_h = area.height.saturating_sub(1 + footer_rows);
        let sidebar_w = (area.width / 3)
            .clamp(22, 44)
            .min(area.width.saturating_sub(12));
        let sidebar = Rect::new(area.x, body_y, sidebar_w, body_h);
        let sep_x = area.x + sidebar_w;
        let diff_x = sep_x.saturating_add(1);
        let diff_area = Rect::new(diff_x, body_y, area.right().saturating_sub(diff_x), body_h);

        self.draw_sidebar(sidebar, buf);
        for row in 0..body_h {
            if sep_x < area.right() {
                buf.set_line(
                    sep_x,
                    body_y + row,
                    &Line::from(Span::styled("│", theme::border())),
                    1,
                );
            }
        }
        self.draw_diff(diff_area, buf);

        let footer_y = area.bottom().saturating_sub(footer_rows);
        self.draw_footer(Rect::new(area.x, footer_y, area.width, footer_rows), buf)
    }

    fn draw_sidebar(&self, area: Rect, buf: &mut Buffer) {
        let rows = usize::from(area.height);
        if self.entries.is_empty() {
            let empty = if self.revision.is_some() {
                " (no files in revision)"
            } else {
                " (working tree clean)"
            };
            buf.set_line(
                area.x,
                area.y,
                &Line::from(Span::styled(empty, theme::dim())),
                area.width,
            );
            return;
        }
        let start = if self.selected >= rows {
            self.selected + 1 - rows
        } else {
            0
        };
        for (i, entry) in self.entries.iter().skip(start).take(rows).enumerate() {
            let idx = start + i;
            let selected = idx == self.selected;
            let badge_style = if entry.is_staged() {
                theme::fg(theme::SUCCESS)
            } else if entry.is_untracked() {
                theme::dim()
            } else {
                theme::fg(theme::WARNING)
            };
            let path_style = if selected && self.focus == Focus::Files {
                theme::focus().add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else if selected {
                theme::focus().add_modifier(Modifier::BOLD)
            } else {
                theme::fg(theme::TEXT)
            };
            let marker = if selected { "▸ " } else { "  " };
            let safe_path = wrap::sanitize(&entry.path);
            let line = Line::from(vec![
                Span::styled(marker, path_style),
                Span::styled(entry.badge(), badge_style),
                Span::raw(" "),
                Span::styled(safe_path.into_owned(), path_style),
            ]);
            buf.set_line(
                area.x,
                area.y + u16::try_from(i).unwrap_or(0),
                &line,
                area.width,
            );
        }
    }

    fn draw_diff(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.diff_text.trim().is_empty() {
            buf.set_line(
                area.x,
                area.y,
                &Line::from(Span::styled(" (no diff)", theme::dim())),
                area.width,
            );
            return;
        }
        let lang = self
            .selected_entry()
            .and_then(|e| Path::new(&e.path).extension().and_then(|s| s.to_str()))
            .and_then(highlight::lang);
        let (lines, _) = diff::render(
            &self.diff_text,
            usize::from(area.width),
            true,
            lang,
            diff::Limit::rows(2000),
        );
        let rows = usize::from(area.height);
        let start = self.diff_scroll.min(lines.len().saturating_sub(rows));
        for (i, line) in lines.iter().skip(start).take(rows).enumerate() {
            buf.set_line(
                area.x,
                area.y + u16::try_from(i).unwrap_or(0),
                line,
                area.width,
            );
        }
    }

    fn draw_footer(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        if self.focus == Focus::Commit {
            let prefix = " commit: ";
            let safe = wrap::sanitize(&self.commit_msg);
            buf.set_line(
                area.x,
                area.y,
                &Line::from(vec![
                    Span::styled(prefix, theme::focus().add_modifier(Modifier::BOLD)),
                    Span::styled(safe.as_ref(), theme::fg(theme::TEXT)),
                ]),
                area.width,
            );
            if area.height > 1 {
                let note = self
                    .status_msg
                    .as_deref()
                    .unwrap_or("Enter commit · Ctrl+G generate · Esc back");
                let style = if self.status_is_err {
                    theme::fg(theme::ERROR)
                } else {
                    theme::dim()
                };
                buf.set_line(
                    area.x,
                    area.y + 1,
                    &Line::from(Span::styled(format!(" {}", wrap::sanitize(note)), style)),
                    area.width,
                );
            }
            let prefix_w = u16::try_from(wrap::str_width(prefix)).unwrap_or(0);
            let before = self.commit_msg.get(..self.commit_cursor).unwrap_or("");
            let off = u16::try_from(wrap::str_width(before)).unwrap_or(0);
            let cx = area
                .x
                .saturating_add(prefix_w)
                .saturating_add(off)
                .min(area.right().saturating_sub(1));
            return Some(Position::new(cx, area.y));
        }

        let bar = if let Some(msg) = &self.status_msg {
            let style: Style = if self.status_is_err {
                theme::fg(theme::ERROR)
            } else {
                theme::fg(theme::SUCCESS)
            };
            Line::from(Span::styled(format!(" {}", wrap::sanitize(msg)), style))
        } else {
            let help = if self.revision.is_some() {
                " j/k files · Tab pane · J/K scroll · r refresh · q quit"
            } else {
                " j/k files · Space stage/unstage · s/u stage/unstage · c commit · g gen msg · Tab pane · q quit"
            };
            Line::from(Span::styled(help, theme::dim()))
        };
        buf.set_line(area.x, area.y, &bar, area.width);
        None
    }
}
