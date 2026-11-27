//! Session-scoped file read tracker (`FileReadTracker`) recording file modification times,
//! seen 1-based line ranges, and indexed merge conflicts so subsequent edits refuse stale or
//! unseen modifications.

use crate::conflicts::ConflictRegistry;
use crate::seen_lines::SeenRanges;
use hrn_kernel::{HrnError, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Remembers the modification time and seen 1-based line ranges of every file at the moment
/// `read_file` returned it, so a later `edit_file` / `write_file` can refuse to overwrite unseen
/// or externally-changed lines, and carries merge conflicts (`path:conflicts`, `conflict://N`).
#[derive(Default)]
pub struct FileReadTracker {
    mtimes: Mutex<HashMap<PathBuf, SystemTime>>,
    seen: Mutex<HashMap<PathBuf, SeenRanges>>,
    conflicts: ConflictRegistry,
}

pub(crate) fn key_of(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl FileReadTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, SystemTime>> {
        self.mtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn seen_map(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, SeenRanges>> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn record(&self, path: &Path, mtime: SystemTime) {
        let key = key_of(path);
        let changed = self
            .map()
            .insert(key.clone(), mtime)
            .is_some_and(|prev| prev != mtime);
        if changed {
            self.seen_map().remove(&key);
        }
    }

    /// Record the file's current on-disk mtime (after this process wrote or formatted it).
    pub fn record_current(&self, path: &Path) {
        if let Ok(mtime) = fs::metadata(path).and_then(|m| m.modified()) {
            self.map().insert(key_of(path), mtime);
        }
    }

    /// Record 1-based inclusive line windows displayed to the model for `path`.
    pub(crate) fn record_seen(&self, path: &Path, windows: &[(usize, usize)], empty_file: bool) {
        let mut map = self.seen_map();
        let entry = map.entry(key_of(path)).or_default();
        if empty_file {
            entry.mark_observed();
        }
        for &(a, b) in windows {
            entry.add(a, b);
        }
    }

    /// Mark an entire written file (`1..=line_count`, or empty) as seen by the model.
    pub(crate) fn record_seen_all(&self, path: &Path, line_count: usize) {
        let mut map = self.seen_map();
        let entry = map.entry(key_of(path)).or_default();
        if line_count == 0 {
            entry.mark_observed();
        } else {
            entry.add(1, line_count);
        }
    }

    /// Adjust tracked seen line ranges after edits `(lo, hi, new_len)` succeeded on `path`.
    pub fn adjust_seen_after_edits(&self, path: &Path, edits: &[(usize, usize, usize)]) {
        if let Some(entry) = self.seen_map().get_mut(&key_of(path)) {
            entry.adjust_after_edits(edits);
        }
    }

    /// Snapshot tracked seen ranges for `path` (default unobserved if not yet read).
    pub(crate) fn seen_snapshot(&self, path: &Path) -> SeenRanges {
        self.seen_map()
            .get(&key_of(path))
            .cloned()
            .unwrap_or_default()
    }

    /// Overwrite tracked seen ranges for `path` (used after multi-hunk patch planning).
    pub(crate) fn set_seen_snapshot(&self, path: &Path, ranges: SeenRanges) {
        if ranges.is_observed() {
            self.seen_map().insert(key_of(path), ranges);
        }
    }

    /// Transfer seen ranges from `from` to `to`, adjusting for `edits` (used on `MV` renames).
    pub(crate) fn transfer_seen(&self, from: &Path, to: &Path, edits: &[(usize, usize, usize)]) {
        let mut ranges = self.seen_snapshot(from);
        if ranges.is_observed() {
            ranges.adjust_after_edits(edits);
            self.seen_map().insert(key_of(to), ranges);
        }
    }

    /// Verify that `path` has been observed at least once (`REM` / `MV` / empty-file insert).
    ///
    /// # Errors
    /// Returns `HrnError::Tool` when `path` has not been read yet.
    pub fn verify_file_seen(&self, path: &Path, shown: &str) -> Result {
        if self
            .seen_map()
            .get(&key_of(path))
            .is_some_and(SeenRanges::is_observed)
        {
            return Ok(());
        }
        Err(HrnError::Tool(format!(
            "unseen lines in `{shown}`: file has not been read yet (seen: none); read it before editing"
        )))
    }

    /// Verify that every 1-based line in `start..=end` was previously displayed to the model.
    ///
    /// # Errors
    /// Returns `HrnError::Tool` when any line in `start..=end` was not previously read.
    pub fn verify_lines_seen(&self, path: &Path, shown: &str, start: usize, end: usize) -> Result {
        let seen = self.seen_map();
        let state = seen.get(&key_of(path));
        if state.is_some_and(|s| s.covers(start, end)) {
            return Ok(());
        }
        let seen_desc = state.map_or_else(|| "none".to_owned(), SeenRanges::describe);
        Err(HrnError::Tool(format!(
            "unseen lines in `{shown}`: edit targets lines {start}-{end}, but only seen {seen_desc}; \
             read those lines before editing"
        )))
    }

    pub(crate) fn forget(&self, path: &Path) {
        let key = key_of(path);
        self.map().remove(&key);
        self.seen_map().remove(&key);
        self.conflicts.forget_file(path);
    }

    /// The merge conflicts seen in files read through this tracker (`path:conflicts`,
    /// `conflict://N`).
    pub(crate) fn conflicts(&self) -> &ConflictRegistry {
        &self.conflicts
    }

    /// Fail when `path` was read before and its mtime has changed since. A file that was never
    /// read has nothing to be stale against and passes.
    pub(crate) fn verify_fresh(&self, path: &Path) -> Result {
        let recorded = self.map().get(&key_of(path)).copied();
        let current = fs::metadata(path).and_then(|m| m.modified()).ok();
        match (recorded, current) {
            (Some(seen), Some(now)) if seen != now => Err(HrnError::Tool(format!(
                "stale edit refused: `{}` changed on disk since you last read it; \
                 read it again before modifying it",
                path.display()
            ))),
            _ => Ok(()),
        }
    }
}
