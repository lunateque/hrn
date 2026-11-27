//! Paints one transcript entry into lines, plus the small formatters (durations, token counts)
//! the status line shares. Prose and thinking sit in a one-column gutter, the user's message is
//! a full-width surface block, tool cards (see `tool`) start at column 0.

use crate::imgview;
use crate::markdown;
use crate::symbols;
use crate::theme;
use crate::tool;
use crate::transcript::{Entry, Level, SteerState};
use crate::wrap;
use hrn_kernel::ImageAttachment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::{Duration, Instant};

/// Animation ticks per pulse frame.
pub(crate) const PULSE_EVERY: usize = 3;

/// Everything a frame's entry rendering depends on.
pub(crate) struct Ctx {
    pub(crate) width: usize,
    pub(crate) verbose: bool,
    pub(crate) frame: usize,
    pub(crate) now: Instant,
}

pub(crate) fn spinner(frame: usize) -> &'static str {
    symbols::current().spinner_frame(frame)
}

/// `0.4s`, `12.4s`, `3m 05s`, `1h 02m`.
pub(crate) fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..60 => format!("{secs}.{}s", d.subsec_millis() / 100),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// `456`, `12.3k`, `1.2M`.
pub(crate) fn tokens(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{}.{}k", n / 1000, n % 1000 / 100),
        _ => format!("{}.{}M", n / 1_000_000, n % 1_000_000 / 100_000),
    }
}

/// `$12.34`; under ten cents a third decimal keeps small spends visible (`$0.004`).
pub(crate) fn money(micros: u64) -> String {
    if micros >= 100_000 {
        let cents = micros.saturating_add(5_000) / 10_000;
        format!("${}.{:02}", cents / 100, cents % 100)
    } else {
        let mills = micros.saturating_add(500) / 1000;
        format!("${}.{:03}", mills / 1000, mills % 1000)
    }
}

fn indent(n: usize) -> Span<'static> {
    Span::raw(" ".repeat(n))
}

pub(crate) fn item(
    entry: &Entry,
    images: &[(Option<usize>, ImageAttachment)],
    expanded: bool,
    ctx: &Ctx,
) -> Vec<Line<'static>> {
    let mut lines = self::entry(entry, expanded, ctx);
    if !images.is_empty() {
        let max_rows = if expanded { 14 } else { 8 };
        lines.extend(imgview::render_gallery(images, ctx.width, max_rows));
    }
    lines
}

pub(crate) fn entry(entry: &Entry, expanded: bool, ctx: &Ctx) -> Vec<Line<'static>> {
    match entry {
        Entry::User(text) => bubble(text, Style::new(), None, None, ctx),
        Entry::Steer { text, state } => steer(text, *state, ctx),
        Entry::Assistant(text) => gutter(markdown::render(
            &wrap::sanitize(text),
            ctx.width.saturating_sub(2),
            Style::new(),
        )),
        Entry::Reasoning {
            text,
            started,
            elapsed,
            live,
        } => reasoning(text, started.filter(|_| *live), *elapsed, expanded, ctx),
        Entry::Tool(card) => tool::render(card, expanded, ctx),
        Entry::Compacted {
            method,
            summary,
            saved_chars,
        } => compacted(method, summary, *saved_chars, expanded, ctx),
        Entry::Notice(level, text) => {
            let syms = symbols::current();
            let (mark, style) = match level {
                Level::Info => (syms.notice_info.as_str(), theme::muted()),
                Level::Warn => (syms.notice_warn.as_str(), theme::fg(theme::WARNING)),
                Level::Error => (syms.notice_error.as_str(), theme::fg(theme::ERROR)),
            };
            let first = [indent(1), Span::styled(mark.to_owned(), style)];
            wrap::wrap_text(
                &wrap::sanitize(text),
                style,
                ctx.width,
                &first,
                &[indent(3)],
            )
        }
        Entry::Footer(text) => vec![Line::from(vec![
            indent(1),
            Span::styled(text.clone(), theme::dim()),
        ])],
    }
}

/// One column of left gutter in front of prose.
fn gutter(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|mut line| {
            line.spans.insert(0, indent(1));
            line
        })
        .collect()
}

/// A line on the surface background: one column of padding each side, filled to `width`.
fn surface_row(line: Line<'static>, width: usize) -> Line<'static> {
    let mut spans = vec![indent(1)];
    spans.extend(line.spans);
    let mut row = Line::from(spans);
    wrap::pad(&mut row, width, theme::surface());
    row
}

/// The user's message: markdown on a full-width surface block with a blank row above and below.
/// `badge` takes the top-left of the first blank row (steering), `note` closes the block.
fn bubble(
    text: &str,
    style: Style,
    badge: Option<Span<'static>>,
    note: Option<Span<'static>>,
    ctx: &Ctx,
) -> Vec<Line<'static>> {
    let blank = || surface_row(Line::default(), ctx.width);
    let mut top = blank();
    if let Some(badge) = badge {
        top = surface_row(Line::from(badge), ctx.width);
    }
    let mut lines = vec![top];
    let body = markdown::render(&wrap::sanitize(text), ctx.width.saturating_sub(2), style);
    lines.extend(body.into_iter().map(|line| surface_row(line, ctx.width)));
    if let Some(note) = note {
        lines.push(surface_row(Line::from(note), ctx.width));
    }
    lines.push(blank());
    lines
}

fn steer(text: &str, state: SteerState, ctx: &Ctx) -> Vec<Line<'static>> {
    let badge = Span::styled("*", theme::accent().add_modifier(Modifier::BOLD));
    let (style, note) = match state {
        SteerState::Queued => (Style::new(), Some("queued for the next step")),
        SteerState::Sent => (Style::new(), None),
        SteerState::Returned => (
            theme::dim().add_modifier(Modifier::CROSSED_OUT),
            Some("returned to the input"),
        ),
    };
    let note = note.map(|n| Span::styled(n, theme::dim()));
    bubble(text, style, Some(badge), note, ctx)
}

/// The thinking text itself (expanded), a pulse while it streams collapsed, or the time it took.
fn reasoning(
    text: &str,
    live_since: Option<Instant>,
    elapsed: Option<Duration>,
    expanded: bool,
    ctx: &Ctx,
) -> Vec<Line<'static>> {
    let style = theme::reasoning();
    if expanded {
        let body = wrap::sanitize(text.trim());
        return gutter(markdown::render(&body, ctx.width.saturating_sub(2), style));
    }
    if live_since.is_some() {
        let syms = symbols::current();
        let glyph = syms.pulse_frame(ctx.frame / PULSE_EVERY);
        let approx = text.chars().count() / 4;
        return vec![Line::from(vec![
            indent(1),
            Span::styled(glyph.to_owned(), theme::muted()),
            Span::styled(" Thinking", theme::muted()),
            Span::styled(
                format!(" {} ~{} tokens", syms.dot, tokens(approx as u64)),
                theme::dim(),
            ),
        ])];
    }
    let label = elapsed.map_or_else(
        || "Thought".to_owned(),
        |d| format!("Thought for {}", duration(d)),
    );
    vec![Line::from(vec![indent(1), Span::styled(label, style)])]
}

/// `── compacted ──` divider; pressing `Ctrl+O` (`expanded == true`) reveals the summary body.
fn compacted(
    method: &str,
    summary: &str,
    saved_chars: usize,
    expanded: bool,
    ctx: &Ctx,
) -> Vec<Line<'static>> {
    let syms = symbols::current();
    let tag = if method.is_empty() { "summary" } else { method };
    let meta = if saved_chars > 0 {
        format!("{tag} {} {saved_chars} chars", syms.dot)
    } else {
        tag.to_owned()
    };
    let div = &syms.divider;
    let label = if expanded {
        format!("{div} compacted {div} ({meta})")
    } else {
        format!("{div} compacted {div} ({meta} {} ctrl+o)", syms.dot)
    };
    let mut lines = vec![Line::from(vec![
        indent(1),
        Span::styled(label, theme::muted()),
    ])];
    if expanded {
        let body = if summary.trim().is_empty() {
            "earlier conversation compacted".to_owned()
        } else {
            wrap::sanitize(summary.trim()).into_owned()
        };
        lines.extend(gutter(markdown::render(
            &body,
            ctx.width.saturating_sub(2),
            theme::dim(),
        )));
    }
    lines
}
