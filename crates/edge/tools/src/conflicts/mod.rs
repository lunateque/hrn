//! Merge-conflict resolution behind `path:conflicts` and `conflict://N`: the parser, registry
//! and views here; [`resolve`] performs the writes.
//!
//! Reading a conflicted file (bare, or with `:conflicts`) registers its hunks in the
//! [`ConflictRegistry`] carried by [`FileReadTracker`]; each hunk keeps a per-file id that
//! survives neighbouring resolutions because ids are re-validated by content.
//! `read conflict://N[/ours|/theirs|/base]` shows a hunk with the file's own line numbers;
//! `write conflict://N` (or `conflict://*`) replaces it, expanding whole-line `@ours` /
//! `@theirs` / `@base` / `@both` tokens.

use crate::fs_read::{self, FileReadTracker};
use crate::paths::display_rel;
use crate::selector::{self, Selector};
use hrn_kernel::{HrnError, Result, ToolOutput};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(crate) const OURS: &str = "<<<<<<<";
pub(crate) const BASE: &str = "|||||||";
pub(crate) const SEP: &str = "=======";
pub(crate) const THEIRS: &str = ">>>>>>>";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    Ours,
    Base,
    Sep,
    Theirs,
}

fn marker_of(line: &str) -> Option<Marker> {
    let (marker, kind) = match line.as_bytes().first() {
        Some(b'<') => (OURS, Marker::Ours),
        Some(b'|') => (BASE, Marker::Base),
        Some(b'=') => (SEP, Marker::Sep),
        Some(b'>') => (THEIRS, Marker::Theirs),
        _ => return None,
    };
    let rest = line.get(marker.len()..)?;
    if kind == Marker::Sep {
        return rest.trim().is_empty().then_some(kind);
    }
    if rest.is_empty() || rest.starts_with([' ', '\t']) {
        Some(kind)
    } else {
        None
    }
}

fn label(line: &str, marker: &str) -> String {
    line.get(marker.len()..)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// One conflict hunk. `start` / `end` are the file's 1-based marker lines.
#[derive(Clone, Debug)]
pub(crate) struct Block {
    pub(crate) id: usize,
    pub(crate) start: usize,
    pub(crate) end: usize,
    ours_label: String,
    theirs_label: String,
    pub(crate) ours: Vec<String>,
    pub(crate) base: Option<Vec<String>>,
    pub(crate) theirs: Vec<String>,
}

impl Block {
    pub(crate) fn same_sides(&self, other: &Block) -> bool {
        self.ours_label == other.ours_label
            && self.theirs_label == other.theirs_label
            && self.ours == other.ours
            && self.base == other.base
            && self.theirs == other.theirs
    }

    /// (`lines`, first line's file number) of one side.
    fn side(&self, name: &str) -> Result<(Vec<String>, usize)> {
        let ours_end = self.start + 1 + self.ours.len();
        match name {
            "ours" => Ok((self.ours.clone(), self.start + 1)),
            "base" => match &self.base {
                Some(base) => Ok((base.clone(), ours_end + 1)),
                None => Err(HrnError::Tool(format!(
                    "conflict {} has no `base` side: the merge was not run with diff3 style",
                    self.id
                ))),
            },
            "theirs" => Ok((self.theirs.clone(), self.end - self.theirs.len())),
            other => Err(HrnError::Tool(format!(
                "`conflict://{}/{other}` takes one of ours, theirs, base",
                self.id
            ))),
        }
    }

    fn summary(&self) -> String {
        let ours = if self.ours_label.is_empty() {
            "ours"
        } else {
            &self.ours_label
        };
        let theirs = if self.theirs_label.is_empty() {
            "theirs"
        } else {
            &self.theirs_label
        };
        let sides = match &self.base {
            Some(base) => format!(
                "{} ours, {} base, {} theirs",
                self.ours.len(),
                base.len(),
                self.theirs.len()
            ),
            None => format!("{} ours, {} theirs", self.ours.len(), self.theirs.len()),
        };
        format!(
            "#{}  L{}-L{}  {ours} vs {theirs}  ({sides})",
            self.id, self.start, self.end
        )
    }
}

/// Parse every complete conflict hunk of `text`.
pub(crate) fn parse(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let at = |i: usize| lines.get(i).copied().unwrap_or_default();
    let slice = |a: usize, b: usize| -> Vec<String> { (a..b).map(|i| at(i).to_owned()).collect() };
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if marker_of(at(i)) != Some(Marker::Ours) {
            i += 1;
            continue;
        }
        let mut base = None;
        let mut sep = None;
        let mut end = None;
        let mut j = i + 1;
        while j < lines.len() {
            match marker_of(at(j)) {
                Some(Marker::Ours) => break,
                Some(Marker::Base) if base.is_none() && sep.is_none() => base = Some(j),
                Some(Marker::Sep) if sep.is_none() => sep = Some(j),
                Some(Marker::Theirs) => {
                    end = Some(j);
                    break;
                }
                _ => j += 1,
            }
        }
        // An unterminated hunk (or one without `=======`) is the model's own text, not a conflict.
        let (Some(sep), Some(end)) = (sep, end) else {
            i += 1;
            continue;
        };
        blocks.push(Block {
            id: 0,
            start: i + 1,
            end: end + 1,
            ours_label: label(at(i), OURS),
            theirs_label: label(at(end), THEIRS),
            ours: slice(i + 1, base.unwrap_or(sep)),
            base: base.map(|b| slice(b + 1, sep)),
            theirs: slice(sep + 1, end),
        });
        i = end + 1;
    }
    blocks
}

/// Registered hunks per file. Ids are kept across re-reads while the hunk's content is
/// unchanged, so resolving `conflict://3` stays meaningful after `conflict://1` was resolved.
#[derive(Default)]
pub(crate) struct ConflictRegistry {
    files: Mutex<HashMap<PathBuf, Vec<Block>>>,
}

impl ConflictRegistry {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Vec<Block>>> {
        self.files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn key_of(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// (Re-)register the hunks of `text` and return them with stable ids.
    pub(crate) fn register(&self, path: &Path, text: &str) -> Vec<Block> {
        let mut blocks = parse(text);
        let mut map = self.map();
        let key = Self::key_of(path);
        let previous = map.remove(&key).unwrap_or_default();
        let mut free: Vec<Block> = previous;
        let mut next_id = free.iter().map(|b| b.id).max().unwrap_or(0) + 1;
        for block in &mut blocks {
            if let Some(at) = free.iter().position(|p| p.same_sides(block)) {
                block.id = free.remove(at).id;
            } else {
                block.id = next_id;
                next_id += 1;
            }
        }
        if !blocks.is_empty() {
            map.insert(key, blocks.clone());
        }
        blocks
    }

    /// Forget a file (deleted or no longer conflicted).
    pub(crate) fn forget_file(&self, path: &Path) {
        self.map().remove(&Self::key_of(path));
    }

    /// Every registered file, ordered by path for stable output.
    fn snapshot(&self) -> Vec<(PathBuf, Vec<Block>)> {
        let mut entries: Vec<(PathBuf, Vec<Block>)> = self
            .map()
            .iter()
            .map(|(path, blocks)| (path.clone(), blocks.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }
}

/// `path` relative to `cwd`, which arrives canonicalized here (registry keys) while `cwd` is
/// the session path: canonicalize the base too or Windows shows a `\\?\C:\…` path.
pub(crate) fn rel(cwd: &Path, path: &Path) -> String {
    let base = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    display_rel(&base, path)
}

pub(crate) struct Address {
    pub(crate) id: Option<usize>,
    pub(crate) side: Option<String>,
}

pub(crate) fn parse_address(rest: &str) -> Result<Address> {
    if rest.is_empty() {
        return Err(HrnError::Tool(
            "conflict:// needs a hunk id (`conflict://1`), a side (`conflict://1/ours`) or `*`"
                .into(),
        ));
    }
    let (id, side) = match rest.split_once('/') {
        Some((id, side)) => (id, Some(side.to_ascii_lowercase())),
        None => (rest, None),
    };
    if let Some(side) = &side
        && !["ours", "theirs", "base"].contains(&side.as_str())
    {
        return Err(HrnError::Tool(format!(
            "`conflict://{rest}` takes one of ours, theirs, base"
        )));
    }
    if id == "*" {
        return Ok(Address { id: None, side });
    }
    match id.parse::<usize>() {
        Ok(n) if n > 0 => Ok(Address { id: Some(n), side }),
        _ => Err(HrnError::Tool(format!(
            "`conflict://{rest}` needs a positive hunk id or `*`"
        ))),
    }
}

pub(crate) fn shown_list(cwd: &Path, entries: &[(PathBuf, Vec<Block>)]) -> String {
    if entries.is_empty() {
        return "none registered".to_owned();
    }
    entries
        .iter()
        .map(|(p, _)| rel(cwd, p))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn stale(id: usize, files: &str) -> HrnError {
    HrnError::Tool(format!(
        "conflict {id} was never read or no longer exists (files with registered conflicts: \
         {files}); re-read `path:conflicts` for the current ids"
    ))
}

/// `lines` sliced by `sel` and numbered from `first` (the file's own coordinates).
fn numbered(lines: &[String], first: usize, sel: &Selector) -> String {
    let total = lines.len();
    let windows = sel.windows(total, fs_read::DEFAULT_VIEW_LIMIT);
    if windows.is_empty() {
        return format!("(the hunk has {total} line(s); the requested range is past the end)");
    }

    let mut rows: Vec<String> = Vec::new();
    for (a, b) in windows {
        if a > 1 && rows.last().is_some_and(|last| !last.ends_with("...")) {
            rows.push("...".to_owned());
        }
        for n in a..=b {
            if let Some(line) = lines.get(n - 1) {
                rows.push(format!(
                    "{}:{}",
                    first + n - 1,
                    crate::fs_read::clip_line(line)
                ));
            }
        }
    }
    rows.join("\n")
}

/// `read conflict://…` where `rest` is everything after the scheme, selectors included.
///
/// # Errors
/// An unknown or stale hunk id, a bad side name, or a file that cannot be re-read.
pub fn read(tracker: &FileReadTracker, cwd: &Path, rest: &str) -> Result<ToolOutput> {
    let (address, sel) = selector::split(rest)?;
    view(tracker, cwd, address, &sel)
}

/// A hunk or one of its sides, numbered in file coordinates.
pub(crate) fn view(
    tracker: &FileReadTracker,
    cwd: &Path,
    rest: &str,
    sel: &Selector,
) -> Result<ToolOutput> {
    let addr = parse_address(rest)?;
    let wanted = addr.id.ok_or_else(|| {
        HrnError::Tool("`conflict://*` is write-only; read `path:conflicts` for the index".into())
    })?;
    let entries = tracker.conflicts().snapshot();
    let found: Vec<(&PathBuf, &Block)> = entries
        .iter()
        .filter_map(|(path, blocks)| blocks.iter().find(|b| b.id == wanted).map(|b| (path, b)))
        .collect();
    if found.is_empty() {
        return Err(stale(wanted, &shown_list(cwd, &entries)));
    }
    let mut out = String::new();
    for (path, block) in &found {
        let shown = rel(cwd, path);
        let (lines, first) = match addr.side.as_deref() {
            Some(side) => block.side(side)?,
            None => (block_lines(&shown, path, block)?, block.start),
        };
        let _ = writeln!(
            out,
            "conflict {} of `{shown}` at file lines {}-{}",
            block.id, block.start, block.end
        );
        out.push_str(&numbered(&lines, first, sel));
    }
    Ok(ToolOutput::ok(out.trim_end().to_owned()))
}

/// The hunk verbatim, markers included, re-read so what is shown is what is on disk.
fn block_lines(shown: &str, path: &Path, block: &Block) -> Result<Vec<String>> {
    let text = fs::read_to_string(path).map_err(|e| {
        HrnError::Tool(format!(
            "cannot read `{shown}` for conflict {}: {e}",
            block.id
        ))
    })?;
    let current = parse(&text)
        .into_iter()
        .find(|b| b.same_sides(block))
        .ok_or_else(|| stale(block.id, &format!("`{shown}` no longer contains that hunk")))?;
    let lines: Vec<String> = text
        .lines()
        .skip(current.start - 1)
        .take(current.end - current.start + 1)
        .map(str::to_owned)
        .collect();
    Ok(lines)
}

/// `read path:conflicts`: register the file's hunks and print their index.
pub(crate) fn index_of(tracker: &FileReadTracker, path: &Path, shown: &str) -> Result<ToolOutput> {
    let bytes =
        fs::read(path).map_err(|e| HrnError::Tool(format!("cannot read `{shown}`: {e}")))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        HrnError::Tool(format!(
            "`{shown}` is not UTF-8 text; conflicts in it cannot be indexed"
        ))
    })?;
    if let Ok(mtime) = fs::metadata(path).and_then(|m| m.modified()) {
        tracker.record(path, mtime);
    }
    let blocks = tracker.conflicts().register(path, &text);
    if blocks.is_empty() {
        return Ok(ToolOutput::ok(format!(
            "`{shown}` has no merge conflict markers (`{OURS}` / `{SEP}` / `{THEIRS}`)."
        )));
    }
    Ok(ToolOutput::ok(index(shown, &blocks)))
}

/// The `path:conflicts` index of `blocks`.
pub(crate) fn index(shown: &str, blocks: &[Block]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "`{shown}` has {} merge conflict(s):", blocks.len());
    for block in blocks {
        let _ = writeln!(out, "{}", block.summary());
    }
    let _ = write!(
        out,
        "\nIds stay put until a hunk's content changes.\n\
         Read one: `conflict://N`, or `conflict://N/ours`, `/theirs`, `/base` for a single side.\n\
         Resolve one: write `conflict://N` with the text, or with exactly one token\n\
         `@ours`, `@theirs`, `@base` (diff3 merges only), `@both`.\n\
         Resolve all: write `conflict://*` with one text, or one `N: @token` line per hunk."
    );
    out
}

/// Note appended to a bare read that walked into conflict markers.
pub(crate) fn note(shown: &str, blocks: &[Block]) -> String {
    let ids = blocks
        .iter()
        .map(|b| b.id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "\n\n[This file has {} merge conflict(s) with ids {ids}; read `{shown}:conflicts` for \
         the index and resolve by writing `conflict://N`.]",
        blocks.len()
    )
}

mod resolve;
#[cfg(test)]
mod tests;

pub use resolve::write;
