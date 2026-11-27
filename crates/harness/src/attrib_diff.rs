//! Unified diff parsing, range extraction, and hunk filtering for [`EditAttribution`].

use super::{
    EditAttribution, FileAttribution, FilteredDiff, normalize_lexical_rel, normalize_rel_path,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

/// Filter a `git diff` unified diff down to ONLY files and hunks overlapping `attribution`.
///
/// Recomputes each kept hunk's `+new_start` offset accurately when preceding hunks in the same
/// file are dropped so the resulting patch applies cleanly via `git apply --cached`.
pub fn filter_diff_to_attributed(
    unified_diff: &str,
    attribution: &EditAttribution,
) -> FilteredDiff {
    let mut out = FilteredDiff::default();
    if unified_diff.trim().is_empty() || attribution.is_empty() {
        return out;
    }
    for section in split_file_sections(unified_diff) {
        process_file_section(&section, attribution, &mut out);
    }
    out
}

pub(super) fn extract_diff_attributions(cwd: &Path, diff: &str) -> Vec<(String, FileAttribution)> {
    let mut by_path: BTreeMap<String, FileAttribution> = BTreeMap::new();
    for section in split_file_sections(diff) {
        let parsed = parse_file_section(&section);
        let Some(raw_path) = parsed.path else {
            continue;
        };
        let Some(rel) = normalize_rel_path(cwd, &raw_path) else {
            continue;
        };
        let mut attr = FileAttribution {
            whole_file: parsed.is_new_or_deleted,
            ranges: Vec::new(),
        };
        if !attr.whole_file {
            for hunk in &parsed.hunks {
                attr.ranges.extend(hunk_changed_ranges(hunk));
            }
            coalesce_ranges(&mut attr.ranges);
        }
        by_path
            .entry(rel)
            .and_modify(|existing| existing.merge(&attr))
            .or_insert(attr);
    }
    by_path.into_iter().collect()
}

fn process_file_section(section: &[&str], attribution: &EditAttribution, out: &mut FilteredDiff) {
    let parsed = parse_file_section(section);
    let Some(raw_path) = parsed.path else {
        return;
    };
    let rel = normalize_lexical_rel(&raw_path);
    let Some(file_attr) = attribution.files.get(&rel) else {
        out.hunks_skipped += parsed.hunks.len();
        return;
    };

    if parsed.hunks.is_empty() {
        if file_attr.whole_file && !parsed.headers.is_empty() {
            append_lines(&mut out.patch, &parsed.headers);
            push_unique(&mut out.files, rel);
        }
        return;
    }

    let mut kept_hunks: Vec<(String, &[&str])> = Vec::new();
    let mut kept_delta: isize = 0;

    for hunk in &parsed.hunks {
        let keep = file_attr.whole_file || file_attr.matches_changes(&hunk_changed_ranges(hunk));
        if !keep {
            out.hunks_skipped += 1;
            continue;
        }
        out.hunks_kept += 1;
        let rewritten_header = rewrite_hunk_header(hunk, kept_delta);
        let delta = isize::try_from(hunk.new_count).unwrap_or(0)
            - isize::try_from(hunk.old_count).unwrap_or(0);
        kept_delta += delta;
        kept_hunks.push((rewritten_header, &hunk.body));
    }

    if kept_hunks.is_empty() {
        return;
    }
    append_lines(&mut out.patch, &parsed.headers);
    for (header, body) in kept_hunks {
        out.patch.push_str(&header);
        out.patch.push('\n');
        append_lines(&mut out.patch, body);
    }
    push_unique(&mut out.files, rel);
}

fn append_lines(dst: &mut String, lines: &[&str]) {
    for line in lines {
        dst.push_str(line);
        dst.push('\n');
    }
}

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.iter().any(|existing| existing == &item) {
        list.push(item);
    }
}

/// Split a unified diff into per-file sections. Supports both `git diff` (`diff --git ...`)
/// and bare `similar::TextDiff` (`--- a/...` / `+++ b/...`) outputs.
fn split_file_sections(diff: &str) -> Vec<Vec<&str>> {
    let mut sections: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut saw_git_header = false;

    for line in diff.lines() {
        let is_git_start = line.starts_with("diff --git ");
        let is_bare_start =
            !saw_git_header && line.starts_with("--- ") && cur.iter().any(|l| l.starts_with("@@ "));
        if is_git_start {
            saw_git_header = true;
        }
        if (is_git_start || is_bare_start) && !cur.is_empty() {
            sections.push(std::mem::take(&mut cur));
        }
        cur.push(line);
    }
    if !cur.is_empty() {
        sections.push(cur);
    }
    sections
}

struct ParsedHunk<'a> {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    suffix: &'a str,
    body: Vec<&'a str>,
}

struct ParsedSection<'a> {
    path: Option<String>,
    is_new_or_deleted: bool,
    headers: Vec<&'a str>,
    hunks: Vec<ParsedHunk<'a>>,
}

fn parse_file_section<'a>(lines: &[&'a str]) -> ParsedSection<'a> {
    let mut headers: Vec<&'a str> = Vec::new();
    let mut hunks: Vec<ParsedHunk<'a>> = Vec::new();
    let mut old_path: Option<String> = None;
    let mut new_path: Option<String> = None;
    let mut git_b_path: Option<String> = None;
    let mut is_new_or_deleted = false;
    let mut in_hunks = false;

    for &line in lines {
        if let Some(parsed_hdr) = parse_hunk_header(line) {
            in_hunks = true;
            hunks.push(parsed_hdr);
            continue;
        }
        if in_hunks {
            if let Some(last) = hunks.last_mut() {
                last.body.push(line);
            }
            continue;
        }
        headers.push(line);
        if line.starts_with("new file mode ") || line.starts_with("deleted file mode ") {
            is_new_or_deleted = true;
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            git_b_path = parse_git_diff_b_path(rest);
        } else if let Some(rest) = line.strip_prefix("--- ") {
            let p = parse_diff_header_path(rest, "a/");
            if p.is_none() {
                is_new_or_deleted = true;
            }
            old_path = p;
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let p = parse_diff_header_path(rest, "b/");
            if p.is_none() {
                is_new_or_deleted = true;
            }
            new_path = p;
        }
    }

    let path = new_path.or(old_path).or(git_b_path);
    ParsedSection {
        path,
        is_new_or_deleted,
        headers,
        hunks,
    }
}

fn parse_diff_header_path(raw: &str, strip_prefix: &str) -> Option<String> {
    let token = raw.split('\t').next().unwrap_or(raw).trim();
    if token.is_empty() || token == "/dev/null" {
        return None;
    }
    let unquoted = unquote_git_path(token);
    Some(
        unquoted
            .strip_prefix(strip_prefix)
            .unwrap_or(&unquoted)
            .to_owned(),
    )
}

fn parse_git_diff_b_path(rest: &str) -> Option<String> {
    if let Some((_, rhs)) = rest.rsplit_once(" b/") {
        return Some(unquote_git_path(rhs.trim()));
    }
    if let Some((_, rhs)) = rest.rsplit_once(" \"b/") {
        let quoted = format!("\"b/{}", rhs.trim());
        let unquoted = unquote_git_path(&quoted);
        return Some(unquoted.strip_prefix("b/").unwrap_or(&unquoted).to_owned());
    }
    None
}

fn unquote_git_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        let inner = trimmed.get(1..trimmed.len() - 1).unwrap_or("");
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\'
                && let Some(esc) = chars.next()
            {
                match esc {
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    't' => out.push('\t'),
                    'n' => out.push('\n'),
                    other => out.push(other),
                }
            } else {
                out.push(ch);
            }
        }
        return out;
    }
    trimmed.to_owned()
}

fn parse_hunk_header(line: &str) -> Option<ParsedHunk<'_>> {
    let rest = line.strip_prefix("@@ -")?;
    let (counts, suffix) = rest.split_once(" @@")?;
    let (old_part, new_part) = counts.split_once(" +")?;
    let (old_start, old_count) = parse_range_pair(old_part)?;
    let (new_start, new_count) = parse_range_pair(new_part)?;
    Some(ParsedHunk {
        old_start,
        old_count,
        new_start,
        new_count,
        suffix,
        body: Vec::new(),
    })
}

fn parse_range_pair(part: &str) -> Option<(usize, usize)> {
    match part.split_once(',') {
        Some((s, c)) => Some((s.parse().ok()?, c.parse().ok()?)),
        None => Some((part.parse().ok()?, 1)),
    }
}

fn rewrite_hunk_header(hunk: &ParsedHunk<'_>, kept_delta: isize) -> String {
    let new_start = if hunk.new_count == 0 {
        let base = isize::try_from(hunk.old_start.saturating_sub(1)).unwrap_or(0);
        usize::try_from((base + kept_delta).max(0)).unwrap_or(0)
    } else {
        let base = isize::try_from(hunk.old_start).unwrap_or(1);
        usize::try_from((base + kept_delta).max(1)).unwrap_or(1)
    };
    let mut header = String::new();
    let _ = write!(header, "@@ -{}", hunk.old_start);
    if hunk.old_count != 1 {
        let _ = write!(header, ",{}", hunk.old_count);
    }
    let _ = write!(header, " +{new_start}");
    if hunk.new_count != 1 {
        let _ = write!(header, ",{}", hunk.new_count);
    }
    header.push_str(" @@");
    header.push_str(hunk.suffix);
    header
}

/// Extract the exact changed line ranges in post-edit (`+`) coordinates for one hunk,
/// excluding unchanged `' '` context lines so unrelated edits nearby do not falsely match.
fn hunk_changed_ranges(hunk: &ParsedHunk<'_>) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut new_line = hunk.new_start;
    for &line in &hunk.body {
        if line.starts_with('+') {
            let ln = new_line.max(1);
            ranges.push((ln, ln));
            new_line += 1;
        } else if line.starts_with('-') {
            let anchor = new_line.max(1);
            ranges.push((anchor, anchor));
        } else if line.starts_with('\\') {
            // "\ No newline at end of file" does not advance line numbers.
        } else {
            new_line += 1;
        }
    }
    coalesce_ranges(&mut ranges);
    ranges
}

pub(super) fn coalesce_ranges(ranges: &mut Vec<(usize, usize)>) {
    if ranges.len() <= 1 {
        return;
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(start, end) in ranges.iter() {
        if let Some(last) = merged.last_mut()
            && start <= last.1.saturating_add(1)
        {
            last.1 = last.1.max(end);
            continue;
        }
        merged.push((start, end));
    }
    *ranges = merged;
}
