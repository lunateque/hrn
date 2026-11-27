//! The prefilter of the semantic `find` tool: a keyword scan over the live tree, no index. It
//! ranks files by how many of the keywords they hold (rarer keywords weigh more), cuts the best
//! files into line-numbered excerpts around the matches, and hands those to a model to score.

use crate::paths::display_rel;
use crate::walk::walker;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Most distinct keywords one scan tracks (one bit each in a `u32` mask).
const MAX_KEYWORDS: usize = 16;
const MAX_VISITED_FILES: usize = 20_000;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SCAN_TIME: Duration = Duration::from_secs(6);
const MAX_HITS_PER_FILE: usize = 4_000;
const LINE_CHARS: usize = 200;
const HINT_CHARS: usize = 120;
/// Lines of context an excerpt keeps above its first match.
const LEAD_LINES: usize = 3;

/// How much of the tree the prefilter keeps.
#[derive(Clone, Copy, Debug)]
pub struct FindOpts {
    /// Files kept after ranking.
    pub max_files: usize,
    /// Longest excerpt, in lines.
    pub max_chunk_lines: usize,
    /// Excerpts kept across all files.
    pub max_chunks: usize,
}

/// One excerpt of a file.
#[derive(Clone, Debug)]
pub struct Chunk {
    /// Path as shown to the model (relative to the session directory when inside it).
    pub path: String,
    /// First line, 1-based.
    pub start: usize,
    /// Last line, inclusive.
    pub end: usize,
    /// The lines as `<number>: <text>`, one per line.
    pub text: String,
    /// The line that best shows why the excerpt matched.
    pub hint: String,
}

/// What the prefilter found.
#[derive(Debug, Default)]
pub struct Prefilter {
    /// Excerpts, best prefilter score first.
    pub chunks: Vec<Chunk>,
    /// Files looked at.
    pub scanned: usize,
    /// Files that held at least one keyword (in their text or their path).
    pub matched: usize,
    /// Files that could not be read.
    pub unreadable: usize,
    /// Why the scan stopped before the whole tree, if it did.
    pub stopped: Option<&'static str>,
}

struct Candidate {
    path: PathBuf,
    /// `(line, keyword mask)` of every matching line, in line order.
    hits: Vec<(usize, u32)>,
    counts: [u32; MAX_KEYWORDS],
    path_mask: u32,
}

impl Candidate {
    fn has(&self, k: usize) -> bool {
        self.counts.get(k).is_some_and(|c| *c > 0) || self.path_mask & (1 << k) != 0
    }
}

/// Lowercased, deduplicated keywords, at most [`MAX_KEYWORDS`].
fn normalize_keywords(raw: &[String]) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in raw {
        let word = word.trim().to_lowercase();
        if !word.is_empty() && !words.contains(&word) {
            words.push(word);
        }
    }
    words.truncate(MAX_KEYWORDS);
    words
}

fn escape(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 4);
    for c in word.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn mask_of(line: &str, words: &[String]) -> (u32, [u32; MAX_KEYWORDS]) {
    let lower = line.to_lowercase();
    let mut mask = 0;
    let mut counts = [0; MAX_KEYWORDS];
    for (k, word) in words.iter().enumerate() {
        let n = lower.matches(word.as_str()).count();
        if n > 0 {
            mask |= 1 << k;
            if let Some(slot) = counts.get_mut(k) {
                *slot = u32::try_from(n).unwrap_or(u32::MAX);
            }
        }
    }
    (mask, counts)
}

/// Scan `root` for files holding any of `words`.
fn scan(
    root: &Path,
    cwd: &Path,
    words: &[String],
    cancel: &CancelFlag,
) -> Result<(Vec<Candidate>, Prefilter)> {
    let pattern = words
        .iter()
        .map(|w| escape(w))
        .collect::<Vec<_>>()
        .join("|");
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(true)
        .build(&pattern)
        .map_err(|e| HrnError::Tool(format!("find: invalid keywords: {e}")))?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build();
    let started = Instant::now();
    let mut found = Prefilter::default();
    let mut candidates = Vec::new();
    for entry in walker(root).build().flatten() {
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        found.scanned += 1;
        if found.scanned > MAX_VISITED_FILES {
            found.stopped = Some("file scan limit reached");
            break;
        }
        if found.scanned.is_multiple_of(256) && started.elapsed() >= MAX_SCAN_TIME {
            found.stopped = Some("scan time limit reached");
            break;
        }
        if entry.metadata().is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let path = entry.path();
        let shown = display_rel(cwd, path).to_lowercase();
        let (path_mask, _) = mask_of(&shown, words);
        let mut hits: Vec<(usize, u32)> = Vec::new();
        let mut counts = [0u32; MAX_KEYWORDS];
        let outcome = searcher.search_path(
            &matcher,
            path,
            UTF8(|lineno, line| {
                let (mask, per_line) = mask_of(line, words);
                for (total, add) in counts.iter_mut().zip(per_line) {
                    *total = total.saturating_add(add);
                }
                hits.push((usize::try_from(lineno).unwrap_or(usize::MAX), mask));
                Ok(hits.len() < MAX_HITS_PER_FILE)
            }),
        );
        if outcome.is_err() {
            found.unreadable += 1;
            continue;
        }
        if hits.is_empty() && path_mask == 0 {
            continue;
        }
        candidates.push(Candidate {
            path: path.to_path_buf(),
            hits,
            counts,
            path_mask,
        });
    }
    found.matched = candidates.len();
    Ok((candidates, found))
}

/// Weight of each keyword: the fewer files hold it, the more it says.
fn weights(candidates: &[Candidate], visited: usize, n: usize) -> Vec<f64> {
    let total = f64::from(u32::try_from(visited).unwrap_or(u32::MAX));
    (0..n)
        .map(|k| {
            let df = candidates.iter().filter(|c| c.has(k)).count();
            let df = f64::from(u32::try_from(df).unwrap_or(u32::MAX));
            ((total + 1.0) / (df + 1.0)).ln() + 1.0
        })
        .collect()
}

fn file_score(c: &Candidate, w: &[f64]) -> f64 {
    w.iter()
        .enumerate()
        .map(|(k, weight)| {
            let count = c.counts.get(k).copied().unwrap_or(0);
            let text = if count > 0 {
                weight * (1.0 + f64::from(count).ln_1p())
            } else {
                0.0
            };
            let path = if c.path_mask & (1 << k) != 0 {
                2.0 * weight
            } else {
                0.0
            };
            text + path
        })
        .sum()
}

/// 1-based inclusive `(start, end)` windows of at most `max_lines` covering every hit.
fn windows(c: &Candidate, total: usize, max_lines: usize) -> Vec<(usize, usize)> {
    if total == 0 {
        return Vec::new();
    }
    if c.hits.is_empty() {
        return vec![(1, max_lines.min(total))];
    }
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut covered = 0;
    for (line, _) in &c.hits {
        if *line <= covered || *line > total {
            continue;
        }
        let start = line.saturating_sub(LEAD_LINES.min(max_lines / 4)).max(1);
        let end = (start + max_lines - 1).min(total);
        out.push((start, end));
        covered = end;
    }
    out
}

fn clip(line: &str, max: usize) -> String {
    let line = line.trim_end();
    match line.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", line.get(..cut).unwrap_or_default()),
        None => line.to_owned(),
    }
}

fn chunks_of(
    c: &Candidate,
    cwd: &Path,
    w: &[f64],
    max_lines: usize,
) -> std::io::Result<Vec<(f64, Chunk)>> {
    let text = std::fs::read_to_string(&c.path)?;
    let lines: Vec<&str> = text.lines().collect();
    let shown = display_rel(cwd, &c.path);
    let mut out = Vec::new();
    for (start, end) in windows(c, lines.len(), max_lines) {
        let inside = c.hits.iter().filter(|(l, _)| (start..=end).contains(l));
        let (mask, count) = inside.clone().fold((0u32, 0u32), |(m, n), (_, hit)| {
            (m | hit, n.saturating_add(1))
        });
        let mut score = 0.05 * f64::from(count);
        for (k, weight) in w.iter().enumerate() {
            if mask & (1 << k) != 0 || (c.hits.is_empty() && c.path_mask & (1 << k) != 0) {
                score += weight;
            }
        }
        let best = inside
            .max_by_key(|(l, hit)| (hit.count_ones(), usize::MAX - l))
            .map_or(start, |(l, _)| *l);
        let hint = lines
            .get(best.saturating_sub(1))
            .map(|l| clip(l.trim(), HINT_CHARS))
            .unwrap_or_default();
        let body = lines
            .iter()
            .enumerate()
            .skip(start - 1)
            .take(end - start + 1)
            .map(|(i, l)| format!("{}: {}", i + 1, clip(l, LINE_CHARS)))
            .collect::<Vec<_>>()
            .join("\n");
        out.push((
            score,
            Chunk {
                path: shown.clone(),
                start,
                end,
                text: body,
                hint,
            },
        ));
    }
    Ok(out)
}

/// Scan `root` for `keywords` and cut the best files into excerpts.
///
/// # Errors
///
/// When `root` does not exist, the keywords are unusable, or the call is cancelled.
pub fn prefilter(
    root: &Path,
    cwd: &Path,
    keywords: &[String],
    opts: FindOpts,
    cancel: &CancelFlag,
) -> Result<Prefilter> {
    if !root.exists() {
        return Err(HrnError::Tool(format!(
            "path `{}` does not exist",
            display_rel(cwd, root)
        )));
    }
    let words = normalize_keywords(keywords);
    if words.is_empty() {
        return Err(HrnError::Tool("find: no keywords to search for".to_owned()));
    }
    let max_lines = opts.max_chunk_lines.max(4);
    let (mut candidates, mut found) = scan(root, cwd, &words, cancel)?;
    let w = weights(&candidates, found.scanned, words.len());
    let mut ranked: Vec<(f64, Candidate)> = candidates
        .drain(..)
        .map(|c| (file_score(&c, &w), c))
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
    ranked.truncate(opts.max_files);
    // Collect each file's excerpts best-first, then take them round-robin over the files: one
    // file dense with keywords cannot crowd every other candidate out of the model's view.
    let mut per_file: Vec<Vec<(f64, Chunk)>> = Vec::with_capacity(ranked.len());
    for (_, c) in &ranked {
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        match chunks_of(c, cwd, &w, max_lines) {
            Ok(mut chunks) => {
                chunks.sort_by(|a, b| b.0.total_cmp(&a.0));
                per_file.push(chunks);
            }
            Err(_) => found.unreadable += 1,
        }
    }
    let mut scored: Vec<(f64, Chunk)> = Vec::with_capacity(opts.max_chunks);
    'rounds: for round in 0.. {
        let mut took = false;
        for file in &mut per_file {
            if scored.len() == opts.max_chunks {
                break 'rounds;
            }
            if let Some(chunk) = file.get(round).cloned() {
                scored.push(chunk);
                took = true;
            }
        }
        if !took {
            break;
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(opts.max_chunks);
    found.chunks = scored.into_iter().map(|(_, chunk)| chunk).collect();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    const OPTS: FindOpts = FindOpts {
        max_files: 5,
        max_chunk_lines: 8,
        max_chunks: 10,
    };

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_file_holding_more_of_the_keywords_comes_first_with_its_range() {
        let dir = TempDir::new("find-rank");
        dir.write("a.rs", "// unrelated\nfn deliver() {}\n");
        let mut body = "x\n".repeat(30);
        body.push_str("fn deliver_result_to_parent() {\n    // subagent result\n}\n");
        dir.write("b.rs", &body);
        dir.write(".gitignore", "skip.rs\n");
        dir.write("skip.rs", "subagent deliver result\n");
        let found = prefilter(
            dir.path(),
            dir.path(),
            &words(&["subagent", "deliver", "result"]),
            OPTS,
            &CancelFlag::new(),
        )
        .unwrap();
        let top = found.chunks.first().expect("a chunk");
        assert_eq!(top.path, "b.rs");
        assert!(
            top.start <= 31 && top.end >= 32,
            "{}-{}",
            top.start,
            top.end
        );
        assert!(
            top.hint.contains("deliver_result_to_parent"),
            "{}",
            top.hint
        );
        assert!(found.chunks.iter().all(|c| c.path != "skip.rs"));
    }

    #[test]
    fn regex_metacharacters_in_keywords_are_literal() {
        let dir = TempDir::new("find-escape");
        dir.write("a.txt", "call(foo)\nplain\n");
        let found = prefilter(
            dir.path(),
            dir.path(),
            &words(&["(foo)"]),
            OPTS,
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(found.chunks.len(), 1);
    }
}
