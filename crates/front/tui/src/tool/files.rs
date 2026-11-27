//! Cards of the file tools: `read` and `write` as code cells, `edit` (either mode) and
//! `apply_patch` as diffs (one card per file), `grep` as matches grouped by file.

use super::{Cx, expand_hint, more, text_rows};
use crate::card::{Frame, Section};
use crate::diff::{self, Limit};
use crate::highlight::{self, Lang};
use crate::theme;
use crate::transcript::ToolState;
use crate::wrap;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::fmt::Write as _;

/// Code rows of a collapsed read or write card.
const CODE_ROWS: usize = 12;
/// Diff rows and hunks of a collapsed edit card.
const DIFF_ROWS: usize = 40;
const DIFF_HUNKS: usize = 8;
/// Match rows and files of a collapsed grep card.
const GREP_ROWS: usize = 6;
const GREP_FILES: usize = 2;

/// An error body, or nothing when the call did not fail.
fn error_rows(cx: &Cx<'_>, width: usize) -> Vec<Line<'static>> {
    if cx.state() != ToolState::Failed {
        return Vec::new();
    }
    let rows = text_rows(&cx.card.output, theme::fg(theme::ERROR), width);
    rows.into_iter().take(cx.cap(6)).collect()
}

fn sections(rows: Vec<Line<'static>>) -> Vec<Section> {
    if rows.is_empty() {
        Vec::new()
    } else {
        vec![Section::plain(rows)]
    }
}

/// Highlighted code rows after `gutter` (one per line), capped; the rest is summed up in a hint.
fn code_rows(
    lines: &[String],
    numbers: Option<&[usize]>,
    lang: Option<Lang>,
    cx: &Cx<'_>,
) -> Vec<Line<'static>> {
    let shown = lines.len().min(cx.cap(CODE_ROWS));
    let digits = numbers
        .and_then(|n| n.iter().max())
        .map_or(0, |max| max.to_string().len().max(2));
    let text = lines.get(..shown).unwrap_or_default().join("\n");
    let mut rows: Vec<Line<'static>> = highlight::lines(&text, lang, theme::plain())
        .into_iter()
        .enumerate()
        .map(|(i, segs)| {
            let mut spans = Vec::new();
            if let Some(n) = numbers.and_then(|n| n.get(i)) {
                spans.push(Span::styled(format!("{n:>digits$} │ "), theme::dim()));
            }
            spans.extend(segs.into_iter().map(|(t, s)| Span::styled(t, s)));
            Line::from(spans)
        })
        .collect();
    if lines.len() > shown {
        rows.push(more(&format!("{} more lines", lines.len() - shown)));
    }
    rows
}

/// The `A-B` of a `File: p (lines A-B of T…)` header.
fn read_range(output: &str) -> Option<(usize, usize)> {
    let head = output.lines().next()?;
    let range = head.split("(lines ").nth(1)?.split(' ').next()?;
    let (a, b) = range.split_once('-')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

pub(super) fn read(cx: &Cx<'_>) -> Vec<Line<'static>> {
    let card = cx.card;
    let path = cx.arg("path");
    let frame = cx.frame(1);
    let done = cx.state() == ToolState::Done;
    let range = if done { read_range(&card.output) } else { None };
    // The model's own selector (`a.rs:50-80`) already says which lines; a bare path gets the
    // range the tool actually returned.
    let has_selector = path.rsplit_once(':').is_some_and(|(_, tail)| {
        tail == "raw"
            || (!tail.is_empty()
                && tail
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b"-+,.".contains(&b)))
    });
    let mut desc = path.to_owned();
    if let (Some((a, b)), false) = (range, has_selector) {
        let _ = write!(desc, ":{a}-{b}");
    }
    let mut header = Cx::header(cx.icon(None), "Read", "", Vec::new());
    header.push(Span::raw(" "));
    header.push(Span::styled(
        wrap::sanitize(&desc).into_owned(),
        theme::plain(),
    ));
    let mut rows = Vec::new();
    if done {
        let mut lines = Vec::new();
        let mut numbers = Vec::new();
        let mut notes = Vec::new();
        let skip = usize::from(card.output.starts_with("File: "));
        for line in card.output.lines().skip(skip) {
            let numbered = line.split_once(':').and_then(|(left, text)| {
                let n: usize = left.parse().ok()?;
                Some((n, text))
            });
            match numbered {
                Some((n, text)) => {
                    numbers.push(n);
                    lines.push(wrap::sanitize(text).into_owned());
                }
                None if !line.trim().is_empty() => notes.push(line),
                None => {}
            }
        }
        if numbers.is_empty() && notes.is_empty() {
            notes.push(card.output.lines().next().unwrap_or("(empty)"));
        }
        let lang = highlight::lang_for_path(path);
        rows = code_rows(&lines, Some(&numbers), lang, cx);
        for note in notes {
            rows.extend(text_rows(note, theme::dim(), frame.inner()));
        }
    } else {
        rows.extend(error_rows(cx, frame.inner()));
    }
    frame.render(header, sections(rows), cx.footer())
}

pub(super) fn write(cx: &Cx<'_>) -> Vec<Line<'static>> {
    let frame = cx.frame(1);
    let content = cx.arg("content");
    let count = content.lines().count();
    let meta = vec![Span::styled(
        format!("{count} line{}", if count == 1 { "" } else { "s" }),
        theme::dim(),
    )];
    let header = Cx::header(
        cx.icon(Some(("✎", theme::ACCENT))),
        "Write",
        cx.arg("path"),
        meta,
    );
    let mut rows = error_rows(cx, frame.inner());
    if rows.is_empty() && cx.state() != ToolState::Failed {
        let lines: Vec<String> = wrap::sanitize(content).lines().map(str::to_owned).collect();
        let lang = highlight::lang_for_path(cx.arg("path"));
        rows = code_rows(&lines, None, lang, cx);
    }
    frame.render(header, sections(rows), cx.footer())
}

/// A unified diff made of an `apply_patch` envelope, for sessions that stored no diff.
fn patch_diff(patch: &str) -> String {
    let mut out = String::new();
    for line in patch.lines() {
        let file = line.strip_prefix("*** ").and_then(|rest| {
            ["Update File: ", "Add File: ", "Delete File: "]
                .iter()
                .find_map(|tag| rest.strip_prefix(tag).map(|p| (*tag, p)))
        });
        if let Some((tag, path)) = file {
            let _ = write!(out, "--- a/{path}\n+++ b/{path}\n");
            out.push_str(match tag {
                "Add File: " => "@@ -0,0 +1 @@\n",
                "Delete File: " => "@@ -1 +0,0 @@\n",
                _ => "",
            });
        } else if line.starts_with("@@") {
            out.push_str("@@ -1 +1 @@\n");
        } else if !out.is_empty() && !line.starts_with("*** ") {
            if !matches!(line.chars().next(), Some('+' | '-' | ' ')) {
                out.push(' ');
            }
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// A unified diff made of a hashline `edit` payload, for sessions that stored no diff: each
/// `[PATH#TAG]` section shows its `+TEXT` rows as additions (the replaced rows are not in the call).
fn hashline_diff(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(path) = line
            .strip_prefix('[')
            .and_then(|l| l.strip_suffix(']'))
            .and_then(|l| l.rsplit_once('#'))
            .map(|(path, _)| path)
        {
            let _ = write!(out, "--- a/{path}\n+++ b/{path}\n");
        } else if line.starts_with("PUT ") && line.ends_with(':') && !out.is_empty() {
            out.push_str("@@ -1 +1 @@\n");
        } else if let Some(row) = line.strip_prefix('+')
            && !out.is_empty()
        {
            out.push('+');
            out.push_str(row);
            out.push('\n');
        }
    }
    out
}

pub(super) fn edit(cx: &Cx<'_>) -> Vec<Line<'static>> {
    let card = cx.card;
    let path = cx.arg("path");
    let known = card.diff.is_some();
    // `edit` is hashline (an `input` script) or replace mode (`old` and `new`).
    let hashline = card.name == "edit" && cx.args.get("input").is_some();
    let text = match (&card.diff, card.name.as_str()) {
        (Some(diff), _) => diff.clone(),
        (None, "apply_patch") => patch_diff(cx.arg("patch")),
        (None, _) if hashline => hashline_diff(cx.arg("input")),
        (None, _) => diff::unified(cx.arg("old"), cx.arg("new")),
    };
    let settled = cx.state() == ToolState::Done;
    let mut out = Vec::new();
    for file in diff::files(&text) {
        let name = if file.path.is_empty() {
            path
        } else {
            &file.path
        };
        let mut frame = cx.frame(0);
        if settled {
            frame.border = theme::BORDER_MUTED;
        }
        let (added, removed) = diff::stats(&file.text);
        let meta = vec![
            Span::styled(format!("+{added}"), theme::fg(theme::SUCCESS)),
            Span::styled(format!(" −{removed}"), theme::fg(theme::ERROR)),
        ];
        let op = if card.name == "apply_patch" || hashline {
            file.op
        } else {
            "Update"
        };
        let header = Cx::header(cx.icon(Some(("✎", theme::ACCENT))), op, name, meta);
        let rows = if settled {
            diff_rows(cx, &file.text, &frame, known, name)
        } else {
            error_rows(cx, frame.inner())
        };
        out.extend(frame.render(header, sections(rows), cx.footer()));
    }
    out
}

fn diff_rows(
    cx: &Cx<'_>,
    text: &str,
    frame: &Frame,
    numbers: bool,
    name: &str,
) -> Vec<Line<'static>> {
    let limit = if cx.expanded {
        Limit::rows(super::EXPANDED_CAP)
    } else {
        Limit {
            rows: DIFF_ROWS,
            hunks: DIFF_HUNKS,
        }
    };
    let lang = highlight::lang_for_path(name);
    let body = text.strip_prefix("--- ").map_or(text, |rest| {
        // The path is in the card header: drop the `---`/`+++` pair.
        rest.split_once('\n')
            .and_then(|(_, tail)| tail.strip_prefix("+++ "))
            .and_then(|tail| tail.split_once('\n'))
            .map_or(text, |(_, hunks)| hunks)
    });
    let (mut rows, hidden) = diff::render(body, frame.inner(), numbers, lang, limit);
    if hidden > 0 {
        rows.push(more(&format!("{hidden} more rows")));
    }
    rows
}

/// One `path:line: text` match.
fn split_match(line: &str) -> Option<(&str, &str, &str)> {
    let (path, rest) = line.split_once(':')?;
    // A drive letter (`C:\x`) belongs to the path.
    let (path, rest) = if path.len() == 1 && rest.starts_with(['\\', '/']) {
        let at = path.len() + 1 + rest.find(':')?;
        (line.get(..at)?, line.get(at + 1..)?)
    } else {
        (path, rest)
    };
    let (number, text) = rest.split_once(": ")?;
    number.parse::<usize>().ok()?;
    Some((path, number, text))
}

pub(super) fn grep(cx: &Cx<'_>) -> Vec<Line<'static>> {
    let card = cx.card;
    let frame = cx.frame(1);
    let mut groups: Vec<(&str, Vec<(&str, &str)>)> = Vec::new();
    let mut notes = Vec::new();
    let done = cx.state() == ToolState::Done;
    for line in card.output.lines().filter(|_| done) {
        match split_match(line) {
            Some((path, number, text)) => match groups.last_mut() {
                Some((p, rows)) if *p == path => rows.push((number, text)),
                _ => groups.push((path, vec![(number, text)])),
            },
            None if line.starts_with('[') => notes.push(line),
            None => {}
        }
    }
    let matches: usize = groups.iter().map(|(_, rows)| rows.len()).sum();
    let meta = if done && matches > 0 {
        vec![Span::styled(
            format!("{matches} matches · {} files", groups.len()),
            theme::dim(),
        )]
    } else {
        Vec::new()
    };
    let header = Cx::header(
        cx.icon(Some(("⌕", theme::ACCENT))),
        "Grep",
        cx.arg("pattern"),
        meta,
    );
    let mut rows = Vec::new();
    let mut hidden = 0;
    if done && matches == 0 {
        rows.push(Line::from(vec![
            Span::styled("⚠ ", theme::fg(theme::WARNING)),
            Span::styled("No matches found", theme::muted()),
        ]));
    }
    let (row_cap, file_cap) = if cx.expanded {
        (super::EXPANDED_CAP, usize::MAX)
    } else {
        (GREP_ROWS, GREP_FILES)
    };
    for (i, (path, found)) in groups.iter().enumerate() {
        if i >= file_cap || rows.len() >= row_cap {
            hidden += found.len() + 1;
            continue;
        }
        rows.push(Line::from(Span::styled(
            wrap::sanitize(path).into_owned(),
            theme::plain().add_modifier(Modifier::BOLD),
        )));
        for (n, (number, text)) in found.iter().enumerate() {
            if rows.len() >= row_cap {
                hidden += found.len() - n;
                break;
            }
            let lead = Span::styled(format!("  {number}: "), theme::dim());
            let text = wrap::sanitize(text);
            let style: Style = theme::muted();
            rows.push(Line::from(vec![
                lead,
                Span::styled(text.into_owned(), style),
            ]));
        }
    }
    if hidden > 0 {
        rows.push(Line::from(vec![
            Span::styled(format!("… {hidden} more "), theme::dim()),
            expand_hint(),
        ]));
    }
    for note in notes {
        rows.push(Line::from(Span::styled(note.to_owned(), theme::dim())));
    }
    rows.extend(error_rows(cx, frame.inner()));
    frame.render(header, sections(rows), cx.footer())
}
