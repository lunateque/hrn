//! The tool-card frame, rounded box: a rounded box whose top bar carries the
//! header (`╭─── header ────╮`), whose labelled sections are tee bars (`├─── Output ───┤`) and
//! whose bottom bar carries the footer (`╰─── 96ms/2m ───╯`). Content rows sit between `│`
//! edges with `pad` columns of inset; every row is exactly `width` columns wide.

use crate::wrap;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Dashes between a corner and the label of a bar.
const CAP: usize = 3;

/// Geometry and colors of one frame.
pub(crate) struct Frame {
    pub(crate) width: usize,
    pub(crate) border: Color,
    /// Painted under every cell of the frame when set.
    pub(crate) bg: Option<Color>,
    /// Columns of inset inside the side edges (edit cards draw their own gutter and use 0).
    pub(crate) pad: usize,
}

/// A run of content rows, introduced by a tee bar when it has a label.
pub(crate) struct Section {
    pub(crate) label: Vec<Span<'static>>,
    pub(crate) rows: Vec<Line<'static>>,
}

impl Section {
    pub(crate) fn plain(rows: Vec<Line<'static>>) -> Self {
        Self {
            label: Vec::new(),
            rows,
        }
    }

    pub(crate) fn labelled(label: Vec<Span<'static>>, rows: Vec<Line<'static>>) -> Self {
        Self { label, rows }
    }
}

/// Cut `spans` to `max` columns; a cut ends in `…` in the style of the last kept span.
pub(crate) fn clip(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= max {
        return spans;
    }
    let budget = max.saturating_sub(1);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let mut style = spans.first().map(|s| s.style).unwrap_or_default();
    'spans: for span in spans {
        style = span.style;
        let mut text = String::new();
        for c in span.content.chars() {
            let w = wrap::char_width(c);
            if used + w > budget {
                if !text.is_empty() {
                    out.push(Span::styled(text, style));
                }
                break 'spans;
            }
            used += w;
            text.push(c);
        }
        out.push(Span::styled(text, style));
    }
    if max > 0 {
        out.push(Span::styled("…", style));
    }
    out
}

fn width_of(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

impl Frame {
    /// Columns available to a content row.
    pub(crate) fn inner(&self) -> usize {
        self.width.saturating_sub(2 + 2 * self.pad).max(1)
    }

    fn edge_style(&self) -> Style {
        let style = Style::new().fg(self.border);
        self.bg.map_or(style, |bg| style.bg(bg))
    }

    fn with_bg(&self, spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
        let Some(bg) = self.bg else { return spans };
        spans
            .into_iter()
            .map(|mut s| {
                if s.style.bg.is_none() {
                    s.style = s.style.bg(bg);
                }
                s
            })
            .collect()
    }

    /// `left` + dashes + ` label ` + dashes + `right`, `width` columns wide.
    fn bar(&self, [left, right]: [char; 2], label: Vec<Span<'static>>) -> Line<'static> {
        let edge = self.edge_style();
        let mut spans = vec![Span::styled(left.to_string(), edge)];
        let label = if label.is_empty() || self.width < CAP + 7 {
            Vec::new()
        } else {
            // Corner, cap, two spaces, at least one dash and the far corner stay around it.
            clip(label, self.width.saturating_sub(CAP + 5))
        };
        let used = if label.is_empty() {
            1
        } else {
            spans.push(Span::styled("─".repeat(CAP), edge));
            spans.push(Span::styled(" ", edge));
            let w = width_of(&label);
            spans.extend(self.with_bg(label));
            spans.push(Span::styled(" ", edge));
            1 + CAP + 2 + w
        };
        let fill = self.width.saturating_sub(used + 1);
        spans.push(Span::styled("─".repeat(fill), edge));
        spans.push(Span::styled(right.to_string(), edge));
        Line::from(spans)
    }

    /// One content row: edges, inset, `line` clipped and padded to the inner width.
    fn row(&self, line: Line<'static>) -> Line<'static> {
        let edge = self.edge_style();
        let fill = self.bg.map_or_else(Style::new, |bg| Style::new().bg(bg));
        let inner = self.inner();
        let content = clip(line.spans, inner);
        let used = width_of(&content);
        let inset = || Span::styled(" ".repeat(self.pad), fill);
        let mut spans = vec![Span::styled("│", edge), inset()];
        spans.extend(self.with_bg(content));
        spans.push(Span::styled(" ".repeat(inner.saturating_sub(used)), fill));
        spans.push(inset());
        spans.push(Span::styled("│", edge));
        Line::from(spans)
    }

    /// The whole card: top bar, sections, bottom bar.
    pub(crate) fn render(
        &self,
        header: Vec<Span<'static>>,
        sections: Vec<Section>,
        footer: Vec<Span<'static>>,
    ) -> Vec<Line<'static>> {
        let mut out = vec![self.bar(['╭', '╮'], header)];
        for section in sections {
            if !section.label.is_empty() {
                out.push(self.bar(['├', '┤'], section.label));
            }
            out.extend(section.rows.into_iter().map(|row| self.row(row)));
        }
        let fits = width_of(&footer) + 2 * CAP + 2 <= self.width;
        if footer.is_empty() || fits {
            out.push(self.bar(['╰', '╯'], footer));
        } else {
            out.push(self.row(Line::from(footer)));
            out.push(self.bar(['╰', '╯'], Vec::new()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: usize) -> Frame {
        Frame {
            width,
            border: Color::Red,
            bg: None,
            pad: 1,
        }
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn bars_carry_header_sections_and_footer() {
        let rows = vec![Line::from("echo hi")];
        let out = frame(21).render(
            vec![Span::raw("Bash")],
            vec![
                Section::plain(rows),
                Section::labelled(vec![Span::raw("Output")], vec![Line::from("hi")]),
            ],
            vec![Span::raw("96ms")],
        );
        assert_eq!(
            text(&out),
            [
                "╭─── Bash ──────────╮",
                "│ echo hi           │",
                "├─── Output ────────┤",
                "│ hi                │",
                "╰─── 96ms ──────────╯"
            ]
            .map(str::to_owned),
        );
        assert!(out.iter().all(|l| l.width() == 21), "{out:?}");
    }

    #[test]
    fn every_row_is_exactly_the_width_and_long_content_is_clipped() {
        let long = Line::from("x".repeat(50));
        let out = frame(20).render(
            vec![Span::raw("a very long header that cannot fit")],
            vec![Section::plain(vec![long])],
            Vec::new(),
        );
        assert!(out.iter().all(|l| l.width() == 20), "{:?}", text(&out));
        assert!(text(&out)[0].contains('…'));
        assert!(text(&out)[1].contains('…'));
    }

    #[test]
    fn a_footer_that_does_not_fit_moves_into_a_row() {
        let out = frame(16).render(
            Vec::new(),
            vec![Section::plain(vec![Line::from("body")])],
            vec![Span::raw("a footer too wide")],
        );
        let shown = text(&out);
        assert_eq!(shown.len(), 4, "{shown:?}");
        assert!(shown[2].starts_with("│ a footer"), "{shown:?}");
        assert_eq!(shown[3], "╰──────────────╯");
    }

    #[test]
    fn the_background_reaches_every_cell() {
        let f = Frame {
            bg: Some(Color::Blue),
            ..frame(12)
        };
        let out = f.render(
            Vec::new(),
            vec![Section::plain(vec![Line::from(Span::raw("hi"))])],
            Vec::new(),
        );
        for line in &out {
            assert!(line.spans.iter().all(|s| s.style.bg == Some(Color::Blue)));
        }
    }
}
