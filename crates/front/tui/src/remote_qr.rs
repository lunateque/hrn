//! Full-window QR pairing overlay for `/remote` (LAN/Tailscale PWA and E2E Relay).

use crate::port::RemoteSessionView;
use crate::theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};

/// User action produced by a key press inside [`RemoteQrOverlay`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteQrAction {
    /// Keep the overlay open with no backend action.
    Stay,
    /// Hide the overlay while leaving the remote server running in the background.
    Close,
    /// Rotate the single-use pairing QR (`true` = control, `false` = view-only).
    Rotate(bool),
    /// Revoke paired sessions and stop the remote server.
    Stop,
}

/// Interactive `/remote` modal rendering a camera-scannable dark-on-light QR code and URLs.
pub(crate) struct RemoteQrOverlay {
    view: RemoteSessionView,
}

impl RemoteQrOverlay {
    pub(crate) fn new(view: RemoteSessionView) -> Self {
        Self { view }
    }

    pub(crate) fn set_view(&mut self, view: RemoteSessionView) {
        self.view = view;
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> RemoteQrAction {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return RemoteQrAction::Stay;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | 'Q') => RemoteQrAction::Close,
            KeyCode::Char('r' | 'R') => RemoteQrAction::Rotate(self.view.can_control),
            KeyCode::Char('v' | 'V') => RemoteQrAction::Rotate(false),
            KeyCode::Char('c' | 'C') => RemoteQrAction::Rotate(true),
            KeyCode::Char('s' | 'S') => RemoteQrAction::Stop,
            _ => RemoteQrAction::Stay,
        }
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 24 || area.height < 8 {
            return;
        }
        let qr_w = self
            .view
            .qr_lines
            .iter()
            .map(|l| wrap::str_width(l))
            .max()
            .unwrap_or(0);
        let qr_h = self.view.qr_lines.len();
        let alt_count = self.view.alt_urls.len().min(2);
        let max_card_w = usize::from(area.width.saturating_sub(2)).min(96);
        let desired_w = qr_w
            .max(self.view.url.len().min(78) + 6)
            .max(70)
            .saturating_add(4);
        let card_w = u16::try_from(desired_w.min(max_card_w)).unwrap_or(area.width);
        let inner_w = usize::from(card_w.saturating_sub(4));
        let url_lines = wrap_url_lines("URL  ", &self.view.url, inner_w, 2);
        let chrome_h = 6 + url_lines.len() + alt_count;
        let max_card_h = usize::from(area.height);
        let fits_qr = !self.view.qr_lines.is_empty()
            && qr_w <= inner_w
            && chrome_h.saturating_add(qr_h) <= max_card_h;

        let body_h = if fits_qr { qr_h } else { 3 };
        let card_h = u16::try_from((chrome_h + body_h).min(max_card_h)).unwrap_or(area.height);
        let x = area.x + area.width.saturating_sub(card_w) / 2;
        let y = area.y + area.height.saturating_sub(card_h) / 2;
        let card = Rect::new(x, y, card_w, card_h);

        Clear.render(card, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(theme::border())
            .style(Style::default())
            .title(Line::from(vec![
                Span::raw(" "),
                Span::styled("hrn remote", theme::accent().add_modifier(Modifier::BOLD)),
                Span::raw(" "),
            ]));
        block.render(card, buf);

        let inner = Rect::new(
            card.x + 2,
            card.y + 1,
            card.width.saturating_sub(4),
            card.height.saturating_sub(2),
        );
        if inner.width > 0 && inner.height > 0 {
            self.draw_body(inner, buf, qr_w, chrome_h + qr_h, fits_qr, &url_lines);
        }
    }

    fn draw_body(
        &self,
        inner: Rect,
        buf: &mut Buffer,
        qr_w: usize,
        need_h: usize,
        fits_qr: bool,
        url_lines: &[String],
    ) {
        let inner_w = usize::from(inner.width);
        let mut row = inner.y;
        let bottom = inner.bottom();

        let role_style = if self.view.can_control {
            theme::fg(theme::SUCCESS).add_modifier(Modifier::BOLD)
        } else {
            theme::fg(theme::GOLD).add_modifier(Modifier::BOLD)
        };
        let devices = format!(
            "{} device{}",
            self.view.connected,
            if self.view.connected == 1 { "" } else { "s" }
        );
        let header = Line::from(vec![
            Span::styled(&self.view.transport_label, theme::accent()),
            Span::styled(" · ", theme::dim()),
            Span::styled(&self.view.mode_label, role_style),
            Span::styled(" · ", theme::dim()),
            Span::styled(devices, theme::muted()),
        ]);
        buf.set_line(inner.x, row, &header, inner.width);
        row = row.saturating_add(2);

        if fits_qr {
            let qr_style = Style::default().fg(Color::Black).bg(Color::White);
            let left_pad = inner_w.saturating_sub(qr_w) / 2;
            let qr_x = inner.x + u16::try_from(left_pad).unwrap_or(0);
            let qr_span_w = u16::try_from(qr_w).unwrap_or(inner.width);
            for line in &self.view.qr_lines {
                if row >= bottom.saturating_sub(1) {
                    break;
                }
                buf.set_line(
                    qr_x,
                    row,
                    &Line::from(Span::styled(line.as_str(), qr_style)),
                    qr_span_w,
                );
                row = row.saturating_add(1);
            }
        } else if row + 1 < bottom {
            let need_w = qr_w.saturating_add(6);
            let msg = Line::from(Span::styled(
                format!("Terminal too small for QR (need {need_w}×{need_h}) — open URL below:"),
                theme::fg(theme::WARNING),
            ));
            buf.set_line(inner.x, row, &msg, inner.width);
            row = row.saturating_add(2);
        }

        if row < bottom.saturating_sub(1) {
            row = row.saturating_add(1);
        }

        for (idx, part) in url_lines.iter().enumerate() {
            if row >= bottom.saturating_sub(1) {
                break;
            }
            let line = if idx == 0 {
                Line::from(vec![
                    Span::styled("URL  ", theme::muted()),
                    Span::styled(part.as_str(), theme::fg(theme::TEXT)),
                ])
            } else {
                Line::from(vec![
                    Span::raw("     "),
                    Span::styled(part.as_str(), theme::fg(theme::TEXT)),
                ])
            };
            buf.set_line(inner.x, row, &line, inner.width);
            row = row.saturating_add(1);
        }

        for alt in self.view.alt_urls.iter().take(2) {
            if row >= bottom.saturating_sub(1) {
                break;
            }
            let truncated = wrap::truncate(alt, inner_w.saturating_sub(5));
            let line = Line::from(vec![
                Span::styled("Alt  ", theme::dim()),
                Span::styled(truncated.into_owned(), theme::muted()),
            ]);
            buf.set_line(inner.x, row, &line, inner.width);
            row = row.saturating_add(1);
        }

        let footer_y = bottom.saturating_sub(1);
        let footer = Line::from(vec![
            Span::styled("r", theme::accent()),
            Span::styled(" rotate · ", theme::muted()),
            Span::styled("v", theme::accent()),
            Span::styled(" view-only · ", theme::muted()),
            Span::styled("c", theme::accent()),
            Span::styled(" control · ", theme::muted()),
            Span::styled("s", theme::accent()),
            Span::styled(" stop · ", theme::muted()),
            Span::styled("esc", theme::accent()),
            Span::styled(" hide (stays active)", theme::muted()),
        ]);
        buf.set_line(inner.x, footer_y, &footer, inner.width);
    }
}

fn wrap_url_lines(prefix: &str, url: &str, max_w: usize, max_lines: usize) -> Vec<String> {
    let avail = max_w.saturating_sub(prefix.len()).max(16);
    if url.len() <= avail {
        return vec![url.to_owned()];
    }
    let mut out = Vec::new();
    let mut rest = url;
    for idx in 0..max_lines {
        if rest.is_empty() {
            break;
        }
        if idx + 1 == max_lines {
            out.push(wrap::truncate(rest, avail).into_owned());
            break;
        }
        let split = rest
            .char_indices()
            .take_while(|(i, _)| *i < avail)
            .last()
            .map_or(rest.len(), |(i, c)| i + c.len_utf8());
        let (head, tail) = rest.split_at(split.min(rest.len()));
        out.push(head.to_owned());
        rest = tail;
    }
    out
}
