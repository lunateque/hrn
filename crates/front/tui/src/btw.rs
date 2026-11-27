//! `/btw` overlay panel: side-thread streaming answer view, follow-up input, topic history list,
//! and `<inject>…</inject>` rail (`app.btw.inject`, `Alt+I`).

use crate::theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hrn_kernel::{BtwExchange, BtwStatus, BtwTopic, extract_inject_block};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

pub(crate) enum BtwAction {
    Stay,
    Close,
    CancelStream,
    Copy(String),
    FollowUp { topic_id: String, question: String },
    Inject { topic: BtwTopic, payload: String },
    AlreadyInjected,
}

enum BtwView {
    Topic {
        topic: BtwTopic,
        streaming: bool,
        follow_up: Option<String>,
        scroll: usize,
    },
    History {
        topics: Vec<BtwTopic>,
        selected: usize,
    },
}

pub(crate) struct BtwPanel {
    view: BtwView,
    btw_inject: bool,
}

impl BtwPanel {
    /// Open a streaming `/btw` panel for a new or continued side question.
    pub(crate) fn streaming(topic: BtwTopic, btw_inject: bool) -> Self {
        Self {
            view: BtwView::Topic {
                topic,
                streaming: true,
                follow_up: None,
                scroll: 0,
            },
            btw_inject,
        }
    }

    /// Open the `/btw` topic history list (bare `/btw`).
    pub(crate) fn history(topics: Vec<BtwTopic>, btw_inject: bool) -> Self {
        Self {
            view: BtwView::History {
                topics,
                selected: 0,
            },
            btw_inject,
        }
    }

    /// True while a `/btw` stream is still in flight.
    pub(crate) const fn is_streaming(&self) -> bool {
        matches!(
            self.view,
            BtwView::Topic {
                streaming: true,
                ..
            }
        )
    }

    /// Append a streamed text delta to the active topic's latest exchange.
    pub(crate) fn push_delta(&mut self, topic_id: &str, delta: &str) {
        if let BtwView::Topic { topic, .. } = &mut self.view
            && topic.id == topic_id
            && let Some(last) = topic.latest_mut()
        {
            last.answer.push_str(delta);
        }
    }

    /// Replace the active topic when the background `/btw` stream finishes or cancels.
    pub(crate) fn finish(&mut self, completed: BtwTopic) {
        if let BtwView::Topic {
            topic, streaming, ..
        } = &mut self.view
            && topic.id == completed.id
        {
            *topic = completed;
            *streaming = false;
        }
    }

    /// Handle a key press (`inject_chord` is true when `app.btw.inject` matched).
    pub(crate) fn key(&mut self, key: KeyEvent, inject_chord: bool) -> BtwAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return if self.is_streaming() {
                self.mark_cancelled_local();
                BtwAction::CancelStream
            } else {
                BtwAction::Close
            };
        }
        match &mut self.view {
            BtwView::History { topics, selected } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => BtwAction::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    BtwAction::Stay
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(topics.len().saturating_sub(1));
                    BtwAction::Stay
                }
                KeyCode::Enter => {
                    if let Some(topic) = topics.get(*selected).cloned() {
                        self.view = BtwView::Topic {
                            topic,
                            streaming: false,
                            follow_up: None,
                            scroll: 0,
                        };
                    }
                    BtwAction::Stay
                }
                _ => BtwAction::Stay,
            },
            BtwView::Topic { .. } => self.topic_key(key, inject_chord),
        }
    }

    fn mark_cancelled_local(&mut self) {
        if let BtwView::Topic {
            topic, streaming, ..
        } = &mut self.view
        {
            *streaming = false;
            if let Some(last) = topic.latest_mut()
                && last.status == BtwStatus::Streaming
            {
                last.status = BtwStatus::Cancelled;
            }
        }
    }

    fn topic_key(&mut self, key: KeyEvent, inject_chord: bool) -> BtwAction {
        let btw_inject = self.btw_inject;
        let BtwView::Topic {
            topic,
            streaming,
            follow_up,
            scroll,
        } = &mut self.view
        else {
            return BtwAction::Stay;
        };

        if let Some(draft) = follow_up {
            match key.code {
                KeyCode::Esc => *follow_up = None,
                KeyCode::Enter => {
                    let q = draft.trim().to_owned();
                    *follow_up = None;
                    if !q.is_empty() {
                        topic.exchanges.push(BtwExchange {
                            question: q.clone(),
                            answer: String::new(),
                            status: BtwStatus::Streaming,
                            injected: false,
                        });
                        *streaming = true;
                        return BtwAction::FollowUp {
                            topic_id: topic.id.clone(),
                            question: q,
                        };
                    }
                }
                KeyCode::Backspace => {
                    draft.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    draft.push(c);
                }
                _ => {}
            }
            return BtwAction::Stay;
        }

        if *streaming {
            if key.code == KeyCode::Esc {
                self.mark_cancelled_local();
                return BtwAction::CancelStream;
            }
            return BtwAction::Stay;
        }

        if inject_chord && btw_inject {
            if let Some(last) = topic.latest_mut()
                && let Some(payload) = extract_inject_block(&last.answer)
            {
                if last.injected {
                    return BtwAction::AlreadyInjected;
                }
                last.injected = true;
                return BtwAction::Inject {
                    topic: topic.clone(),
                    payload,
                };
            }
            return BtwAction::Stay;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => BtwAction::Close,
            KeyCode::Char('c' | 'C') => topic
                .latest()
                .map(|ex| ex.answer.clone())
                .filter(|s| !s.is_empty())
                .map_or(BtwAction::Stay, BtwAction::Copy),
            KeyCode::Char('f' | 'F') => {
                *follow_up = Some(String::new());
                BtwAction::Stay
            }
            KeyCode::Up | KeyCode::Char('k') => {
                *scroll = scroll.saturating_sub(1);
                BtwAction::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *scroll = scroll.saturating_add(1);
                BtwAction::Stay
            }
            _ => BtwAction::Stay,
        }
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 20 || area.height < 6 {
            return;
        }
        Clear.render(area, buf);
        match &self.view {
            BtwView::History { topics, selected } => {
                Self::draw_history(area, buf, topics, *selected);
            }
            BtwView::Topic {
                topic,
                streaming,
                follow_up,
                scroll,
            } => self.draw_topic(area, buf, topic, *streaming, follow_up.as_deref(), *scroll),
        }
    }

    fn draw_history(area: Rect, buf: &mut Buffer, topics: &[BtwTopic], selected: usize) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(
                format!(" /btw history ({} topics) ", topics.len()),
                theme::bold(),
            ));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height < 3 {
            return;
        }
        let rows = usize::from(inner.height.saturating_sub(2));
        let sel = selected.min(topics.len().saturating_sub(1));
        let first = (sel + 1).saturating_sub(rows);
        let width = usize::from(inner.width);
        for (i, topic) in topics.iter().enumerate().skip(first).take(rows) {
            let count = format!("({} msg) ", topic.exchanges.len());
            let room = width.saturating_sub(4 + count.len());
            let mut line = Line::from(vec![
                Span::raw(if i == sel { "▸ " } else { "  " }),
                Span::styled(count, theme::dim()),
                Span::raw(wrap::truncate(&wrap::sanitize(&topic.title), room).into_owned()),
            ]);
            if i == sel {
                line = line.style(theme::focus().add_modifier(Modifier::BOLD));
            }
            let y = inner.y + u16::try_from(i - first).unwrap_or(0);
            buf.set_line(inner.x, y, &line, inner.width);
        }
        if topics.is_empty() {
            let line = Line::from(Span::styled(
                "no /btw topics in this session yet",
                theme::dim(),
            ));
            buf.set_line(inner.x + 2, inner.y, &line, inner.width.saturating_sub(2));
        }
        let footer = Line::from(vec![
            Span::styled("enter", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" open  ", theme::muted()),
            Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" close", theme::muted()),
        ]);
        buf.set_line(inner.x, inner.bottom() - 1, &footer, inner.width);
    }

    fn draw_topic(
        &self,
        area: Rect,
        buf: &mut Buffer,
        topic: &BtwTopic,
        streaming: bool,
        follow_up: Option<&str>,
        scroll: usize,
    ) {
        let badge = if streaming { " · streaming…" } else { "" };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(
                format!(" /btw: {}{badge} ", wrap::truncate(&topic.title, 48)),
                theme::bold(),
            ));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height < 4 {
            return;
        }
        let width = usize::from(inner.width);
        let mut lines: Vec<Line<'static>> = Vec::new();
        for ex in &topic.exchanges {
            lines.extend(wrap::wrap_text(
                &wrap::sanitize(&ex.question),
                theme::accent().add_modifier(Modifier::BOLD),
                width,
                &[Span::styled("Q: ", theme::accent())],
                &[],
            ));
            let body = if ex.answer.is_empty() && streaming {
                "…".to_owned()
            } else {
                wrap::sanitize(&ex.answer).into_owned()
            };
            for raw_line in body.lines() {
                lines.extend(wrap::wrap_text(raw_line, theme::plain(), width, &[], &[]));
            }
            match &ex.status {
                BtwStatus::Cancelled => {
                    lines.extend(wrap::wrap_text(
                        "[cancelled · partial kept]",
                        theme::fg(theme::WARNING),
                        width,
                        &[],
                        &[],
                    ));
                }
                BtwStatus::Error(err) => {
                    lines.extend(wrap::wrap_text(
                        &wrap::sanitize(&format!("[error: {err}]")),
                        theme::fg(theme::ERROR),
                        width,
                        &[],
                        &[],
                    ));
                }
                BtwStatus::Streaming | BtwStatus::Done => {}
            }
            if self.btw_inject
                && let Some(payload) = extract_inject_block(&ex.answer)
            {
                let tag = if ex.injected {
                    "│ inject (sent): "
                } else {
                    "│ inject (Alt+I): "
                };
                lines.extend(wrap::wrap_text(
                    &wrap::sanitize(&payload),
                    theme::fg(theme::GOLD),
                    width,
                    &[Span::styled(
                        tag,
                        theme::fg(theme::GOLD).add_modifier(Modifier::BOLD),
                    )],
                    &[],
                ));
            }
            lines.push(Line::default());
        }

        let body_rows = usize::from(inner.height.saturating_sub(2));
        let max_scroll = lines.len().saturating_sub(body_rows);
        let offset = scroll.min(max_scroll);
        for (i, line) in lines.iter().skip(offset).take(body_rows).enumerate() {
            let y = inner.y + u16::try_from(i).unwrap_or(0);
            buf.set_line(inner.x, y, line, inner.width);
        }

        let footer = self.topic_footer(topic, streaming, follow_up);
        buf.set_line(inner.x, inner.bottom() - 1, &footer, inner.width);
    }

    fn topic_footer(
        &self,
        topic: &BtwTopic,
        streaming: bool,
        follow_up: Option<&str>,
    ) -> Line<'static> {
        if let Some(draft) = follow_up {
            Line::from(vec![
                Span::styled("follow-up › ", theme::accent().add_modifier(Modifier::BOLD)),
                Span::raw(draft.to_owned()),
            ])
        } else if streaming {
            Line::from(vec![
                Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" cancel (keeps partial)", theme::muted()),
            ])
        } else {
            let mut spans = vec![
                Span::styled("c", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" copy  ", theme::muted()),
                Span::styled("f", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" follow-up  ", theme::muted()),
            ];
            if self.btw_inject
                && topic
                    .latest()
                    .is_some_and(|ex| !ex.injected && extract_inject_block(&ex.answer).is_some())
            {
                spans.push(Span::styled(
                    "alt+i",
                    theme::fg(theme::GOLD).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(" inject  ", theme::muted()));
            }
            spans.push(Span::styled(
                "esc",
                theme::accent().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(" close", theme::muted()));
            Line::from(spans)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_topic(answer: &str) -> BtwTopic {
        BtwTopic {
            id: "btw_1".into(),
            title: "check config".into(),
            exchanges: vec![BtwExchange {
                question: "check config".into(),
                answer: answer.into(),
                status: BtwStatus::Done,
                injected: false,
            }],
            updated_at_ms: 10,
        }
    }

    #[test]
    fn btw_esc_cancels_keeping_partial_and_inject_sends_once() {
        let mut streaming = BtwPanel::streaming(
            BtwTopic {
                id: "btw_s".into(),
                title: "q".into(),
                exchanges: vec![BtwExchange {
                    question: "q".into(),
                    answer: "partial ".into(),
                    status: BtwStatus::Streaming,
                    injected: false,
                }],
                updated_at_ms: 1,
            },
            true,
        );
        streaming.push_delta("btw_s", "text");
        assert!(matches!(
            streaming.key(KeyEvent::from(KeyCode::Esc), false),
            BtwAction::CancelStream
        ));
        assert!(!streaming.is_streaming());
        assert!(matches!(
            streaming.key(KeyEvent::from(KeyCode::Char('c')), false),
            BtwAction::Copy(text) if text == "partial text"
        ));

        let mut panel = BtwPanel {
            view: BtwView::Topic {
                topic: sample_topic("Answer <inject>run cargo test</inject>"),
                streaming: false,
                follow_up: None,
                scroll: 0,
            },
            btw_inject: true,
        };
        assert!(matches!(
            panel.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::ALT), true),
            BtwAction::Inject { payload, .. } if payload == "run cargo test"
        ));
        assert!(matches!(
            panel.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::ALT), true),
            BtwAction::AlreadyInjected
        ));
    }
}
