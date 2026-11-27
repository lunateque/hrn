//! Shared rendering for the structural tools: file grouping, match rows, parse-issue blocks.
//!
//! The shapes are omp's: directory results group files under `#`/`##` headers, a single-file
//! result lists them under `[path#TAG]` headers, a matched row is `*LINE:text` (continuation
//! lines of a multi-line match get a leading space), and parse problems are deduplicated and
//! capped instead of aborting the run.

use std::collections::BTreeSet;
use std::fmt::Write as _;

/// Longest rendered before/after line in an `ast_edit` preview.
pub const PREVIEW_LINE_CHARS: usize = 120;
/// Most parse issues one result shows.
pub const PARSE_ERRORS_LIMIT: usize = 20;

/// One file's rendered body under its header.
pub struct Section {
    /// The file's current content tag, when the file is readable.
    pub tag: Option<String>,
    /// The per-file count shown in a header (`ast_edit` only).
    pub count: Option<usize>,
    /// The file's body lines.
    pub lines: Vec<String>,
}

/// Grouped tree of a directory scope: the shared directory prefix folded into one `#` header,
/// then `## path#TAG (N)` per file, each followed by its section.
#[must_use]
pub fn grouped(files: &[String], render: impl Fn(&str) -> Section) -> String {
    let prefix = common_dir(files);
    let header = if prefix.is_empty() { "#" } else { "##" };
    let mut out = String::new();
    if !prefix.is_empty() {
        let _ = writeln!(out, "# {prefix}");
    }
    for path in files {
        let section = render(path);
        if section.lines.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let rest = path.strip_prefix(&prefix).unwrap_or(path);
        let _ = write!(out, "{header} {rest}");
        write_tail(&mut out, &section);
        out.push('\n');
        for line in &section.lines {
            let _ = writeln!(out, "{line}");
        }
    }
    out.trim_end().to_owned()
}

/// A flat list of files separated by blank lines, each headed by `[path#TAG]` when taggable.
#[must_use]
pub fn flat(files: &[String], render: impl Fn(&str) -> Section) -> String {
    let mut out = String::new();
    for path in files {
        let section = render(path);
        if section.lines.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        match &section.tag {
            Some(tag) => {
                let _ = write!(out, "[{path}#{tag}]");
                if let Some(count) = section.count {
                    let _ = write!(out, " ({count} replacement{})", plural(count));
                }
                out.push('\n');
            }
            None => {
                let _ = writeln!(out, "{path}");
            }
        }
        for line in &section.lines {
            let _ = writeln!(out, "{line}");
        }
    }
    out.trim_end().to_owned()
}

fn write_tail(out: &mut String, section: &Section) {
    if let Some(tag) = &section.tag {
        let _ = write!(out, "#{tag}");
    }
    if let Some(count) = section.count {
        let _ = write!(out, " ({count} replacement{})", plural(count));
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The directory shared by every `path`, with a trailing `/` (`""` when they meet only at the
/// root, or any path is bare).
fn common_dir(files: &[String]) -> String {
    let mut prefix = String::new();
    for (i, path) in files.iter().enumerate() {
        let dir = path.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
        prefix = if i == 0 {
            dir
        } else {
            let cut = common_prefix_len(prefix.as_bytes(), dir.as_bytes());
            prefix.get(..cut).map_or(String::new(), |head| {
                head.rsplit_once('/')
                    .map_or(String::new(), |(d, _)| d.to_owned())
            })
        };
        if prefix.is_empty() {
            return String::new();
        }
    }
    if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    }
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// `*LINE:text` for the first line of a match, ` LINE:text` for the rest.
#[must_use]
pub fn match_lines(start_line: usize, text: &str) -> Vec<String> {
    text.split('\n')
        .enumerate()
        .filter_map(|(i, line)| {
            let line_no = start_line.checked_add(i)?;
            let marker = if i == 0 { "*" } else { " " };
            Some(format!("{marker}{line_no}:{line}"))
        })
        .collect()
}

/// The collected parse issues, deduplicated and capped; the shown list and the full count.
#[must_use]
pub fn parse_errors(errors: &[String]) -> (Vec<String>, usize) {
    let unique: BTreeSet<&str> = errors.iter().map(String::as_str).collect();
    let total = unique.len();
    let mut shown: Vec<String> = unique
        .into_iter()
        .take(PARSE_ERRORS_LIMIT)
        .map(str::to_owned)
        .collect();
    shown.sort_unstable();
    (shown, total)
}

/// The `Parse issues:` block over the shown errors of [`parse_errors`].
#[must_use]
pub fn parse_errors_block(show: &[String], total: usize) -> String {
    let mut block = String::new();
    if show.len() < total {
        let _ = writeln!(block, "Parse issues ({} / {total}):", show.len());
    } else {
        block.push_str("Parse issues:\n");
    }
    for err in show {
        let _ = writeln!(block, "- {err}");
    }
    block.trim_end().to_owned()
}

/// The first line of `text`, clipped to [`PREVIEW_LINE_CHARS`] characters.
#[must_use]
pub fn first_line(text: &str) -> String {
    let line = text.split('\n').next().unwrap_or_default();
    match line.char_indices().nth(PREVIEW_LINE_CHARS) {
        Some((cut, _)) => line.get(..cut).unwrap_or_default().to_owned(),
        None => line.to_owned(),
    }
}
