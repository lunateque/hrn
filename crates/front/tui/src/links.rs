//! `Alt+O` (`app.links.hint`): OSC-8 hyperlink and URL extractor plus hint picker overlay for
//! links visible in the current transcript.

use crate::theme;
use crate::transcript::{Entry, Transcript};
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};
use std::collections::HashSet;
use std::process::Command;

const HINT_CHARS: &str = "asdfghjklqwertyuiopzxcvbnm1234567890";

/// One discovered hyperlink in the transcript with its assigned single-char hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HintedLink {
    pub(crate) hint: char,
    pub(crate) url: String,
    pub(crate) label: Option<String>,
}

/// Outcome of routing a key to [`LinkHintsOverlay`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LinkAction {
    Stay,
    Close,
    Open(String),
    Copy(String),
}

/// Extract unique OSC-8, Markdown `[label](url)`, and bare URLs from `transcript` (newest first).
pub(crate) fn extract_transcript_links(transcript: &Transcript) -> Vec<HintedLink> {
    let mut raw: Vec<(String, Option<String>)> = Vec::new();
    for i in (0..transcript.len()).rev() {
        let Some(entry) = transcript.entry(i) else {
            continue;
        };
        match entry {
            Entry::Assistant(text)
            | Entry::User(text)
            | Entry::Steer { text, .. }
            | Entry::Notice(_, text)
            | Entry::Reasoning { text, .. } => extract_from_text(text, &mut raw),
            Entry::Compacted { summary, .. } => extract_from_text(summary, &mut raw),
            Entry::ToolGroup(group) => {
                for card in group.cards.iter().rev() {
                    extract_from_text(&card.output, &mut raw);
                    extract_from_text(&card.live, &mut raw);
                }
            }
            Entry::Footer(_) => {}
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (url, label) in raw {
        if !seen.insert(url.clone()) {
            continue;
        }
        let Some(hint) = HINT_CHARS.chars().nth(out.len()) else {
            break;
        };
        out.push(HintedLink { hint, url, label });
    }
    out
}

fn extract_from_text(text: &str, out: &mut Vec<(String, Option<String>)>) {
    extract_osc8(text, out);
    extract_markdown_links(text, out);
    extract_bare_urls(text, out);
}

fn extract_osc8(text: &str, out: &mut Vec<(String, Option<String>)>) {
    let mut rest = text;
    while let Some(pos) = rest.find("\x1b]8;") {
        let after = rest.get(pos + 4..).unwrap_or_default();
        let Some(semi) = after.find(';') else {
            break;
        };
        let url_part = after.get(semi + 1..).unwrap_or_default();
        let (url, tail) = if let Some(bel) = url_part.find('\x07') {
            (
                url_part.get(..bel).unwrap_or_default(),
                url_part.get(bel + 1..).unwrap_or_default(),
            )
        } else if let Some(st) = url_part.find("\x1b\\") {
            (
                url_part.get(..st).unwrap_or_default(),
                url_part.get(st + 2..).unwrap_or_default(),
            )
        } else {
            break;
        };
        let url = url.trim();
        let label = tail
            .find("\x1b]8;")
            .and_then(|end| tail.get(..end))
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != url)
            .map(str::to_owned);
        if is_supported_url(url) {
            out.push((url.to_owned(), label));
        }
        rest = tail;
    }
}

fn extract_markdown_links(text: &str, out: &mut Vec<(String, Option<String>)>) {
    let mut rest = text;
    while let Some(open_b) = rest.find('[') {
        let after_b = rest.get(open_b + 1..).unwrap_or_default();
        let Some(mid) = after_b.find("](") else {
            rest = after_b;
            continue;
        };
        let label = after_b.get(..mid).unwrap_or_default();
        let after_p = after_b.get(mid + 2..).unwrap_or_default();
        let Some(close_p) = after_p.find(')') else {
            rest = after_p;
            continue;
        };
        let url = after_p.get(..close_p).unwrap_or_default().trim();
        if !label.contains('\n') && !url.contains(char::is_whitespace) && is_supported_url(url) {
            let lbl =
                (!label.trim().is_empty() && label.trim() != url).then(|| label.trim().to_owned());
            out.push((url.to_owned(), lbl));
        }
        rest = after_p.get(close_p + 1..).unwrap_or_default();
    }
}

fn extract_bare_urls(text: &str, out: &mut Vec<(String, Option<String>)>) {
    for raw_tok in text.split_whitespace() {
        let tok = raw_tok.trim_start_matches(['(', '[', '<', '"', '\'', '`']);
        for scheme in ["https://", "http://", "file://", "local://"] {
            if let Some(pos) = tok.find(scheme) {
                let candidate = tok
                    .get(pos..)
                    .unwrap_or_default()
                    .trim_end_matches(['.', ',', ';', ':', ')', ']', '>', '"', '\'', '`', '!']);
                if candidate.len() > scheme.len() && is_supported_url(candidate) {
                    out.push((candidate.to_owned(), None));
                }
            }
        }
    }
}

fn is_supported_url(url: &str) -> bool {
    !url.is_empty()
        && !url.chars().any(char::is_control)
        && (url.starts_with("https://")
            || url.starts_with("http://")
            || url.starts_with("file://")
            || url.starts_with("local://")
            || url.starts_with("mailto:"))
}

/// Open `url` using the platform's default handler.
pub(crate) fn open_url(url: &str) -> Result<(), String> {
    if !is_supported_url(url) {
        return Err(format!("unsupported URL `{url}`"));
    }
    #[cfg(windows)]
    let res = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let res = Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let res = Command::new("xdg-open").arg(url).spawn();
    res.map(drop)
        .map_err(|e| format!("cannot open `{url}`: {e}"))
}

/// Full-window overlay presenting hinted transcript links.
pub(crate) struct LinkHintsOverlay {
    links: Vec<HintedLink>,
    selected: usize,
}

impl LinkHintsOverlay {
    pub(crate) fn new(links: Vec<HintedLink>) -> Self {
        Self { links, selected: 0 }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> LinkAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return LinkAction::Close;
        }
        match key.code {
            KeyCode::Esc => LinkAction::Close,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                LinkAction::Stay
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.links.len().saturating_sub(1));
                LinkAction::Stay
            }
            KeyCode::Enter => self
                .links
                .get(self.selected)
                .map_or(LinkAction::Close, |l| {
                    if shift {
                        LinkAction::Copy(l.url.clone())
                    } else {
                        LinkAction::Open(l.url.clone())
                    }
                }),
            KeyCode::Char(c) if !ctrl => {
                let low = c.to_ascii_lowercase();
                if let Some(link) = self.links.iter().find(|l| l.hint == low) {
                    if shift || c.is_ascii_uppercase() {
                        LinkAction::Copy(link.url.clone())
                    } else {
                        LinkAction::Open(link.url.clone())
                    }
                } else {
                    LinkAction::Stay
                }
            }
            _ => LinkAction::Stay,
        }
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 16 || area.height < 5 {
            return;
        }
        Clear.render(area, buf);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(
                format!(" Links ({}) ", self.links.len()),
                theme::bold(),
            ))
            .title_bottom(Span::styled(
                " [key]/enter open | Shift+[key] copy | esc close ",
                theme::muted(),
            ));
        let inner = block.inner(area);
        block.render(area, buf);
        let rows = usize::from(inner.height);
        let first = (self.selected + 1).saturating_sub(rows);
        for (i, link) in self.links.iter().enumerate().skip(first).take(rows) {
            let on = i == self.selected;
            let badge = format!("[{}] ", link.hint);
            let label = link
                .label
                .as_deref()
                .map(|l| format!("{l} - "))
                .unwrap_or_default();
            let room = usize::from(inner.width).saturating_sub(badge.len() + label.len() + 2);
            let mut spans = vec![
                Span::raw(if on { "> " } else { "  " }),
                Span::styled(badge, theme::accent().add_modifier(Modifier::BOLD)),
            ];
            if !label.is_empty() {
                spans.push(Span::styled(label, theme::plain()));
            }
            spans.push(Span::styled(
                wrap::truncate(&link.url, room).into_owned(),
                theme::link_url(),
            ));
            let mut line = Line::from(spans);
            if on {
                line = line.style(theme::focus().add_modifier(Modifier::BOLD));
            }
            let y = inner.y + u16::try_from(i - first).unwrap_or(0);
            buf.set_line(inner.x, y, &line, inner.width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_osc8_markdown_and_bare_links_deduplicated() {
        let mut t = Transcript::default();
        t.push(Entry::Assistant(
            "See [Docs](https://docs.example.com/guide) and \x1b]8;;https://osc8.example.com\x07OSC link\x1b]8;;\x07 or local://1.log and https://docs.example.com/guide."
                .into(),
        ));
        let links = extract_transcript_links(&t);
        let urls: Vec<&str> = links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://osc8.example.com",
                "https://docs.example.com/guide",
                "local://1.log",
            ]
        );
        assert_eq!(links[0].hint, 'a');
        assert_eq!(links[0].label.as_deref(), Some("OSC link"));
        assert_eq!(links[1].hint, 's');
        assert_eq!(links[1].label.as_deref(), Some("Docs"));
        assert_eq!(links[2].hint, 'd');
    }
}
