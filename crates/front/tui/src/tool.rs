//! Tool cards in a rounded frame (`card`): one renderer per tool family (`shell`, `files`), a
//! generic card for the rest and the todo checklist drawn from the call's own arguments.
//! Cards are collapsed to a few rows; `ctrl+o` (or a click) shows up to [`EXPANDED_CAP`].

mod files;
mod shell;

use crate::card::{Frame, Section};
use crate::render::{self, Ctx};
use crate::theme;
use crate::transcript::{ToolCard, ToolState};
use crate::wrap;
use hrn_kernel::TodoStatus;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::time::Duration;

/// Rows shown under an expanded card.
const EXPANDED_CAP: usize = 400;
/// Output rows of a collapsed generic card.
const COLLAPSED_OUTPUT: usize = 3;
/// The tools that get a nicer title than their raw name.
const TITLES: [(&str, &str); 3] = [
    ("web_fetch", "Fetch"),
    ("web_search", "Search"),
    ("glob", "Find"),
];

/// Paint the card for `card`.
pub(crate) fn render(card: &ToolCard, expanded: bool, ctx: &Ctx) -> Vec<Line<'static>> {
    let cx = Cx {
        card,
        args: serde_json::from_str(&card.arguments).unwrap_or(Value::Null),
        expanded: expanded || ctx.verbose,
        ctx,
    };
    match card.name.as_str() {
        "bash" => shell::render(&cx),
        "read" => files::read(&cx),
        "write" => files::write(&cx),
        "edit" | "apply_patch" => files::edit(&cx),
        "grep" => files::grep(&cx),
        "todo" => todo(&cx),
        _ => generic(&cx),
    }
}

/// What a renderer needs about its call.
struct Cx<'a> {
    card: &'a ToolCard,
    args: Value,
    expanded: bool,
    ctx: &'a Ctx,
}

impl Cx<'_> {
    fn arg(&self, key: &str) -> &str {
        self.args
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    fn state(&self) -> ToolState {
        self.card.state
    }

    /// Rows a collapsed card shows, or the cap once expanded.
    fn cap(&self, collapsed: usize) -> usize {
        if self.expanded {
            EXPANDED_CAP
        } else {
            collapsed
        }
    }

    fn border(&self) -> Color {
        match self.state() {
            ToolState::Running | ToolState::Awaiting => theme::ACCENT,
            ToolState::Failed => theme::ERROR,
            ToolState::Denied => theme::WARNING,
            ToolState::Done | ToolState::Cancelled => theme::DIM,
        }
    }

    /// The frame (no forced background; the terminal theme shows through).
    fn frame(&self, pad: usize) -> Frame {
        Frame {
            width: self.ctx.width,
            border: self.border(),
            bg: None,
            pad,
        }
    }

    /// The state glyph; a settled card shows `identity` (its tool's own glyph) instead of `✔`.
    fn icon(&self, identity: Option<(&'static str, Color)>) -> Span<'static> {
        match self.state() {
            ToolState::Running => {
                Span::styled(render::spinner(self.ctx.frame).to_string(), theme::accent())
            }
            ToolState::Awaiting => Span::styled("…", theme::fg(theme::WARNING)),
            ToolState::Done => match identity {
                Some((glyph, color)) => Span::styled(glyph, theme::fg(color)),
                None => Span::styled("✔", theme::fg(theme::SUCCESS)),
            },
            ToolState::Failed => Span::styled("✘", theme::fg(theme::ERROR)),
            ToolState::Denied => Span::styled("x", theme::fg(theme::WARNING)),
            ToolState::Cancelled => Span::styled("x", theme::dim()),
        }
    }

    /// `icon Title: description  meta`.
    fn header(
        icon: Span<'static>,
        title: &str,
        desc: &str,
        meta: Vec<Span<'static>>,
    ) -> Vec<Span<'static>> {
        let mut spans = vec![
            icon,
            Span::raw(" "),
            Span::styled(title.to_owned(), theme::plain()),
        ];
        if !desc.is_empty() {
            spans.push(Span::styled(": ", theme::muted()));
            spans.push(Span::styled(
                wrap::sanitize(desc).into_owned(),
                theme::muted(),
            ));
        }
        if !meta.is_empty() {
            spans.push(Span::raw(" "));
            spans.extend(meta);
        }
        spans
    }

    /// Wall time of a running or settled call.
    fn wall(&self) -> Option<Duration> {
        match self.state() {
            ToolState::Running | ToolState::Awaiting => self
                .card
                .started
                .map(|s| self.ctx.now.saturating_duration_since(s)),
            _ => self.card.elapsed,
        }
    }

    /// The bottom-bar label every tool shares: progress, approval and cancellation.
    fn footer(&self) -> Vec<Span<'static>> {
        let wall = self.wall().filter(|d| d.as_secs() >= 1);
        match self.state() {
            ToolState::Running => wall
                .map(|d| {
                    vec![Span::styled(
                        format!("elapsed {}s", d.as_secs()),
                        theme::dim(),
                    )]
                })
                .unwrap_or_default(),
            ToolState::Awaiting => {
                vec![Span::styled("awaiting approval", theme::fg(theme::WARNING))]
            }
            ToolState::Denied => vec![Span::styled("denied", theme::fg(theme::WARNING))],
            ToolState::Cancelled => vec![Span::styled("cancelled", theme::dim())],
            ToolState::Done => wall
                .map(|d| vec![Span::styled(short(d), theme::dim())])
                .unwrap_or_default(),
            ToolState::Failed => Vec::new(),
        }
    }
}

/// `96ms`, `1.2s`, `2m`, `2m5s`.
fn short(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0 => format!("{}ms", d.as_millis()),
        1..60 => format!("{:.1}s", d.as_secs_f64()),
        _ if secs.is_multiple_of(60) => format!("{}m", secs / 60),
        _ => format!("{}m{}s", secs / 60, secs % 60),
    }
}

/// The dim `⟦ctrl+o: Expand⟧` that closes a trimmed body.
fn expand_hint() -> Span<'static> {
    Span::styled("⟦ctrl+o: Expand⟧", theme::dim())
}

/// A dim `… {what}` row ending in the expand hint.
fn more(what: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("… {what} "), theme::dim()),
        expand_hint(),
    ])
}

/// `text` as wrapped rows of one style, sanitized and without trailing blank lines.
fn text_rows(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let text = wrap::sanitize(text);
    text.trim_end()
        .lines()
        .flat_map(|line| wrap::wrap_text(line, style, width, &[], &[]))
        .collect()
}

/// The generic card: `✔ Title: key` and a few output rows.
fn generic(cx: &Cx<'_>) -> Vec<Line<'static>> {
    let card = cx.card;
    let title = TITLES
        .iter()
        .find(|(name, _)| *name == card.name)
        .map_or(card.name.as_str(), |(_, title)| *title);
    let frame = cx.frame(1);
    let header = Cx::header(cx.icon(None), title, &card.key, Vec::new());
    let live = matches!(cx.state(), ToolState::Running | ToolState::Awaiting);
    let (text, style) = match cx.state() {
        _ if live => (card.live.as_str(), theme::muted()),
        ToolState::Done => (card.output.as_str(), theme::muted()),
        ToolState::Failed => (card.output.as_str(), theme::fg(theme::ERROR)),
        _ => ("", theme::muted()),
    };
    let all = text_rows(text, style, frame.inner());
    let cap = cx.cap(COLLAPSED_OUTPUT);
    let hidden = all.len().saturating_sub(cap);
    let mut rows: Vec<Line<'static>> = if live {
        all.into_iter().skip(hidden).collect()
    } else {
        all.into_iter().take(cap).collect()
    };
    if hidden > 0 {
        let marker = more(&format!("{hidden} more lines"));
        if live {
            rows.insert(0, marker);
        } else {
            rows.push(marker);
        }
    }
    let sections = if rows.is_empty() {
        Vec::new()
    } else {
        vec![Section::plain(rows)]
    };
    frame.render(header, sections, cx.footer())
}

/// The checklist a `todo` call draws: the `init` payload as a phase tree; every other op (and
/// resumed sessions, where the arguments are all the card has) falls back to the generic card,
/// whose result line already shows the summary.
fn todo(cx: &Cx<'_>) -> Vec<Line<'static>> {
    let view = hrn_kernel::todo::TodoCall {
        op: hrn_kernel::todo::TodoOp::View,
        inferred: false,
    };
    let call = hrn_kernel::todo::parse_call(&cx.args).unwrap_or(view);
    let hrn_kernel::todo::TodoOp::Init { list } = &call.op else {
        return generic(cx);
    };
    let frame = cx.frame(1);
    let total: usize = list.iter().map(|p| p.tasks.len()).sum();
    let closed = list
        .iter()
        .flat_map(|p| &p.tasks)
        .filter(|t| t.status.closed())
        .count();
    let meta = vec![Span::styled(format!("{closed}/{total}"), theme::dim())];
    let header = Cx::header(cx.icon(Some(("☑", theme::SUCCESS))), "Todo", "", meta);
    let multi = list.len() > 1;
    let mut rows = Vec::new();
    for phase in list {
        if multi && !phase.tasks.is_empty() {
            rows.push(Line::from(Span::styled(
                format!("{}:", phase.name),
                theme::dim().add_modifier(Modifier::BOLD),
            )));
        }
        for item in &phase.tasks {
            let style = if item.status.closed() {
                theme::muted()
            } else if item.status == TodoStatus::InProgress {
                theme::accent().add_modifier(Modifier::BOLD)
            } else {
                theme::plain()
            };
            let first = [Span::styled(
                format!("{} ", item.status.marker()),
                theme::accent(),
            )];
            let rest = [Span::raw("  ")];
            let text = wrap::sanitize(&item.content);
            rows.extend(wrap::wrap_text(&text, style, frame.inner(), &first, &rest));
        }
    }
    frame.render(header, vec![Section::plain(rows)], cx.footer())
}
