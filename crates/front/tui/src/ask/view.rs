//! Painting the ask form: context blocks, choice rows, text inputs and inline errors.

use super::AskPanel;
use crate::theme;
use crate::wrap;
use hrn_kernel::{Field, Item, Kind, Spec, TextKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Widget};
use std::time::Instant;

/// The painted form and where the focus sits in it.
struct Rows {
    lines: Vec<Line<'static>>,
    /// First row of the focused field, and the row the cursor is on.
    focus: (usize, usize),
}

fn pad() -> Span<'static> {
    Span::raw(" ")
}

impl AskPanel {
    /// The panel's natural height for `width` columns.
    pub(crate) fn height(&self, width: usize) -> usize {
        // Rows, the key hints, and the borders.
        self.rows(width.saturating_sub(2)).lines.len() + 3
    }

    /// Paint the panel inside `area`.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(
                " Question ",
                theme::accent().add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height < 2 || inner.width < 8 {
            return;
        }
        let rows = self.rows(usize::from(inner.width));
        let view = usize::from(inner.height) - 1;
        let top = self.scroll_for(&rows, view);
        for (y, line) in (inner.y..).zip(rows.lines.iter().skip(top).take(view)) {
            buf.set_line(inner.x, y, line, inner.width);
        }
        let more = rows.lines.len() > top + view;
        let hints = self.hints(top > 0, more);
        buf.set_line(inner.x, inner.bottom() - 1, &hints, inner.width);
    }

    /// The first visible row: the focused field if it fits, else the cursor's row.
    fn scroll_for(&self, rows: &Rows, view: usize) -> usize {
        let (field_top, row) = rows.focus;
        let mut top = self.scroll.get();
        if row < top || row >= top + view {
            top = (row + 1).saturating_sub(view);
        }
        if field_top < top && row + 1 - field_top <= view {
            top = field_top;
        }
        if self.focus == 0 {
            top = 0;
        }
        top = top.min(rows.lines.len().saturating_sub(view));
        self.scroll.set(top);
        top
    }

    fn hints(&self, above: bool, below: bool) -> Line<'static> {
        let mut keys = vec![("↑↓", "move"), ("space", "toggle"), ("enter", "submit")];
        if self.in_area() {
            keys.insert(2, ("alt+enter", "newline"));
        }
        keys.push(("esc", "cancel"));
        let mut spans = Vec::new();
        for (i, (key, label)) in keys.into_iter().enumerate() {
            spans.push(Span::styled(
                if i == 0 { " " } else { " · " },
                theme::hint(),
            ));
            spans.push(Span::styled(key, theme::accent()));
            spans.push(Span::styled(format!(" {label}"), theme::hint()));
        }
        if let Some(label) = self.countdown() {
            spans.push(Span::styled(format!(" · {label}"), theme::hint()));
        }
        if above || below {
            let arrows = match (above, below) {
                (true, true) => " ↕",
                (true, false) => " ↑",
                _ => " ↓",
            };
            spans.push(Span::styled(arrows, theme::accent()));
        }
        Line::from(spans)
    }

    fn countdown(&self) -> Option<String> {
        let left = self.deadline?.saturating_duration_since(Instant::now());
        Some(format!("auto-submit in {}s", left.as_secs() + 1))
    }

    fn rows(&self, inner: usize) -> Rows {
        let room = inner.saturating_sub(1);
        let mut out = Rows {
            lines: Vec::new(),
            focus: (0, 0),
        };
        let mut slot = 0;
        let mut field_at = 0;
        let mut previous: Option<&str> = None;
        for item in &self.form.items {
            match item {
                Item::Text { kind, text } => {
                    out.lines.extend(text_lines(*kind, text, room));
                    previous = Some(text);
                }
                Item::Image { alt } => {
                    let text = format!("▣ {alt}");
                    out.lines.extend(wrapped(&text, theme::dim(), room, 1));
                    previous = None;
                }
                Item::Table { header, rows } => {
                    out.lines.extend(table_lines(*header, rows, room));
                    previous = None;
                }
                Item::Field(field) => {
                    self.field_rows(field, field_at, previous, room, &mut slot, &mut out);
                    field_at += 1;
                    previous = None;
                }
            }
        }
        out
    }

    fn field_rows(
        &self,
        field: &Field,
        at: usize,
        previous: Option<&str>,
        room: usize,
        slot: &mut usize,
        out: &mut Rows,
    ) {
        if matches!(field.kind, Kind::Hidden(_)) {
            return;
        }
        let top = if field.kind != Kind::Flag && previous != Some(field.question.as_str()) {
            if !out.lines.is_empty() {
                out.lines.push(Line::default());
            }
            let required = if field.required { " *" } else { "" };
            let text = format!("{}{required}", field.question);
            let start = out.lines.len();
            out.lines.extend(wrapped(&text, theme::bold(), room, 1));
            start
        } else {
            out.lines.len()
        };
        let Some(answer) = self.answers.get(at) else {
            return;
        };
        let radio = field.kind == Kind::One;
        let mark = |on: bool| match (radio, on) {
            (true, true) => "(•) ",
            (true, false) => "( ) ",
            (false, true) => "[x] ",
            (false, false) => "[ ] ",
        };
        let place = |slot: &mut usize, out: &mut Rows, row: usize| {
            if *slot == self.focus {
                out.focus = (top, row);
            }
            *slot += 1;
        };
        match &field.kind {
            Kind::Text(spec) => {
                let focused = *slot == self.focus;
                let lines = self.input_lines(&answer.text, spec, focused, room.saturating_sub(3));
                let cursor_row = out.lines.len() + if focused { self.cursor_line() } else { 0 };
                place(slot, out, cursor_row);
                for (i, spans) in lines.into_iter().enumerate() {
                    out.lines.push(input_row(i == 0, focused, spans));
                }
            }
            Kind::One | Kind::Many | Kind::Flag => {
                for (opt, choice) in field.options.iter().enumerate() {
                    let focused = *slot == self.focus;
                    let row = out.lines.len();
                    place(slot, out, row);
                    let on = answer.picked.contains(&opt);
                    let first = [
                        pad(),
                        Span::styled(if focused { "› " } else { "  " }, theme::focus()),
                        Span::styled(mark(on), mark_style(on)),
                    ];
                    let label = wrap::sanitize(&choice.label);
                    out.lines
                        .extend(choice_lines(&label, focused, room, &first));
                }
                if field.other {
                    let focused = *slot == self.focus;
                    let row = out.lines.len() + if focused { self.cursor_line() } else { 0 };
                    place(slot, out, row);
                    let on = !answer.other.trim().is_empty();
                    let head = vec![
                        pad(),
                        Span::styled(if focused { "› " } else { "  " }, theme::focus()),
                        Span::styled(mark(on), mark_style(on)),
                        Span::styled("Other: ", theme::muted()),
                    ];
                    let spec = Spec {
                        placeholder: "type your own answer".to_owned(),
                        ..Spec::default()
                    };
                    let room = room.saturating_sub(wrap::str_width("   [x] Other: "));
                    for (i, spans) in self
                        .input_lines(&answer.other, &spec, focused, room)
                        .into_iter()
                        .enumerate()
                    {
                        let mut line = if i == 0 {
                            head.clone()
                        } else {
                            vec![Span::raw(" ".repeat(14))]
                        };
                        line.extend(spans);
                        out.lines.push(Line::from(line));
                    }
                }
            }
            Kind::Hidden(_) => {}
        }
        if let Some(Some(error)) = self.errors.get(at) {
            let text = format!("✗ {error}");
            let style = theme::fg(theme::ERROR);
            out.lines.extend(wrapped_indent(&text, style, room, 3));
        }
    }

    /// The line of the focused text the cursor is on.
    fn cursor_line(&self) -> usize {
        self.text().map_or(0, |text| {
            text.chars()
                .take(self.cursor)
                .filter(|c| *c == '\n')
                .count()
        })
    }

    /// The text of one input as styled lines: a window around the cursor when focused.
    fn input_lines(
        &self,
        text: &str,
        spec: &Spec,
        focused: bool,
        room: usize,
    ) -> Vec<Vec<Span<'static>>> {
        if text.is_empty() {
            let hint = match (spec.placeholder.as_str(), spec.input.as_str()) {
                ("", "date") => "YYYY-MM-DD",
                ("", "email") => "name@example.com",
                ("", "url") => "https://",
                (placeholder, _) => placeholder,
            };
            let mut spans = Vec::new();
            if focused {
                spans.push(Span::styled(" ", cursor_style()));
            }
            spans.push(Span::styled(hint.to_owned(), theme::dim()));
            return vec![spans];
        }
        let mut before = 0;
        text.split('\n')
            .map(|line| {
                let len = line.chars().count();
                let col = (focused && (before..=before + len).contains(&self.cursor))
                    .then(|| self.cursor - before);
                before += len + 1;
                input_spans(line, col, room)
            })
            .collect()
    }
}

fn mark_style(on: bool) -> Style {
    if on {
        theme::fg(theme::SUCCESS)
    } else {
        theme::dim()
    }
}

fn cursor_style() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// One input row: the `›` gutter on the first line, an indent after.
fn input_row(first: bool, focused: bool, spans: Vec<Span<'static>>) -> Line<'static> {
    let gutter = match (first, focused) {
        (true, true) => Span::styled("› ", theme::focus()),
        (true, false) => Span::styled("› ", theme::dim()),
        (false, _) => Span::raw("  "),
    };
    let mut line = vec![pad(), gutter];
    line.extend(spans);
    Line::from(line)
}

/// `line` windowed to `room` columns around the cursor column `col` (the head when unfocused).
fn input_spans(line: &str, col: Option<usize>, room: usize) -> Vec<Span<'static>> {
    let chars: Vec<char> = wrap::sanitize(line).chars().collect();
    let width = |cs: &[char]| cs.iter().map(|c| wrap::char_width(*c)).sum::<usize>();
    let anchor = col.unwrap_or(0).min(chars.len());
    let cursor_cell = usize::from(col.is_some());
    let (mut start, mut end) = (0, chars.len());
    if width(&chars) + cursor_cell > room {
        // Room for the cursor cell and an ellipsis at each end; most of it before the cursor.
        let budget = room.saturating_sub(2 + cursor_cell);
        let back = if anchor == chars.len() {
            budget
        } else {
            budget * 2 / 3
        };
        let (mut used, mut lo, mut hi) = (0, anchor, anchor);
        while lo > 0 {
            let w = chars.get(lo - 1).map_or(0, |c| wrap::char_width(*c));
            if used + w > back {
                break;
            }
            used += w;
            lo -= 1;
        }
        while let Some(c) = chars.get(hi) {
            let w = wrap::char_width(*c);
            if used + w > budget {
                break;
            }
            used += w;
            hi += 1;
        }
        (start, end) = (lo, hi);
    }
    let text = |from: usize, to: usize| -> String {
        chars.get(from..to).unwrap_or_default().iter().collect()
    };
    let mut spans = Vec::new();
    if start > 0 {
        spans.push(Span::styled("…", theme::dim()));
    }
    match col {
        Some(_) => {
            spans.push(Span::styled(text(start, anchor), theme::plain()));
            let under = chars.get(anchor).map_or(" ".to_owned(), char::to_string);
            spans.push(Span::styled(under, cursor_style()));
            spans.push(Span::styled(
                text((anchor + 1).min(end), end),
                theme::plain(),
            ));
        }
        None => spans.push(Span::styled(text(start, end), theme::plain())),
    }
    if end < chars.len() {
        spans.push(Span::styled("…", theme::dim()));
    }
    spans
}

/// Wrap `text` in `style` with `indent` columns before every line.
fn wrapped(text: &str, style: Style, room: usize, indent: usize) -> Vec<Line<'static>> {
    let lead = [Span::raw(" ".repeat(indent))];
    wrap::wrap_text(&wrap::sanitize(text), style, room + 1, &lead, &lead)
}

/// Like [`wrapped`], with every line indented by `indent` columns beyond the form's margin.
fn wrapped_indent(text: &str, style: Style, room: usize, indent: usize) -> Vec<Line<'static>> {
    wrapped(text, style, room, 1 + indent)
}

fn choice_lines(
    label: &str,
    focused: bool,
    room: usize,
    first: &[Span<'static>],
) -> Vec<Line<'static>> {
    let style = if focused {
        theme::focus().bg(theme::SELECTED_BG)
    } else {
        theme::plain()
    };
    let rest = [Span::raw(" ".repeat(first.iter().map(Span::width).sum()))];
    wrap::wrap_text(label, style, room + 1, first, &rest)
}

fn text_lines(kind: TextKind, text: &str, room: usize) -> Vec<Line<'static>> {
    match kind {
        TextKind::Heading => wrapped(text, theme::heading(2), room, 1),
        TextKind::Legend => wrapped(text, theme::accent().add_modifier(Modifier::BOLD), room, 1),
        TextKind::Para => wrapped(text, theme::plain(), room, 1),
        TextKind::List => {
            let rest = [Span::raw("   ")];
            wrap::wrap_text(
                &wrap::sanitize(text),
                theme::plain(),
                room + 1,
                &[pad()],
                &rest,
            )
        }
        TextKind::Pre => wrap::sanitize(text)
            .split('\n')
            .map(|line| {
                let line = wrap::truncate(line, room.saturating_sub(3)).into_owned();
                Line::from(vec![
                    pad(),
                    Span::styled("│ ", theme::dim()),
                    Span::styled(line, theme::code_block()),
                ])
            })
            .collect(),
    }
}

/// A table with its columns aligned; the widest columns give way when the room is short.
fn table_lines(header: bool, rows: &[Vec<String>], room: usize) -> Vec<Line<'static>> {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|cell| wrap::str_width(cell))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let gaps = 3 * cols.saturating_sub(1);
    while widths.iter().sum::<usize>() + gaps > room {
        let Some(widest) = widths.iter_mut().max() else {
            break;
        };
        if *widest <= 1 {
            break;
        }
        *widest -= 1;
    }
    let mut lines = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        let style = if header && r == 0 {
            theme::fg(theme::YELLOW).add_modifier(Modifier::BOLD)
        } else {
            theme::muted()
        };
        let mut spans = vec![pad()];
        for (c, width) in widths.iter().enumerate() {
            if c > 0 {
                spans.push(Span::styled(" │ ", theme::border()));
            }
            let cell = row.get(c).map_or("", String::as_str);
            let cell = wrap::truncate(&wrap::sanitize(cell), *width).into_owned();
            let fill = width.saturating_sub(wrap::str_width(&cell));
            spans.push(Span::styled(format!("{cell}{}", " ".repeat(fill)), style));
        }
        lines.push(Line::from(spans));
        if header && r == 0 {
            let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            lines.push(Line::from(vec![
                pad(),
                Span::styled(rule.join("─┼─"), theme::border()),
            ]));
        }
    }
    lines
}
