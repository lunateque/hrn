//! Four-step match ladder behind `edit_file` (spec §4.3 / §11f).
//!
//! 1. exact substring match (or exact whole-line match when `whole_lines`);
//! 2. whole-line match ignoring trailing whitespace (when `fuzzy_match`);
//! 3. whole-line match after normalizing indentation (when `fuzzy_match`);
//! 4. character-similarity fuzzy match at or above `fuzzy_threshold` (when `fuzzy_match`).
//!
//! Every step must match *uniquely* (unless `replace_all`): the first step that matches at all
//! decides. A step with several matches is an error rather than a fall-through, because the later
//! steps are strictly looser and can only match more places. On a total miss the three nearest
//! regions of the file are returned with line numbers so the model can correct itself in one step.

use similar::TextDiff;
use std::collections::HashSet;
use std::fmt::{self, Write as _};
use std::time::Duration;

/// Longest window (in lines) shown / scored for a nearest-snippet hint.
const MAX_WINDOW: usize = 30;
/// Token-overlap candidates re-ranked with the (more expensive) character diff.
const RERANK: usize = 24;
/// Character cap per side for the similarity diff.
const SIMILARITY_CHARS: usize = 2000;
/// Longest list of ambiguous line numbers printed.
const MAX_LISTED_LINES: usize = 8;
/// Default fuzzy similarity threshold (omp `edit.fuzzyThreshold = 0.95`).
pub const DEFAULT_FUZZY_THRESHOLD: f32 = 0.95;

/// Which ladder step produced the match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Exact,
    TrailingWs,
    Indent,
    Fuzzy,
}

impl Step {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact match",
            Self::TrailingWs => "trailing-whitespace-insensitive match",
            Self::Indent => "indentation-normalized match",
            Self::Fuzzy => "fuzzy similarity match",
        }
    }
}

/// Matching options.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Opts {
    /// Replace every (non-overlapping) match instead of requiring a unique one.
    pub(crate) replace_all: bool,
    /// Skip the substring step: `old` must cover whole lines (patch hunks).
    pub(crate) whole_lines: bool,
    /// Allow whitespace, indentation, and similarity fallback steps when exact match misses.
    pub(crate) fuzzy_match: bool,
    /// Minimum character similarity ratio (`0.0..=1.0`) for the fuzzy step.
    pub(crate) fuzzy_threshold: f32,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            replace_all: false,
            whole_lines: false,
            fuzzy_match: true,
            fuzzy_threshold: DEFAULT_FUZZY_THRESHOLD,
        }
    }
}

/// Successful replacement.
#[derive(Debug)]
pub(crate) struct Applied {
    pub(crate) text: String,
    pub(crate) step: Step,
    pub(crate) count: usize,
    /// 1-based inclusive line ranges touched in the original text (for `enforce_seen_lines`).
    pub(crate) matched_ranges: Vec<(usize, usize)>,
}

/// Why the ladder did not produce an edit.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LadderError {
    Ambiguous { step: Step, lines: Vec<usize> },
    NotFound { snippets: String },
}

impl fmt::Display for LadderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ambiguous { step, lines } => {
                let listed: Vec<String> = lines
                    .iter()
                    .take(MAX_LISTED_LINES)
                    .map(usize::to_string)
                    .collect();
                let more = if lines.len() > MAX_LISTED_LINES {
                    ", ..."
                } else {
                    ""
                };
                write!(
                    f,
                    "`old` matches {} places ({}, starting at lines {}{more}); include more \
                     surrounding context to make it unique, or pass `replace_all: true`",
                    lines.len(),
                    step.label(),
                    listed.join(", "),
                )
            }
            Self::NotFound { snippets } => write!(
                f,
                "`old` was not found (tried exact, trailing-whitespace-insensitive, \
                 indentation-normalized and fuzzy matching). Nearest regions of the file:\n{snippets}"
            ),
        }
    }
}

/// Replace `old` with `new` in `body` (both LF-normalized) following the ladder.
pub(crate) fn apply(body: &str, old: &str, new: &str, opts: Opts) -> Result<Applied, LadderError> {
    if !opts.whole_lines {
        let offsets: Vec<usize> = body.match_indices(old).map(|(i, _)| i).collect();
        if !offsets.is_empty() {
            return apply_exact(body, old, new, &offsets, opts.replace_all);
        }
    }
    let body_lines: Vec<&str> = body.split('\n').collect();
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let width = old_lines.len();

    if opts.whole_lines {
        let exact_lines = find_windows(&body_lines, &old_lines, |win, old| win == old);
        if !exact_lines.is_empty() {
            let verbatim =
                |_: usize| -> Vec<String> { new_lines.iter().map(|l| (*l).to_owned()).collect() };
            return apply_windows(
                &body_lines,
                &exact_lines,
                width,
                Step::Exact,
                opts,
                verbatim,
            );
        }
    }

    if !opts.fuzzy_match {
        return Err(LadderError::NotFound {
            snippets: nearest_snippets(&body_lines, &old_lines, 3),
        });
    }

    let trailing = find_windows(&body_lines, &old_lines, |win, old| {
        win.iter()
            .zip(old)
            .all(|(w, o)| w.trim_end() == o.trim_end())
    });
    if !trailing.is_empty() {
        let verbatim =
            |_: usize| -> Vec<String> { new_lines.iter().map(|l| (*l).to_owned()).collect() };
        return apply_windows(
            &body_lines,
            &trailing,
            width,
            Step::TrailingWs,
            opts,
            verbatim,
        );
    }

    let old_min = min_indent(&old_lines);
    let indented = find_windows(&body_lines, &old_lines, |win, old| {
        indent_equivalent(win, old, old_min)
    });
    if !indented.is_empty() {
        let reindent = |start: usize| {
            let window = body_lines.get(start..start + width).unwrap_or_default();
            reindent_new(&new_lines, window, &old_lines, old_min)
        };
        return apply_windows(&body_lines, &indented, width, Step::Indent, opts, reindent);
    }

    let fuzzy = find_fuzzy_windows(&body_lines, &old_lines, opts.fuzzy_threshold);
    if !fuzzy.is_empty() {
        let reindent = |start: usize| {
            let window = body_lines.get(start..start + width).unwrap_or_default();
            reindent_new(&new_lines, window, &old_lines, old_min)
        };
        return apply_windows(&body_lines, &fuzzy, width, Step::Fuzzy, opts, reindent);
    }

    Err(LadderError::NotFound {
        snippets: nearest_snippets(&body_lines, &old_lines, 3),
    })
}

fn apply_exact(
    body: &str,
    old: &str,
    new: &str,
    offsets: &[usize],
    replace_all: bool,
) -> Result<Applied, LadderError> {
    if offsets.len() > 1 && !replace_all {
        let lines = offsets
            .iter()
            .map(|&off| body.get(..off).unwrap_or_default().matches('\n').count() + 1)
            .collect();
        return Err(LadderError::Ambiguous {
            step: Step::Exact,
            lines,
        });
    }
    let line_span = old.strip_suffix('\n').unwrap_or(old).matches('\n').count();
    let chosen_offsets: &[usize] = if replace_all {
        offsets
    } else {
        offsets.get(..1).unwrap_or_default()
    };
    let matched_ranges = chosen_offsets
        .iter()
        .map(|&off| {
            let start = body.get(..off).unwrap_or_default().matches('\n').count() + 1;
            (start, start + line_span)
        })
        .collect();
    let (text, count) = if replace_all {
        (body.replace(old, new), offsets.len())
    } else {
        (body.replacen(old, new, 1), 1)
    };
    Ok(Applied {
        text,
        step: Step::Exact,
        count,
        matched_ranges,
    })
}

/// `old`/`new` as lines: one trailing newline is a terminator, not an extra blank line, and an
/// empty string is *no* lines (a deletion).
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .collect()
}

/// Start indices of every window of `old.len()` lines accepted by `matches`.
fn find_windows(
    body: &[&str],
    old: &[&str],
    matches: impl Fn(&[&str], &[&str]) -> bool,
) -> Vec<usize> {
    let width = old.len();
    if width == 0 || width > body.len() {
        return Vec::new();
    }
    (0..=body.len() - width)
        .filter(|&start| {
            body.get(start..start + width)
                .is_some_and(|win| matches(win, old))
        })
        .collect()
}

/// Non-overlapping windows whose character similarity to `old` meets `threshold` (`0.0..=1.0`).
fn find_fuzzy_windows(body: &[&str], old: &[&str], threshold: f32) -> Vec<usize> {
    let width = old.len();
    if width == 0 || width > body.len() || !threshold.is_finite() {
        return Vec::new();
    }
    let threshold = threshold.clamp(0.0, 1.0);
    let old_text = clipped(old);
    if old_text.trim().is_empty() {
        return Vec::new();
    }
    let candidates = candidate_starts(body, old, width);
    let mut scored: Vec<(f32, usize)> = candidates
        .into_iter()
        .filter_map(|start| {
            let end = (start + width).min(body.len());
            let window = clipped(body.get(start..end).unwrap_or_default());
            if window.trim().is_empty() {
                return None;
            }
            let ratio = TextDiff::configure()
                .timeout(Duration::from_millis(30))
                .diff_chars(old_text.as_str(), window.as_str())
                .ratio();
            (ratio >= threshold).then_some((ratio, start))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut chosen: Vec<usize> = Vec::new();
    for (_, start) in scored {
        if chosen.iter().all(|&prev| prev.abs_diff(start) >= width) {
            chosen.push(start);
        }
    }
    chosen.sort_unstable();
    chosen
}

/// Candidate window start positions: full scan on modest files, token-overlap prefilter on large.
fn candidate_starts(body: &[&str], old: &[&str], width: usize) -> Vec<usize> {
    let last_start = body.len().saturating_sub(width);
    if body.len() <= 200 {
        return (0..=last_start).collect();
    }
    let tokens: HashSet<&str> = old.iter().flat_map(|l| l.split_whitespace()).collect();
    let mut prefix = vec![0usize];
    for line in body {
        let hits = line
            .split_whitespace()
            .filter(|t| tokens.contains(t))
            .count();
        prefix.push(prefix.last().copied().unwrap_or(0) + hits);
    }
    let mut scored: Vec<(usize, usize)> = (0..=last_start)
        .filter_map(|start| {
            let end = (start + width).min(body.len());
            let hits = prefix.get(end)?.saturating_sub(*prefix.get(start)?);
            (hits > 0).then_some((hits, start))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(RERANK);
    scored.into_iter().map(|(_, start)| start).collect()
}

fn apply_windows(
    body: &[&str],
    starts: &[usize],
    width: usize,
    step: Step,
    opts: Opts,
    replacement: impl Fn(usize) -> Vec<String>,
) -> Result<Applied, LadderError> {
    if starts.len() > 1 && !opts.replace_all {
        return Err(LadderError::Ambiguous {
            step,
            lines: starts.iter().map(|s| s + 1).collect(),
        });
    }
    // Overlapping windows cannot both be replaced; keep the earlier one.
    let mut chosen: Vec<usize> = Vec::new();
    for &start in starts {
        if chosen.last().is_none_or(|&prev| start >= prev + width) {
            chosen.push(start);
        }
        if !opts.replace_all {
            break;
        }
    }

    let mut out = String::new();
    let mut first = true;
    let mut push = |line: &str| {
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(line);
    };
    let mut cursor = 0usize;
    for &start in &chosen {
        for line in body.get(cursor..start).unwrap_or_default() {
            push(line);
        }
        for line in replacement(start) {
            push(&line);
        }
        cursor = start + width;
    }
    for line in body.get(cursor..).unwrap_or_default() {
        push(line);
    }
    let matched_ranges = chosen.iter().map(|&s| (s + 1, s + width)).collect();
    Ok(Applied {
        text: out,
        step,
        count: chosen.len(),
        matched_ranges,
    })
}

fn indent_len(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn min_indent(lines: &[&str]) -> usize {
    lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent_len(l))
        .min()
        .unwrap_or(0)
}

fn indent_equivalent(win: &[&str], old: &[&str], old_min: usize) -> bool {
    let win_min = min_indent(win);
    win.iter().zip(old).all(|(w, o)| {
        let (w_blank, o_blank) = (w.trim().is_empty(), o.trim().is_empty());
        if w_blank || o_blank {
            return w_blank && o_blank;
        }
        w.trim() == o.trim() && indent_len(w) - win_min == indent_len(o) - old_min
    })
}

/// Re-express `new` (written in `old`'s indentation frame) in the matched window's frame.
fn reindent_new(new: &[&str], window: &[&str], old: &[&str], old_min: usize) -> Vec<String> {
    let win_min = min_indent(window);
    let win_indent = window
        .iter()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| l.get(..win_min))
        .unwrap_or_default();
    let old_indent = old
        .iter()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| l.get(..old_min))
        .unwrap_or_default();
    new.iter()
        .map(|line| {
            if line.trim().is_empty() {
                return String::new();
            }
            let rest = line
                .strip_prefix(old_indent)
                .unwrap_or_else(|| line.trim_start_matches([' ', '\t']));
            format!("{win_indent}{rest}")
        })
        .collect()
}

fn clipped(lines: &[&str]) -> String {
    let joined = lines
        .iter()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    joined.chars().take(SIMILARITY_CHARS).collect()
}

/// Render up to `top_k` near-miss windows meeting `min_ratio` (used by hashline near-miss hints).
pub(crate) fn nearest_above_threshold(
    body: &[&str],
    needle: &[&str],
    min_ratio: f32,
    top_k: usize,
) -> Option<String> {
    let width = needle.len().clamp(1, MAX_WINDOW);
    if body.is_empty() || needle.is_empty() || width > body.len() {
        return None;
    }
    let starts = find_fuzzy_windows(body, needle, min_ratio);
    if starts.is_empty() {
        return None;
    }
    let mut out = String::new();
    for start in starts.into_iter().take(top_k) {
        let end = (start + width).min(body.len());
        let _ = writeln!(out, "--- near-miss lines {}-{end} ---", start + 1);
        for (i, line) in body.get(start..end).unwrap_or_default().iter().enumerate() {
            let _ = writeln!(out, "{}:{line}", start + i + 1);
        }
    }
    Some(out)
}

/// The `top_k` regions of `body` most similar to `old`, rendered with line numbers.
pub(crate) fn nearest_snippets(body: &[&str], old: &[&str], top_k: usize) -> String {
    if body.iter().all(|l| l.trim().is_empty()) {
        return "(file is empty)".to_owned();
    }
    let width = old.len().clamp(1, MAX_WINDOW);
    let tokens: HashSet<&str> = old.iter().flat_map(|l| l.split_whitespace()).collect();

    // Stage 1: cheap token overlap through prefix sums.
    let mut prefix = vec![0usize];
    for line in body {
        let hits = line
            .split_whitespace()
            .filter(|t| tokens.contains(t))
            .count();
        prefix.push(prefix.last().copied().unwrap_or(0) + hits);
    }
    let last_start = body.len().saturating_sub(width);
    let mut candidates: Vec<(usize, usize)> = (0..=last_start)
        .filter_map(|start| {
            let end = (start + width).min(body.len());
            let hits = prefix.get(end)?.saturating_sub(*prefix.get(start)?);
            (hits > 0).then_some((hits, start))
        })
        .collect();
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    candidates.truncate(RERANK);
    if candidates.is_empty() {
        return "(no similar lines found)".to_owned();
    }

    // Stage 2: character-level similarity of the surviving windows.
    let old_text = clipped(old);
    let mut scored: Vec<(f32, usize)> = candidates
        .iter()
        .map(|&(_, start)| {
            let end = (start + width).min(body.len());
            let window = clipped(body.get(start..end).unwrap_or_default());
            let ratio = TextDiff::configure()
                .timeout(Duration::from_millis(30))
                .diff_chars(old_text.as_str(), window.as_str())
                .ratio();
            (ratio, start)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));

    let mut chosen: Vec<(f32, usize)> = Vec::new();
    for (ratio, start) in scored {
        if chosen
            .iter()
            .all(|&(_, prev)| prev.abs_diff(start) >= width)
        {
            chosen.push((ratio, start));
            if chosen.len() >= top_k {
                break;
            }
        }
    }

    let mut out = String::new();
    for (ratio, start) in chosen {
        let end = (start + width).min(body.len());
        let _ = writeln!(
            out,
            "--- lines {}-{end} ({:.0}% similar) ---",
            start + 1,
            ratio * 100.0
        );
        for (i, line) in body.get(start..end).unwrap_or_default().iter().enumerate() {
            let _ = writeln!(out, "{:>5}| {line}", start + i + 1);
        }
    }
    out
}

#[cfg(test)]
#[path = "edit_ladder_tests.rs"]
mod tests;
