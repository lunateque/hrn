//! Unified diffs as structured rows: `{sign}{number}│ content`, the whole row colored by what
//! happened to it (removed red, added green, context muted and highlighted). A one-for-one line
//! replacement gets word emphasis in reverse video, indentation shows as dim `·` / ` → `, and a
//! `…` row marks the gap between hunks.

use crate::card;
use crate::highlight::{self, Lang};
use crate::theme;
use crate::wrap::{self, Seg};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use similar::{Algorithm, ChangeTag, TextDiff};

/// Unified diff text for an edit preview (no file header, three lines of context).
pub(crate) fn unified(old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .to_string()
}

/// How much of a diff to paint: rows and hunks (whichever runs out first).
#[derive(Clone, Copy)]
pub(crate) struct Limit {
    pub(crate) rows: usize,
    pub(crate) hunks: usize,
}

impl Limit {
    #[cfg(test)]
    pub(crate) const ALL: Self = Self {
        rows: usize::MAX,
        hunks: usize::MAX,
    };

    pub(crate) const fn rows(rows: usize) -> Self {
        Self {
            rows,
            hunks: usize::MAX,
        }
    }
}

enum Row<'a> {
    File(&'a str),
    Hunk,
    Context(usize, &'a str),
    Removed(usize, &'a str),
    Added(usize, &'a str),
}

fn hunk_start(header: &str) -> Option<(usize, usize)> {
    let mut parts = header.split_whitespace();
    let old = parts
        .next()?
        .strip_prefix('-')?
        .split(',')
        .next()?
        .parse()
        .ok()?;
    let new = parts
        .next()?
        .strip_prefix('+')?
        .split(',')
        .next()?
        .parse()
        .ok()?;
    Some((old, new))
}

fn parse(diff: &str) -> Vec<Row<'_>> {
    let mut rows = Vec::new();
    let (mut old, mut new) = (1, 1);
    let mut in_hunk = false;
    let mut lines = diff.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(path) = line.strip_prefix("--- ")
            && lines.peek().is_some_and(|next| next.starts_with("+++ "))
        {
            lines.next();
            let path = path.strip_prefix("a/").unwrap_or(path);
            rows.push(Row::File(path));
            in_hunk = false;
            continue;
        }
        if let Some(header) = line.strip_prefix("@@") {
            (old, new) = hunk_start(header).unwrap_or((old, new));
            rows.push(Row::Hunk);
            in_hunk = true;
            continue;
        }
        if !in_hunk || line.starts_with('\\') {
            continue;
        }
        if let Some(text) = line.strip_prefix('-') {
            rows.push(Row::Removed(old, text));
            old += 1;
        } else if let Some(text) = line.strip_prefix('+') {
            rows.push(Row::Added(new, text));
            new += 1;
        } else {
            rows.push(Row::Context(old, line.strip_prefix(' ').unwrap_or(line)));
            old += 1;
            new += 1;
        }
    }
    rows
}

/// Added and removed line counts.
pub(crate) fn stats(diff: &str) -> (usize, usize) {
    parse(diff)
        .iter()
        .fold((0, 0), |(added, removed), row| match row {
            Row::Added(..) => (added + 1, removed),
            Row::Removed(..) => (added, removed + 1),
            _ => (added, removed),
        })
}

/// One file's share of a (possibly multi-file) diff.
pub(crate) struct FileDiff {
    pub(crate) path: String,
    /// `Update`, `Create` or `Delete`, read off the first hunk header.
    pub(crate) op: &'static str,
    pub(crate) text: String,
}

/// Split a diff at its `--- a/path` / `+++ b/path` headers. Header-less text is one entry with an
/// empty path.
pub(crate) fn files(diff: &str) -> Vec<FileDiff> {
    let lines: Vec<&str> = diff.lines().collect();
    let starts: Vec<usize> = (0..lines.len())
        .filter(|&i| {
            lines.get(i).is_some_and(|l| l.starts_with("--- "))
                && lines.get(i + 1).is_some_and(|l| l.starts_with("+++ "))
        })
        .collect();
    if starts.is_empty() {
        return vec![file_of(String::new(), diff.to_owned())];
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts.get(n + 1).copied().unwrap_or(lines.len());
            let body = lines.get(start..end).unwrap_or_default();
            let path = body
                .first()
                .and_then(|l| l.strip_prefix("--- "))
                .map(|p| p.strip_prefix("a/").unwrap_or(p).to_owned())
                .unwrap_or_default();
            file_of(path, body.join("\n"))
        })
        .collect()
}

fn file_of(path: String, text: String) -> FileDiff {
    let header = text
        .lines()
        .find_map(|l| l.strip_prefix("@@"))
        .unwrap_or_default();
    let op = if header.contains("-0,0 ") {
        "Create"
    } else if header.contains("+0,0 ") {
        "Delete"
    } else {
        "Update"
    };
    FileDiff { path, op, text }
}

/// Character-level emphasis for a removed/added pair, in the base style of each side. A pair
/// that is mostly different gets none: reversing nearly the whole line only adds noise.
fn emphasis(old: &str, new: &str, removed: Style, added: Style) -> (Vec<Seg>, Vec<Seg>) {
    let changes = similar::utils::diff_chars(Algorithm::Patience, old, new);
    let same: usize = changes
        .iter()
        .filter(|(tag, _)| *tag == ChangeTag::Equal)
        .map(|(_, text)| text.chars().count())
        .sum();
    let longest = old.chars().count().max(new.chars().count());
    if same * 2 < longest {
        let (old, new) = (
            wrap::sanitize(old).into_owned(),
            wrap::sanitize(new).into_owned(),
        );
        return (vec![(old, removed)], vec![(new, added)]);
    }
    let (mut out_old, mut out_new) = (Vec::new(), Vec::new());
    for (tag, text) in changes {
        let text = wrap::sanitize(text).into_owned();
        match tag {
            ChangeTag::Equal => {
                out_old.push((text.clone(), removed));
                out_new.push((text, added));
            }
            ChangeTag::Delete => out_old.push((text, removed.add_modifier(Modifier::REVERSED))),
            ChangeTag::Insert => out_new.push((text, added.add_modifier(Modifier::REVERSED))),
        }
    }
    (out_old, out_new)
}

/// Rendered diff rows within `limit`; the count of rows left out is returned too. `numbers:
/// false` hides line numbers, for previews whose numbers would be meaningless. `lang` highlights
/// context rows.
pub(crate) fn render(
    diff: &str,
    width: usize,
    numbers: bool,
    lang: Option<Lang>,
    limit: Limit,
) -> (Vec<Line<'static>>, usize) {
    let rows = parse(diff);
    let digits = rows
        .iter()
        .map(|row| match row {
            Row::Context(n, _) | Row::Removed(n, _) | Row::Added(n, _) => *n,
            _ => 0,
        })
        .max()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(3);
    let mut p = Painter {
        width,
        digits: if numbers { digits } else { 0 },
        lang,
        out: Vec::new(),
        gaps: Vec::new(),
        painted: false,
        prev: None,
        state: highlight::State::default(),
    };
    let mut i = 0;
    while let Some(row) = rows.get(i) {
        match row {
            Row::File(path) => p.file(path),
            Row::Hunk => p.hunk(),
            Row::Context(n, text) => p.context(*n, text),
            Row::Removed(..) | Row::Added(..) => {
                let removed = take_run(&rows, &mut i, true);
                let added = take_run(&rows, &mut i, false);
                p.changes(&removed, &added);
                continue;
            }
        }
        i += 1;
    }
    let total = p.out.len();
    let cut = limit
        .hunks
        .checked_sub(1)
        .and_then(|n| p.gaps.get(n))
        .copied()
        .unwrap_or(total)
        .min(limit.rows);
    let mut out = p.out;
    out.truncate(cut);
    (out, total.saturating_sub(cut))
}

fn take_run<'a>(rows: &[Row<'a>], i: &mut usize, removed: bool) -> Vec<(usize, &'a str)> {
    let mut run = Vec::new();
    while let Some(row) = rows.get(*i) {
        match (row, removed) {
            (Row::Removed(n, text), true) | (Row::Added(n, text), false) => run.push((*n, *text)),
            _ => break,
        }
        *i += 1;
    }
    run
}

struct Painter {
    width: usize,
    /// Number column width; 0 hides the numbers.
    digits: usize,
    lang: Option<Lang>,
    out: Vec<Line<'static>>,
    /// Index of every `…` row, where a hunk cap may cut.
    gaps: Vec<usize>,
    /// Rows were painted since the last file header, so a new hunk needs a separator.
    painted: bool,
    /// The number printed on the previous row, so a repeat is blanked.
    prev: Option<usize>,
    state: highlight::State,
}

/// The indentation of `text` and the rest of the line.
fn split_indent(text: &str) -> (&str, &str) {
    let rest = text.trim_start_matches([' ', '\t']);
    text.split_at(text.len() - rest.len())
}

impl Painter {
    fn file(&mut self, path: &str) {
        if !self.out.is_empty() {
            self.out.push(Line::default());
        }
        let path = wrap::sanitize(path).into_owned();
        let style = theme::muted().add_modifier(Modifier::BOLD);
        self.out.push(Line::from(Span::styled(path, style)));
        self.painted = false;
        self.prev = None;
    }

    fn hunk(&mut self) {
        if self.painted {
            self.gaps.push(self.out.len());
            self.out
                .push(Line::from(Span::styled("…", theme::diff_context())));
        }
        self.prev = None;
        self.state = highlight::State::default();
    }

    /// `{sign}{number}│`, the number column right-aligned.
    fn gutter(&mut self, number: usize, sign: char, style: Style) -> Span<'static> {
        let shown = if self.digits > 0 && self.prev != Some(number) {
            number.to_string()
        } else {
            String::new()
        };
        self.prev = Some(number);
        let cell = format!("{sign}{shown}");
        Span::styled(format!("{cell:>w$}│", w = self.digits + 1), style)
    }

    /// Indentation as dim marks (`·`, ` → `), or plain spaces under highlighting.
    fn indent(indent: &str, style: Style, marks: bool) -> Vec<Seg> {
        if indent.is_empty() {
            return Vec::new();
        }
        if !marks {
            return vec![(indent.replace('\t', "    "), style)];
        }
        let dim = style.fg(theme::DIM);
        indent
            .chars()
            .map(|c| (if c == '\t' { " → " } else { "·" }.to_owned(), dim))
            .collect()
    }

    fn push(&mut self, number: usize, sign: char, style: Style, lead: Vec<Seg>, body: Vec<Seg>) {
        let mut spans = vec![self.gutter(number, sign, style)];
        spans.extend(
            lead.into_iter()
                .chain(body)
                .map(|(t, s)| Span::styled(t, s)),
        );
        self.out.push(Line::from(card::clip(spans, self.width)));
        self.painted = true;
    }

    fn context(&mut self, number: usize, text: &str) {
        let style = theme::diff_context();
        let (indent, rest) = split_indent(text);
        let marks = self.lang.is_none();
        let lead = Self::indent(indent, style, marks);
        let rest = wrap::sanitize(rest);
        let body = if self.lang.is_some() {
            highlight::line(&rest, self.lang, &mut self.state, style)
        } else {
            vec![(rest.into_owned(), style)]
        };
        self.push(number, ' ', style, lead, body);
    }

    fn changes(&mut self, removed: &[(usize, &str)], added: &[(usize, &str)]) {
        self.state = highlight::State::default();
        let (rm, ad) = (theme::diff_removed(), theme::diff_added());
        if let ([(old_n, old)], [(new_n, new)]) = (removed, added) {
            let (old_indent, old_rest) = split_indent(old);
            let (new_indent, new_rest) = split_indent(new);
            let (old_segs, new_segs) = emphasis(old_rest, new_rest, rm, ad);
            let lead = Self::indent(old_indent, rm, true);
            self.push(*old_n, '-', rm, lead, old_segs);
            let lead = Self::indent(new_indent, ad, true);
            self.push(*new_n, '+', ad, lead, new_segs);
            return;
        }
        for (rows, sign, style) in [(removed, '-', rm), (added, '+', ad)] {
            for (number, text) in rows {
                let (indent, rest) = split_indent(text);
                let lead = Self::indent(indent, style, true);
                let body = vec![(wrap::sanitize(rest).into_owned(), style)];
                self.push(*number, sign, style, lead, body);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n fn a() {}\n-let x = 1;\n+let x = 2;\n fn b() {}\n";

    fn shown(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.to_string().trim_end().to_owned())
            .collect()
    }

    #[test]
    fn rows_carry_signs_numbers_a_bar_and_word_emphasis() {
        let (lines, hidden) = render(DIFF, 40, true, None, Limit::ALL);
        assert_eq!(hidden, 0);
        assert_eq!(
            shown(&lines),
            [
                "src/lib.rs",
                "   1│fn a() {}",
                "  -2│let x = 1;",
                "   +│let x = 2;",
                "   3│fn b() {}",
            ]
        );
        let added = lines.get(3).expect("added row");
        let emphasised: Vec<&str> = added
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(emphasised, ["2"]);
        let removed = lines.get(2).expect("removed row");
        assert_eq!(
            removed.spans.first().map(|s| s.style.fg),
            Some(theme::diff_removed().fg)
        );
        assert_eq!(stats(DIFF), (1, 1));
    }

    #[test]
    fn blocks_of_changes_get_no_word_emphasis_and_indent_is_marked() {
        let diff = "@@ -1,2 +1,3 @@\n-\tone\n-two\n+  three\n+four\n+five\n";
        let (lines, _) = render(diff, 40, true, None, Limit::ALL);
        assert!(shown(&lines)[0].contains(" → one"), "{:?}", shown(&lines));
        assert!(shown(&lines)[2].contains("··three"), "{:?}", shown(&lines));
        let reversed = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.add_modifier.contains(Modifier::REVERSED));
        assert!(!reversed);
    }

    #[test]
    fn previews_hide_numbers_and_gaps_and_caps_apply() {
        let diff = unified("a\nb\n", "a\nc\n");
        let (lines, _) = render(&diff, 20, false, None, Limit::ALL);
        assert!(shown(&lines)[1].starts_with("-│b"), "{:?}", shown(&lines));
        let long = (0..50)
            .map(|i| format!("+line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (lines, hidden) = render(
            &format!("@@ -0,0 +1,50 @@\n{long}\n"),
            20,
            true,
            None,
            Limit::rows(10),
        );
        assert_eq!((lines.len(), hidden), (10, 40));
        let two = "@@ -1 +1 @@\n-a\n+b\n@@ -9 +9 @@\n-c\n+d\n";
        let (all, _) = render(two, 20, true, None, Limit::ALL);
        assert_eq!(shown(&all)[1..3], ["   +│b", "…"].map(String::from));
        let (first, hidden) = render(two, 20, true, None, Limit { rows: 99, hunks: 1 });
        assert_eq!((first.len(), hidden), (2, 3));
    }

    #[test]
    fn multi_file_diffs_split_with_their_operation() {
        let diff = format!(
            "{DIFF}--- a/new.txt\n+++ b/new.txt\n@@ -0,0 +1,1 @@\n+hi\n--- a/old.txt\n+++ b/old.txt\n@@ -1,1 +0,0 @@\n-bye\n"
        );
        let parts = files(&diff);
        let got: Vec<(&str, &str)> = parts.iter().map(|f| (f.path.as_str(), f.op)).collect();
        assert_eq!(
            got,
            [
                ("src/lib.rs", "Update"),
                ("new.txt", "Create"),
                ("old.txt", "Delete")
            ]
        );
        assert_eq!(stats(&parts[1].text), (1, 0));
    }
}
