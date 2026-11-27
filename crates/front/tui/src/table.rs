//! Markdown tables: columns fitted to the terminal width, cells word-wrapped, sharp borders.
//! Too narrow for a grid, rows degrade to `a · b · c` lines.

use crate::theme;
use crate::wrap::{self, Seg};
use pulldown_cmark::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

const MIN_COLUMN: usize = 3;

pub(crate) struct Table {
    aligns: Vec<Alignment>,
    /// The first row is the header when `has_head`.
    rows: Vec<Vec<Vec<Seg>>>,
    has_head: bool,
    row: Vec<Vec<Seg>>,
    cell: Vec<Seg>,
}

fn cell_width(cell: &[Seg]) -> usize {
    cell.iter().map(|(t, _)| wrap::str_width(t)).sum()
}

fn border(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), theme::plain())
}

impl Table {
    pub(crate) fn new(aligns: Vec<Alignment>) -> Self {
        Self {
            aligns,
            rows: Vec::new(),
            has_head: false,
            row: Vec::new(),
            cell: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, text: &str, style: Style) {
        let text = text.replace('\n', " ");
        match self.cell.last_mut() {
            Some((t, s)) if *s == style => t.push_str(&text),
            _ => self.cell.push((text, style)),
        }
    }

    pub(crate) fn end_cell(&mut self) {
        let cell = std::mem::take(&mut self.cell);
        self.row.push(cell);
    }

    pub(crate) fn end_row(&mut self) {
        let row = std::mem::take(&mut self.row);
        self.rows.push(row);
    }

    /// `pulldown-cmark` puts header cells straight into the head, without a row.
    pub(crate) fn end_head(&mut self) {
        if !self.row.is_empty() {
            self.end_row();
        }
        self.has_head = true;
    }

    pub(crate) fn render(
        &self,
        width: usize,
        first: &[Span<'static>],
        rest: &[Span<'static>],
    ) -> Vec<Line<'static>> {
        let columns = self
            .rows
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .max(self.aligns.len());
        if columns == 0 {
            return Vec::new();
        }
        let prefix_width: usize = rest.iter().map(Span::width).sum();
        let avail = width.saturating_sub(prefix_width + 3 * columns + 1);
        if avail < columns * MIN_COLUMN {
            return self.render_stacked(width, first, rest);
        }
        let natural: Vec<usize> = (0..columns)
            .map(|c| {
                self.rows
                    .iter()
                    .filter_map(|r| r.get(c))
                    .map(|cell| cell_width(cell))
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect();
        let widths = fit(natural, avail);

        let mut out = vec![rule(first, &widths, ['┌', '┬', '┐'])];
        for (i, row) in self.rows.iter().enumerate() {
            let head = self.has_head && i == 0;
            out.extend(self.render_row(row, &widths, head, rest));
            if i + 1 < self.rows.len() {
                out.push(rule(rest, &widths, ['├', '┼', '┤']));
            }
        }
        out.push(rule(rest, &widths, ['└', '┴', '┘']));
        out
    }

    fn render_row(
        &self,
        row: &[Vec<Seg>],
        widths: &[usize],
        head: bool,
        prefix: &[Span<'static>],
    ) -> Vec<Line<'static>> {
        let cells: Vec<Vec<Line<'static>>> = widths
            .iter()
            .enumerate()
            .map(|(c, w)| {
                let segs = row
                    .get(c)
                    .map(|cell| styled(cell, head))
                    .unwrap_or_default();
                wrap::wrap(&segs, *w, &[], &[])
            })
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        (0..height)
            .map(|i| {
                let mut spans = prefix.to_vec();
                spans.push(border("│"));
                for (c, w) in widths.iter().enumerate() {
                    let line = cells
                        .get(c)
                        .and_then(|lines| lines.get(i))
                        .cloned()
                        .unwrap_or_default();
                    let pad = w.saturating_sub(line.width());
                    let (left, right) = match self.aligns.get(c) {
                        Some(Alignment::Right) => (pad, 0),
                        Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                        _ => (0, pad),
                    };
                    spans.push(Span::raw(" ".repeat(left + 1)));
                    spans.extend(line.spans);
                    spans.push(Span::raw(" ".repeat(right + 1)));
                    spans.push(border("│"));
                }
                Line::from(spans)
            })
            .collect()
    }

    fn render_stacked(
        &self,
        width: usize,
        first: &[Span<'static>],
        rest: &[Span<'static>],
    ) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            let head = self.has_head && i == 0;
            let mut segs: Vec<Seg> = Vec::new();
            for (c, cell) in row.iter().enumerate() {
                if c > 0 {
                    segs.push((" · ".to_owned(), theme::dim()));
                }
                segs.extend(styled(cell, head));
            }
            let lead = if out.is_empty() { first } else { rest };
            out.extend(wrap::wrap(&segs, width, lead, rest));
        }
        out
    }
}

fn styled(cell: &[Seg], head: bool) -> Vec<Seg> {
    if !head {
        return cell.to_vec();
    }
    cell.iter()
        .map(|(t, s)| (t.clone(), s.fg(theme::YELLOW).add_modifier(Modifier::BOLD)))
        .collect()
}

/// Shrink the widest column one cell at a time until the table fits (or nothing can shrink).
fn fit(mut widths: Vec<usize>, avail: usize) -> Vec<usize> {
    let mut total: usize = widths.iter().sum();
    while total > avail {
        let Some(widest) = widths.iter_mut().max() else {
            break;
        };
        if *widest <= MIN_COLUMN {
            break;
        }
        *widest -= 1;
        total -= 1;
    }
    widths
}

fn rule(
    prefix: &[Span<'static>],
    widths: &[usize],
    [left, mid, right]: [char; 3],
) -> Line<'static> {
    let mut text = String::new();
    text.push(left);
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            text.push(mid);
        }
        text.push_str(&"─".repeat(w + 2));
    }
    text.push(right);
    let mut spans = prefix.to_vec();
    spans.push(border(text));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(rows: &[&[&str]]) -> Table {
        let mut t = Table::new(vec![Alignment::None, Alignment::Right]);
        for (i, row) in rows.iter().enumerate() {
            for cell in *row {
                t.push(cell, Style::new());
                t.end_cell();
            }
            if i == 0 {
                t.end_head();
            } else {
                t.end_row();
            }
        }
        t
    }

    #[test]
    fn grid_fits_the_width_and_aligns_columns() {
        let t = table(&[&["name", "n"], &["alpha", "1"], &["b", "22"]]);
        let lines: Vec<String> = t
            .render(40, &[], &[])
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            lines,
            [
                "┌───────┬────┐",
                "│ name  │  n │",
                "├───────┼────┤",
                "│ alpha │  1 │",
                "├───────┼────┤",
                "│ b     │ 22 │",
                "└───────┴────┘",
            ]
        );
    }

    #[test]
    fn wide_cells_wrap_and_tiny_widths_stack() {
        let t = table(&[&["k", "v"], &["key", "a long value that wraps"]]);
        let lines = t.render(20, &[], &[]);
        assert!(lines.iter().all(|l| l.width() <= 20), "{lines:?}");
        assert!(lines.len() > 5);
        let stacked: Vec<String> = t
            .render(8, &[], &[])
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(stacked.first().map(String::as_str), Some("k · v"));
    }
}
