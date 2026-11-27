//! `/sessions`: a filterable overlay listing stored sessions, newest first, with an `I` import
//! sub-view for read-only Claude Code and Codex CLI transcripts.

use crate::port::ExternalSessionRow;
use crate::theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hrn_kernel::{AgentStatus, SessionMeta};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};
use std::collections::HashMap;

struct ImportState {
    items: Vec<ExternalSessionRow>,
    query: String,
    selected: usize,
}

pub(crate) struct Picker {
    sessions: Vec<SessionMeta>,
    lifecycles: HashMap<String, String>,
    query: String,
    selected: usize,
    import: Option<ImportState>,
}

pub(crate) enum Action {
    Stay,
    Close,
    Resume(String),
    /// User pressed `I` to open the Claude/Codex read-only import sub-view.
    OpenImport,
    /// User picked an external session to import and switch to.
    Import {
        source: String,
        id: String,
    },
}

/// `3m ago`, `5h ago`, `4d ago`, or months for older sessions.
fn ago(then_ms: i64, now_ms: i64) -> String {
    let secs = (now_ms - then_ms).max(0) / 1000;
    match secs {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        86_400..2_592_000 => format!("{}d ago", secs / 86_400),
        _ => format!("{}mo ago", secs / 2_592_000),
    }
}

/// Fallback lifecycle label derived from [`SessionMeta::status`] when the backend did not supply
/// a branch-inspected label (`complete | interrupted | aborted | error`).
pub(crate) const fn default_lifecycle(meta: &SessionMeta) -> &'static str {
    match meta.status {
        AgentStatus::Error => "error",
        AgentStatus::Working | AgentStatus::Blocked => "interrupted",
        AgentStatus::Idle => "complete",
    }
}

impl Picker {
    #[cfg(test)]
    pub(crate) fn new(sessions: Vec<SessionMeta>) -> Self {
        Self::with_lifecycles(sessions, HashMap::new())
    }

    pub(crate) fn with_lifecycles(
        sessions: Vec<SessionMeta>,
        lifecycles: HashMap<String, String>,
    ) -> Self {
        Self {
            sessions,
            lifecycles,
            query: String::new(),
            selected: 0,
            import: None,
        }
    }

    /// True while the `I` Claude/Codex import sub-view is open.
    pub(crate) const fn in_import_view(&self) -> bool {
        self.import.is_some()
    }

    /// Enter the external session import sub-view with discovered Claude/Codex sessions.
    pub(crate) fn enter_import(&mut self, items: Vec<ExternalSessionRow>) {
        self.import = Some(ImportState {
            items,
            query: String::new(),
            selected: 0,
        });
    }

    fn lifecycle_for<'a>(&'a self, session: &'a SessionMeta) -> &'a str {
        self.lifecycles
            .get(&session.id)
            .map_or_else(|| default_lifecycle(session), String::as_str)
    }

    /// The id of the selected session, for the pin/delete chords.
    pub(crate) fn selected_id(&self) -> Option<String> {
        if self.import.is_some() {
            return None;
        }
        self.visible().get(self.selected).map(|s| s.id.clone())
    }

    /// Drop a deleted session from the list.
    pub(crate) fn remove(&mut self, session_id: &str) {
        self.sessions.retain(|s| s.id != session_id);
        self.selected = self.selected.min(self.visible().len().saturating_sub(1));
    }

    /// Reflect a pin change locally so the marker updates before the list is re-fetched.
    pub(crate) fn set_pinned(&mut self, session_id: &str, pinned: bool) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.pinned = pinned;
        }
    }

    fn visible(&self) -> Vec<&SessionMeta> {
        let query = self.query.to_lowercase();
        self.sessions
            .iter()
            .filter(|s| {
                query.is_empty()
                    || s.title.to_lowercase().contains(&query)
                    || s.cwd.to_lowercase().contains(&query)
                    || s.id.starts_with(&query)
            })
            .collect()
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Action {
        if self.import.is_some() {
            return self.import_key(key);
        }
        if key.code == KeyCode::Char('I')
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Action::OpenImport;
        }
        let count = self.visible().len();
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Action::Close;
            }
            KeyCode::Enter => {
                return self
                    .visible()
                    .get(self.selected)
                    .map_or(Action::Stay, |s| Action::Resume(s.id.clone()));
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::PageDown => self.selected = (self.selected + 10).min(count.saturating_sub(1)),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = count.saturating_sub(1),
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

    fn import_key(&mut self, key: KeyEvent) -> Action {
        let Some(imp) = self.import.as_mut() else {
            return Action::Stay;
        };
        let count = visible_imports(imp).len();
        match key.code {
            KeyCode::Esc => {
                self.import = None;
                return Action::Stay;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Action::Close;
            }
            KeyCode::Enter => {
                let picked = visible_imports(imp)
                    .get(imp.selected)
                    .map(|row| (row.source.clone(), row.id.clone()));
                return match picked {
                    Some((source, id)) => Action::Import { source, id },
                    None => Action::Stay,
                };
            }
            KeyCode::Up => imp.selected = imp.selected.saturating_sub(1),
            KeyCode::Down => imp.selected = (imp.selected + 1).min(count.saturating_sub(1)),
            KeyCode::PageUp => imp.selected = imp.selected.saturating_sub(10),
            KeyCode::PageDown => imp.selected = (imp.selected + 10).min(count.saturating_sub(1)),
            KeyCode::Home => imp.selected = 0,
            KeyCode::End => imp.selected = count.saturating_sub(1),
            KeyCode::Backspace => {
                imp.query.pop();
                imp.selected = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                imp.query.push(c);
                imp.selected = 0;
            }
            _ => {}
        }
        Action::Stay
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer, now_ms: i64) {
        if area.width < 12 || area.height < 5 {
            return;
        }
        if let Some(imp) = &self.import {
            draw_import(imp, area, buf, now_ms);
            return;
        }
        Clear.render(area, buf);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(" Resume a session ", theme::bold()))
            .title_bottom(Line::from(vec![
                Span::styled(" enter", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" resume · ", theme::muted()),
                Span::styled("I", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" import · ", theme::muted()),
                Span::styled("alt+k", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" pin · ", theme::muted()),
                Span::styled("bksp", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" delete · ", theme::muted()),
                Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" back ", theme::muted()),
            ]));
        let Some(inner) =
            inset_with_search(area, block, &self.query, "type to filter sessions…", buf)
        else {
            return;
        };
        let inner_width = usize::from(inner.width);

        let visible = self.visible();
        let rows = usize::from(inner.height - 2);
        let first = (self.selected + 1).saturating_sub(rows);
        for (i, session) in visible.iter().enumerate().skip(first).take(rows) {
            let is_sel = i == self.selected;
            let when = format!("{:>9}  ", ago(session.updated_at_ms, now_ms));
            let lifecycle = self.lifecycle_for(session);
            let state_col = format!("{lifecycle:<11}  ");
            let state_style = match lifecycle {
                "interrupted" => theme::fg(theme::WARNING),
                "error" => theme::fg(theme::ERROR),
                "aborted" => theme::muted(),
                _ => theme::dim(),
            };
            let title = if session.title.trim().is_empty() {
                "(untitled)"
            } else {
                session.title.as_str()
            };
            let used = wrap::str_width(&when) + wrap::str_width(&state_col) + 4;
            let room = inner_width.saturating_sub(used);
            let title_style = if is_sel {
                theme::focus().add_modifier(Modifier::BOLD)
            } else {
                theme::fg(theme::TEXT)
            };
            let mut spans = vec![
                Span::styled(if is_sel { "▸ " } else { "  " }, theme::focus()),
                Span::styled(when, theme::dim()),
                Span::styled(state_col, state_style),
                Span::styled(if session.pinned { "★ " } else { "  " }, theme::accent()),
                Span::styled(
                    wrap::truncate(&wrap::sanitize(title), room).into_owned(),
                    title_style,
                ),
            ];
            if is_sel {
                apply_selected_bg(&mut spans, inner_width);
            }
            let y = inner.y + 2 + u16::try_from(i - first).unwrap_or(0);
            buf.set_line(inner.x, y, &Line::from(spans), inner.width);
        }
        if visible.is_empty() {
            let msg = if self.sessions.is_empty() {
                "no stored sessions yet · press I to import from Claude/Codex/OMP"
            } else {
                "no matching sessions"
            };
            let line = Line::from(Span::styled(msg, theme::dim()));
            buf.set_line(
                inner.x + 2,
                inner.y + 2,
                &line,
                inner.width.saturating_sub(2),
            );
        }
    }
}

fn visible_imports(imp: &ImportState) -> Vec<&ExternalSessionRow> {
    let query = imp.query.to_lowercase();
    imp.items
        .iter()
        .filter(|row| {
            query.is_empty()
                || row.source.to_lowercase().contains(&query)
                || row.title.to_lowercase().contains(&query)
                || row.cwd.to_lowercase().contains(&query)
                || row.id.to_lowercase().starts_with(&query)
        })
        .collect()
}

fn inset_with_search(
    area: Rect,
    block: Block<'_>,
    query: &str,
    placeholder: &str,
    buf: &mut Buffer,
) -> Option<Rect> {
    let raw_inner = block.inner(area);
    block.render(area, buf);
    let pad = u16::from(raw_inner.width >= 24);
    let inner = Rect::new(
        raw_inner.x + pad,
        raw_inner.y,
        raw_inner.width.saturating_sub(pad * 2),
        raw_inner.height,
    );
    if inner.height < 3 {
        return None;
    }
    let query_line = Line::from(vec![
        Span::styled("❯ ", theme::accent().add_modifier(Modifier::BOLD)),
        if query.is_empty() {
            Span::styled(placeholder.to_owned(), theme::dim())
        } else {
            Span::styled(query.to_owned(), theme::fg(theme::TEXT))
        },
    ]);
    buf.set_line(inner.x, inner.y, &query_line, inner.width);
    let rule = Line::from(Span::styled(
        "─".repeat(usize::from(inner.width)),
        theme::fg(theme::BORDER_MUTED),
    ));
    buf.set_line(inner.x, inner.y + 1, &rule, inner.width);
    Some(inner)
}

fn apply_selected_bg(spans: &mut Vec<Span<'static>>, inner_width: usize) {
    let total_w: usize = spans.iter().map(Span::width).sum();
    if inner_width > total_w {
        spans.push(Span::raw(" ".repeat(inner_width - total_w)));
    }
    for span in spans {
        span.style = span.style.bg(theme::SELECTED_BG);
    }
}

fn draw_import(imp: &ImportState, area: Rect, buf: &mut Buffer, now_ms: i64) {
    Clear.render(area, buf);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(Span::styled(
            " Import Claude / Codex / OMP session (read-only) ",
            theme::bold(),
        ))
        .title_bottom(Line::from(vec![
            Span::styled(" enter", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" import & switch · ", theme::muted()),
            Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" back to sessions ", theme::muted()),
        ]));
    let Some(inner) = inset_with_search(
        area,
        block,
        &imp.query,
        "type to filter claude/codex/omp sessions…",
        buf,
    ) else {
        return;
    };
    let inner_width = usize::from(inner.width);

    let visible = visible_imports(imp);
    let rows = usize::from(inner.height - 2);
    let first = (imp.selected + 1).saturating_sub(rows);
    for (i, item) in visible.iter().enumerate().skip(first).take(rows) {
        let is_sel = i == imp.selected;
        let badge = format!("[{:6}] ", item.source);
        let when = format!("{:>9}  ", ago(item.updated_at_ms, now_ms));
        let title = if item.title.trim().is_empty() {
            item.id.as_str()
        } else {
            item.title.as_str()
        };
        let label = format!("{title} ({})", item.cwd);
        let used = 2 + wrap::str_width(&badge) + wrap::str_width(&when);
        let room = inner_width.saturating_sub(used);
        let label_style = if is_sel {
            theme::focus().add_modifier(Modifier::BOLD)
        } else {
            theme::fg(theme::TEXT)
        };
        let mut spans = vec![
            Span::styled(if is_sel { "▸ " } else { "  " }, theme::focus()),
            Span::styled(badge, theme::accent()),
            Span::styled(when, theme::dim()),
            Span::styled(
                wrap::truncate(&wrap::sanitize(&label), room).into_owned(),
                label_style,
            ),
        ];
        if is_sel {
            apply_selected_bg(&mut spans, inner_width);
        }
        let y = inner.y + 2 + u16::try_from(i - first).unwrap_or(0);
        buf.set_line(inner.x, y, &Line::from(spans), inner.width);
    }
    if visible.is_empty() {
        let msg = if imp.items.is_empty() {
            "no Claude, Codex, or OMP sessions found"
        } else {
            "no matching external sessions"
        };
        let line = Line::from(Span::styled(msg, theme::dim()));
        buf.set_line(
            inner.x + 2,
            inner.y + 2,
            &line,
            inner.width.saturating_sub(2),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::{AgentStatus, PermissionMode, ThinkingLevel};

    fn meta(id: &str, title: &str) -> SessionMeta {
        SessionMeta {
            pinned: false,
            id: id.to_owned(),
            title: title.to_owned(),
            cwd: "C:/work".to_owned(),
            branch: None,
            head_node_id: None,
            model: None,
            thinking: ThinkingLevel::Off,
            goal: None,
            permission_mode: PermissionMode::Default,
            parent_session_id: None,
            parent_node_id: None,
            status: AgentStatus::Idle,
            title_source: None,
            exit_ms: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn filters_and_picks_a_session() {
        let mut picker = Picker::new(vec![meta("a1", "fix parser"), meta("b2", "write docs")]);
        for c in "docs".chars() {
            picker.key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert!(
            matches!(picker.key(KeyEvent::from(KeyCode::Enter)), Action::Resume(id) if id == "b2")
        );
        assert!(matches!(
            picker.key(KeyEvent::from(KeyCode::Esc)),
            Action::Close
        ));
        assert_eq!(ago(0, 7_200_000), "2h ago");
    }

    #[test]
    fn import_chord_opens_and_selects_external_session() {
        let mut picker = Picker::new(vec![meta("a1", "existing")]);
        assert!(matches!(
            picker.key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT)),
            Action::OpenImport
        ));
        picker.enter_import(vec![
            ExternalSessionRow {
                source: "claude".into(),
                id: "c-uuid-1".into(),
                title: "Refactor auth".into(),
                cwd: "/repo/a".into(),
                updated_at_ms: 1000,
                archived: false,
            },
            ExternalSessionRow {
                source: "codex".into(),
                id: "x-thread-2".into(),
                title: "Fix storage".into(),
                cwd: "/repo/b".into(),
                updated_at_ms: 500,
                archived: false,
            },
        ]);
        assert!(picker.in_import_view());
        picker.key(KeyEvent::from(KeyCode::Down));
        assert!(matches!(
            picker.key(KeyEvent::from(KeyCode::Enter)),
            Action::Import { source, id } if source == "codex" && id == "x-thread-2"
        ));
        assert!(matches!(
            picker.key(KeyEvent::from(KeyCode::Esc)),
            Action::Stay
        ));
        assert!(!picker.in_import_view());
    }

    #[test]
    fn draws_lifecycle_labels_for_sessions() {
        let mut s1 = meta("s1", "done");
        s1.status = AgentStatus::Idle;
        let mut s2 = meta("s2", "crashed");
        s2.status = AgentStatus::Working;
        let mut s3 = meta("s3", "cancelled");
        s3.status = AgentStatus::Idle;
        let mut s4 = meta("s4", "failed");
        s4.status = AgentStatus::Error;
        let mut labels = HashMap::new();
        labels.insert("s3".to_owned(), "aborted".to_owned());
        let picker = Picker::with_lifecycles(vec![s1, s2, s3, s4], labels);
        let area = Rect::new(0, 0, 80, 12);
        let mut buf = Buffer::empty(area);
        picker.draw(area, &mut buf, 10_000);
        let text: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for expected in ["complete", "interrupted", "aborted", "error"] {
            assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
        }
    }
}
