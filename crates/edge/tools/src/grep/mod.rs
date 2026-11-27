//! In-process ripgrep-style `grep` tool: omp's contract (`;` path lists with `!glob`
//! exclusions, `file:N-M` selectors, cross-line patterns, `skip` file pages, grouped output).

use crate::group::{self, Entry, Style};
use crate::paths::display_rel;
use crate::scope::{Scope, Target, walk_target};
use crate::selector::Selector;
use crate::walk::WalkFlags;
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use hrn_kernel::{CancelFlag, HrnError, Result, ToolOutput};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// Files per result page (`skip` advances by this).
const FILE_PAGE: usize = 20;
/// Matches kept per file when several files are searched / when exactly one is.
const MULTI_FILE_MATCHES: usize = 10;
const SINGLE_FILE_MATCHES: usize = 50;
/// Matches after which the scan stops counting more files.
const TOTAL_CAP: usize = 2000;
/// Bytes of one emitted line (head plus the `...` markers).
const MAX_LINE_BYTES: usize = 512;
const SEARCH_BUDGET: Duration = Duration::from_secs(30);
const TAG_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// What the model asked for.
pub struct GrepRequest<'a> {
    /// The regex (or literal text when the regex engine rejects it).
    pub pattern: &'a str,
    /// The `;`-separated files, directories and globs (default `.`).
    pub path: Option<&'a str>,
    /// Case-sensitive matching.
    pub case_sensitive: bool,
    /// Respect `.gitignore` during directory scans.
    pub gitignore: bool,
    /// Matching files to skip (the page offset).
    pub skip: usize,
}

/// How hits are rendered (set by the module, not the model).
#[derive(Clone, Copy)]
pub struct GrepStyle {
    /// Hashline rows (`*5:text` under `[path#TAG]`) rather than plain (`*5|text`).
    pub hashline: bool,
    /// Context lines shown before each match (omp's `grep.contextBefore`).
    pub context_before: usize,
    /// Context lines shown after each match (omp's `grep.contextAfter`).
    pub context_after: usize,
}

/// Matches (and context) collected from one file.
struct FileHits {
    rows: Vec<String>,
    matches: usize,
    capped: bool,
}

struct Collector<'a> {
    matcher: &'a RegexMatcher,
    sep: char,
    cap: usize,
    windows: &'a [(usize, usize)],
    hits: FileHits,
    last: u64,
}

impl Collector<'_> {
    fn wanted(&self, line: u64) -> bool {
        self.windows.is_empty()
            || usize::try_from(line)
                .is_ok_and(|n| self.windows.iter().any(|&(a, b)| (a..=b).contains(&n)))
    }

    fn push(&mut self, line: u64, text: &[u8], is_match: bool) {
        if !self.wanted(line) {
            return;
        }
        if self.last != 0 && line > self.last + 1 {
            self.hits.rows.push("...".to_owned());
        }
        self.last = line;
        let text = String::from_utf8_lossy(text);
        let text = text.trim_end_matches(['\r', '\n']);
        let col = if is_match && text.len() > MAX_LINE_BYTES {
            self.matcher
                .find(text.as_bytes())
                .ok()
                .flatten()
                .map(|m| m.start())
        } else {
            None
        };
        let (shown, col) = cut_line(text, col);
        let mark = if is_match { '*' } else { ' ' };
        let suffix = col.map_or_else(String::new, |c| format!(" [col {c}]"));
        self.hits
            .rows
            .push(format!("{mark}{line}{}{shown}{suffix}", self.sep));
    }
}

impl Sink for Collector<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> io::Result<bool> {
        let first = mat.line_number().unwrap_or(0);
        if !self.wanted(first)
            && !(first..first + mat.lines().count() as u64).any(|n| self.wanted(n))
        {
            return Ok(true);
        }
        if self.hits.matches >= self.cap {
            self.hits.capped = true;
            return Ok(false);
        }
        self.hits.matches += 1;
        for (i, line) in mat.lines().enumerate() {
            self.push(first + i as u64, line, true);
        }
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> io::Result<bool> {
        if matches!(ctx.kind(), SinkContextKind::Before | SinkContextKind::After) {
            self.push(ctx.line_number().unwrap_or(0), ctx.bytes(), false);
        }
        Ok(true)
    }
}

/// `line` cut to [`MAX_LINE_BYTES`]: its head when the match starts there, else a window around
/// the match (the 1-indexed character column of the match is returned for the `[col N]` tag).
fn cut_line(line: &str, match_at: Option<usize>) -> (String, Option<usize>) {
    if line.len() <= MAX_LINE_BYTES {
        return (line.to_owned(), None);
    }
    let boundary = |mut at: usize| {
        while !line.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let budget = MAX_LINE_BYTES - 3;
    match match_at {
        Some(at) if at >= budget - 32 => {
            let start = boundary(at.saturating_sub(64));
            let end = boundary((start + MAX_LINE_BYTES - 6).min(line.len()));
            let col = line.get(..at).map_or(0, |s| s.chars().count()) + 1;
            let tail = if end < line.len() { "..." } else { "" };
            (
                format!("...{}{tail}", line.get(start..end).unwrap_or_default()),
                Some(col),
            )
        }
        _ => (
            format!("{}...", line.get(..boundary(budget)).unwrap_or_default()),
            None,
        ),
    }
}

/// Escape `{` / `}` that do not form a quantifier like `{2}`, `{2,}` or `{2,4}`.
fn escape_stray_braces(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len());
    let (mut i, mut in_class, mut in_quantifier) = (0, false, false);
    while let Some(&c) = chars.get(i) {
        match c {
            '\\' => {
                out.push(c);
                if let Some(&next) = chars.get(i + 1) {
                    out.push(next);
                }
                i += 2;
                continue;
            }
            '[' => in_class = true,
            ']' => in_class = false,
            '{' if !in_class => {
                let close = chars
                    .iter()
                    .skip(i)
                    .position(|&ch| ch == '}')
                    .map(|p| p + i);
                in_quantifier = close.is_some_and(|end| {
                    let body: String = chars.get(i + 1..end).unwrap_or_default().iter().collect();
                    let (a, b) = body.split_once(',').unwrap_or((&body, ""));
                    !a.is_empty()
                        && a.chars().all(|d| d.is_ascii_digit())
                        && b.chars().all(|d| d.is_ascii_digit())
                });
                if !in_quantifier {
                    out.push('\\');
                }
            }
            '}' if !in_class => {
                if in_quantifier {
                    in_quantifier = false;
                } else {
                    out.push('\\');
                }
            }
            _ => {}
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Escape unescaped parentheses outside character classes.
fn escape_parens(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let (mut escaped, mut in_class) = (false, false);
    for c in pattern.chars() {
        if !escaped && !in_class && (c == '(' || c == ')') {
            out.push('\\');
        }
        match c {
            '\\' if !escaped => escaped = true,
            '[' if !escaped => {
                in_class = true;
                escaped = false;
            }
            ']' if !escaped => {
                in_class = false;
                escaped = false;
            }
            _ => escaped = false,
        }
        out.push(c);
    }
    out
}

fn escape_literal(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() * 2);
    for c in pattern.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The matcher for `pattern`; a pattern the regex engine rejects is retried with stray braces
/// and parentheses escaped and finally as literal text, and the returned note says so.
fn build_matcher(
    pattern: &str,
    case_sensitive: bool,
    multiline: bool,
) -> Result<(RegexMatcher, Option<String>)> {
    let build = |p: &str| {
        let mut b = RegexMatcherBuilder::new();
        b.case_insensitive(!case_sensitive).multi_line(true);
        if !multiline {
            b.line_terminator(Some(b'\n'));
        }
        b.build(p)
    };
    let first = match build(pattern) {
        Ok(m) => return Ok((m, None)),
        Err(e) => e,
    };
    let braces = escape_stray_braces(pattern);
    let attempts = [
        braces.clone(),
        escape_parens(&braces),
        escape_literal(pattern),
    ];
    for (n, attempt) in attempts.iter().enumerate() {
        if let Ok(m) = build(attempt) {
            let how = if n == 2 {
                "searched as literal text"
            } else {
                "searched with stray braces/parentheses escaped"
            };
            let reason = first.to_string();
            let reason = reason.lines().last().unwrap_or_default().trim();
            return Ok((
                m,
                Some(format!("Pattern is not a valid regex ({reason}); {how}.")),
            ));
        }
    }
    Err(HrnError::Tool(format!(
        "Invalid regex `{pattern}`: {first}"
    )))
}

fn searcher_for(multiline: bool, explicit_file: bool, style: GrepStyle) -> Searcher {
    SearcherBuilder::new()
        .line_number(true)
        .multi_line(multiline)
        .before_context(style.context_before)
        .after_context(style.context_after)
        .binary_detection(if explicit_file {
            BinaryDetection::none()
        } else {
            BinaryDetection::quit(0)
        })
        .build()
}

/// Line windows of a `file:N-M` target (`None` for no selector); `Some(empty)` when the
/// selector lies past the end.
fn windows_of(target: &Target, path: &Path) -> Option<Vec<(usize, usize)>> {
    let sel: &Selector = &target.selector;
    if sel.spans.is_empty() {
        return None;
    }
    let total = fs::read(path).map_or(0, |b| bytecount_lines(&b));
    Some(sel.windows(total, total.max(1)))
}

#[expect(
    clippy::naive_bytecount,
    reason = "runs once per explicitly selected file, not on the hot path"
)]
fn bytecount_lines(bytes: &[u8]) -> usize {
    let n = bytes.iter().filter(|&&b| b == b'\n').count();
    n + usize::from(bytes.last().is_some_and(|&b| b != b'\n'))
}

/// The `[path#TAG]` snapshot tag of a file, as `read` computes it.
fn tag_for(path: &Path) -> Option<String> {
    if fs::metadata(path).ok()?.len() > TAG_MAX_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    Some(crate::hashline::tag_of(
        text.strip_prefix('\u{feff}').unwrap_or(&text),
    ))
}

/// One scanned file of a result page.
struct Hit {
    shown: String,
    path: std::path::PathBuf,
    hits: FileHits,
}

/// What the scan collected: the current page, how many files matched in total and how many
/// matches those hold, and which files were cut at the per-file cap.
struct Scan {
    page: Vec<Hit>,
    files: usize,
    matches: usize,
    capped: Vec<String>,
}

/// Search the scope named by `req.path` for `req.pattern`.
///
/// # Errors
/// An empty or unusable pattern, an unparseable or missing `path`, a scan past 30 seconds, or
/// cancellation.
pub fn run_grep(
    cwd: &Path,
    req: &GrepRequest<'_>,
    style: GrepStyle,
    cancel: &CancelFlag,
) -> Result<ToolOutput> {
    if req.pattern.trim().is_empty() {
        return Err(HrnError::Tool("Pattern must not be empty".into()));
    }
    let multiline = req.pattern.contains('\n') || req.pattern.contains("\\n");
    let (matcher, pattern_note) = build_matcher(req.pattern, req.case_sensitive, multiline)?;
    let scope = Scope::parse(cwd, req.path, true)?;
    let single_file =
        scope.targets.first().is_some_and(Target::is_file) && scope.targets.len() == 1;
    let scan = scan(
        cwd,
        req,
        style,
        &scope,
        &matcher,
        multiline,
        single_file,
        cancel,
    )?;
    let mut sections = render(scan, req.skip, style, single_file);
    if !scope.missing.is_empty() {
        sections.push(format!(
            "Skipped missing paths: {}",
            scope.missing.join(", ")
        ));
    }
    sections.extend(pattern_note);
    Ok(ToolOutput::ok(sections.join("\n\n")))
}

/// Walk the scope and collect the page `req.skip` points at.
#[expect(
    clippy::too_many_arguments,
    reason = "one call site; the request is already destructured"
)]
fn scan(
    cwd: &Path,
    req: &GrepRequest<'_>,
    style: GrepStyle,
    scope: &Scope,
    matcher: &RegexMatcher,
    multiline: bool,
    single_file: bool,
    cancel: &CancelFlag,
) -> Result<Scan> {
    let cap = if single_file {
        SINGLE_FILE_MATCHES
    } else {
        MULTI_FILE_MATCHES
    };
    let flags = WalkFlags {
        gitignore: req.gitignore,
        hidden: true,
    };
    let sep = if style.hashline { ':' } else { '|' };
    let started = Instant::now();
    let mut seen = HashSet::new();
    let mut scan = Scan {
        page: Vec::new(),
        files: 0,
        matches: 0,
        capped: Vec::new(),
    };
    'scan: for target in &scope.targets {
        let mut searcher = searcher_for(multiline, target.is_file(), style);
        for (entry, rel) in walk_target(target, cwd, &scope.excludes, flags, &mut seen) {
            if cancel.is_cancelled() {
                return Err(HrnError::Cancelled);
            }
            if started.elapsed() >= SEARCH_BUDGET {
                return Err(HrnError::Tool(
                    "Grep timed out after 30s; narrow paths or pattern, or scope with `glob` first"
                        .into(),
                ));
            }
            if !entry.file_type().is_some_and(|t| t.is_file()) || !target.matches(&rel) {
                continue;
            }
            let path = entry.path();
            let windows = windows_of(target, path).unwrap_or_default();
            if !target.selector.spans.is_empty() && windows.is_empty() {
                continue;
            }
            let mut sink = Collector {
                matcher,
                sep,
                cap,
                windows: &windows,
                hits: FileHits {
                    rows: Vec::new(),
                    matches: 0,
                    capped: false,
                },
                last: 0,
            };
            // Unreadable files are skipped like omp's native scan does.
            if searcher.search_path(matcher, path, &mut sink).is_err() || sink.hits.matches == 0 {
                continue;
            }
            let shown = display_rel(cwd, path);
            scan.matches += sink.hits.matches;
            if sink.hits.capped {
                scan.capped.push(shown.clone());
            }
            let want_page = single_file || (scan.files >= req.skip && scan.page.len() < FILE_PAGE);
            if want_page {
                scan.page.push(Hit {
                    shown,
                    path: path.to_path_buf(),
                    hits: sink.hits,
                });
            }
            scan.files += 1;
            if scan.matches >= TOTAL_CAP {
                break 'scan;
            }
        }
    }
    Ok(scan)
}

/// The result sections: the page itself and the footers that explain what is not shown.
fn render(scan: Scan, skip: usize, style: GrepStyle, single_file: bool) -> Vec<String> {
    let Scan {
        page,
        files: total_files,
        matches: total_matches,
        mut capped,
    } = scan;
    let mut sections: Vec<String> = Vec::new();
    if page.is_empty() {
        sections.push(if total_files > 0 && !single_file {
            format!("No more results ({total_files} files total; skip={skip} is past the end)")
        } else {
            "No matches found".to_owned()
        });
        return sections;
    }
    if single_file {
        let Some(hit) = page.first() else {
            return sections;
        };
        let tag = if style.hashline {
            tag_for(&hit.path).map_or_else(String::new, |t| format!("#{t}"))
        } else {
            String::new()
        };
        let mut text = format!("[{}{tag}]", hit.shown);
        for row in &hit.hits.rows {
            let _ = write!(text, "\n{row}");
        }
        sections.push(text);
        if hit.hits.capped {
            sections.push(format!(
                "More than {SINGLE_FILE_MATCHES} matches; narrow the pattern or search a line \
                 range (`{}:200-400`).",
                hit.shown
            ));
        }
        return sections;
    }
    let shown_count = page.len();
    let entries: Vec<Entry> = page
        .iter()
        .map(|hit| Entry {
            tag: style.hashline.then(|| tag_for(&hit.path)).flatten(),
            path: hit.shown.clone(),
            rows: hit.hits.rows.clone(),
        })
        .collect();
    sections.push(group::grouped(&entries, Style::Files));
    let end = skip + shown_count;
    let more = if total_matches >= TOTAL_CAP { "+" } else { "" };
    if skip > 0 || end < total_files || !more.is_empty() {
        sections.push(format!(
            "Showing files {}-{end} of {total_files}{more}. Use skip={end} for the next page, or \
             narrow paths/pattern.",
            skip + 1
        ));
    }
    capped.retain(|f| entries.iter().any(|e| &e.path == f));
    if !capped.is_empty() {
        sections.push(format!(
            "More than {MULTI_FILE_MATCHES} matches in {}; grep keeps the first \
             {MULTI_FILE_MATCHES} per file when searching several. Search one file alone for up \
             to {SINGLE_FILE_MATCHES}.",
            capped.join(", ")
        ));
    }
    sections
}

#[cfg(test)]
mod tests;
