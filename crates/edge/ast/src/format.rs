//! The model-facing text of both tools, in omp's shapes.

use crate::protocol::{RewriteData, SearchData};
use crate::render::{
    Section, first_line, flat, grouped, match_lines, parse_errors, parse_errors_block,
};
use std::fmt::Write as _;

const LIMIT_NOTE: &str = "Result limit reached; narrow path or use skip to page through results.";
const EDIT_LIMIT_NOTE: &str = "Limit reached; narrow paths.";
const NO_MATCH_WITH_ERRORS: &str = "No matches found. Parse issues mean the query may be mis-scoped; narrow `path` before \
     concluding absence.";

/// A rendered result: the model-facing text and the files it names.
pub struct Shown {
    /// The text to hand to the model.
    pub text: String,
    /// The distinct files the result names, in report order.
    pub files: Vec<String>,
}

/// The `[path#TAG]` header tag of one displayed path.
pub type Tag<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Render one search pass.
#[must_use]
pub fn show_search(found: &SearchData, is_directory: bool, tag: Tag<'_>) -> Shown {
    let (shown_errors, total_errors) = parse_errors(&found.parse_errors);
    let files = order(&found.matches);
    if found.matches.is_empty() {
        let mut text = if shown_errors.is_empty() {
            "No matches found".to_owned()
        } else {
            NO_MATCH_WITH_ERRORS.to_owned()
        };
        append_errors(&mut text, &shown_errors, total_errors);
        return Shown { text, files };
    }
    let render = |path: &str| -> Section {
        let mut lines = Vec::new();
        for m in found.matches.iter().filter(|m| m.path == path) {
            lines.extend(match_lines(m.line, &m.text));
            if !m.meta.is_empty() {
                let meta = m
                    .meta
                    .iter()
                    .map(|(k, v)| format!("{k}={}", one_row(v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("  meta: {meta}"));
            }
        }
        Section {
            tag: tag(path),
            count: None,
            lines,
        }
    };
    let mut text = body(&files, is_directory, render);
    if found.limit_reached {
        let _ = write!(text, "\n\n{LIMIT_NOTE}");
    }
    append_errors(&mut text, &shown_errors, total_errors);
    Shown { text, files }
}

/// Render one rewrite pass; `note` is the pending-proposal line put above the diff.
#[must_use]
pub fn show_rewrite(
    rewritten: &RewriteData,
    is_directory: bool,
    note: Option<&str>,
    tag: Tag<'_>,
) -> Shown {
    let (shown_errors, total_errors) = parse_errors(&rewritten.parse_errors);
    let files: Vec<String> = rewritten.per_file.iter().map(|(p, _)| p.clone()).collect();
    if rewritten.total == 0 {
        let mut text = "No replacements made".to_owned();
        append_errors(&mut text, &shown_errors, total_errors);
        return Shown { text, files };
    }
    let render = |path: &str| -> Section {
        let mut lines = Vec::new();
        for c in rewritten.changes.iter().filter(|c| c.path == path) {
            lines.push(format!("-{}:{}", c.line, first_line(&c.before)));
            lines.push(format!("+{}:{}", c.line, first_line(&c.after)));
        }
        Section {
            tag: tag(path),
            count: rewritten
                .per_file
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, n)| *n),
            lines,
        }
    };
    let mut text = body(&files, is_directory, render);
    if let Some(lead) = note {
        text = format!("{lead}\n\n{text}");
    }
    if rewritten.limit_reached {
        let _ = write!(text, "\n\n{EDIT_LIMIT_NOTE}");
    }
    append_errors(&mut text, &shown_errors, total_errors);
    Shown { text, files }
}

/// The apply report: what landed, plus a fresh content tag per touched file.
#[must_use]
pub fn show_applied(rewritten: &RewriteData, tag: Tag<'_>) -> String {
    let mut text = format!(
        "Applied {} replacement{} in {} file{}.",
        rewritten.total,
        plural(rewritten.total),
        rewritten.files_touched,
        plural(rewritten.files_touched)
    );
    for (path, _) in &rewritten.per_file {
        if let Some(t) = tag(path) {
            let _ = writeln!(text, "[{path}#{t}]");
        }
    }
    text
}

/// The files a pass touched, in report order.
#[must_use]
pub fn files_of(rewritten: &RewriteData) -> Vec<String> {
    rewritten.per_file.iter().map(|(p, _)| p.clone()).collect()
}

fn body(files: &[String], is_directory: bool, render: impl Fn(&str) -> Section) -> String {
    if is_directory {
        grouped(files, render)
    } else {
        flat(files, render)
    }
}

fn order(matches: &[crate::protocol::Found]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for m in matches {
        if !seen.iter().any(|s| s == &m.path) {
            seen.push(m.path.clone());
        }
    }
    seen
}

fn append_errors(text: &mut String, shown: &[String], total: usize) {
    if !shown.is_empty() {
        let _ = write!(text, "\n\n{}", parse_errors_block(shown, total));
    }
}

/// A capture that spans lines still gets one row.
fn one_row(text: &str) -> String {
    text.replace('\n', " ")
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
