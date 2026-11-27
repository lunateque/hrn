//! `edit_file` (search-replace through the match ladder) and `write_file`.
//!
//! Both refuse to overwrite a file that changed since the model last read it, write atomically,
//! and keep the file's BOM and line-ending style.

use crate::edit_flags::match_generated_glob_canonical;
use crate::edit_ladder::{self, DEFAULT_FUZZY_THRESHOLD, Opts};
use crate::fs_read::FileReadTracker;
use crate::paths::display_rel;
use crate::text_codec::{TextFile, normalize_eol, write_atomic};
use hrn_kernel::{HrnError, Result, ToolOutput};
use similar::{DiffOp, TextDiff};
use std::fs;
use std::io;
use std::path::Path;

/// Policy knobs shared by `replace` and `apply_patch` editors.
#[derive(Clone, Debug)]
pub struct EditOpts {
    /// Enable trailing-whitespace, indentation, and similarity fallback matching.
    pub fuzzy_match: bool,
    /// Minimum similarity ratio (`0.0..=1.0`) for fuzzy matching.
    pub fuzzy_threshold: f32,
    /// Refuse edits touching lines not yet displayed by `read`.
    pub enforce_seen_lines: bool,
    /// Auto-generated / lockfile globs refused when non-empty.
    pub blocked_globs: Vec<String>,
}

impl Default for EditOpts {
    fn default() -> Self {
        Self {
            fuzzy_match: true,
            fuzzy_threshold: DEFAULT_FUZZY_THRESHOLD,
            enforce_seen_lines: false,
            blocked_globs: Vec::new(),
        }
    }
}

/// Unified diff (3 lines of context) between two LF-normalized texts.
pub fn unified_diff(label: &str, before: &str, after: &str) -> String {
    TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{label}"), &format!("b/{label}"))
        .to_string()
}

fn io_err(action: &str, shown: &str, err: &io::Error) -> HrnError {
    HrnError::Tool(format!("cannot {action} `{shown}`: {err}"))
}

/// Refuse edits targeting an auto-generated or lockfile path when `blocked_globs` matches
/// either the lexical path or its canonicalized target (defeating symlink bypasses).
///
/// # Errors
/// Returns `HrnError::Tool` citing the matched glob when blocked.
pub fn check_generated_guard(
    path: &Path,
    cwd: &Path,
    shown: &str,
    blocked_globs: &[String],
) -> Result {
    if let Some(glob) = match_generated_glob_canonical(path, cwd, blocked_globs) {
        return Err(HrnError::Tool(format!(
            "refused edit to auto-generated file `{shown}` (matched glob `{glob}`); \
             set `[edit] block_auto_generated = false` to allow"
        )));
    }
    Ok(())
}

/// Verify that every line modified, deleted, or inserted between LF-normalized `before` and
/// `after` falls inside `tracker`'s recorded seen ranges for `path`, returning ascending edit
/// deltas `(lo, hi, new_len)` suitable for [`FileReadTracker::adjust_seen_after_edits`].
///
/// # Errors
/// Returns `HrnError::Tool` if any changed or inserted region touches unseen lines.
pub fn verify_diff_lines_seen(
    tracker: &FileReadTracker,
    path: &Path,
    shown: &str,
    before: &str,
    after: &str,
) -> Result<Vec<(usize, usize, usize)>> {
    let old_line_count = if before.is_empty() {
        0
    } else {
        before
            .strip_suffix('\n')
            .unwrap_or(before)
            .split('\n')
            .count()
    };
    let diff = TextDiff::from_lines(before, after);
    let mut deltas = Vec::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Replace {
                old_index,
                old_len,
                new_len,
                ..
            } => {
                if old_len > 0 {
                    tracker.verify_lines_seen(path, shown, old_index + 1, old_index + old_len)?;
                }
                deltas.push((old_index, old_index + old_len, new_len));
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                if old_len > 0 {
                    tracker.verify_lines_seen(path, shown, old_index + 1, old_index + old_len)?;
                }
                deltas.push((old_index, old_index + old_len, 0));
            }
            DiffOp::Insert {
                old_index, new_len, ..
            } => {
                if old_line_count == 0 {
                    tracker.verify_file_seen(path, shown)?;
                } else if old_index == 0 {
                    tracker.verify_lines_seen(path, shown, 1, 1)?;
                } else if old_index >= old_line_count {
                    tracker.verify_lines_seen(path, shown, old_line_count, old_line_count)?;
                } else {
                    tracker.verify_lines_seen(path, shown, old_index, old_index + 1)?;
                }
                deltas.push((old_index, old_index, new_len));
            }
        }
    }
    Ok(deltas)
}

/// Replace `old` with `new` in `path` (the search-replace editor) using default options.
pub fn edit_file_sync(
    tracker: &FileReadTracker,
    cwd: &Path,
    path: &Path,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<ToolOutput> {
    edit_file_sync_with_opts(
        tracker,
        cwd,
        path,
        old,
        new,
        replace_all,
        &EditOpts::default(),
    )
}

/// Replace `old` with `new` in `path` enforcing `opts` (fuzzy threshold, generated-file guard,
/// and seen-line check).
pub fn edit_file_sync_with_opts(
    tracker: &FileReadTracker,
    cwd: &Path,
    path: &Path,
    old: &str,
    new: &str,
    replace_all: bool,
    opts: &EditOpts,
) -> Result<ToolOutput> {
    let shown = display_rel(cwd, path);
    check_generated_guard(path, cwd, &shown, &opts.blocked_globs)?;
    if old.is_empty() {
        return Err(HrnError::Tool("`old` must not be empty".into()));
    }
    let (old, new) = (normalize_eol(old), normalize_eol(new));
    if old == new {
        return Err(HrnError::Tool("`old` and `new` are identical".into()));
    }
    tracker.verify_fresh(path)?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(HrnError::Tool(format!(
                "`{shown}` does not exist; use `write_file` to create it"
            )));
        }
        Err(e) => return Err(io_err("read", &shown, &e)),
    };
    let mut file =
        TextFile::decode(bytes).map_err(|e| HrnError::Tool(format!("`{shown}`: {e}")))?;

    let applied = edit_ladder::apply(
        &file.text,
        &old,
        &new,
        Opts {
            replace_all,
            whole_lines: false,
            fuzzy_match: opts.fuzzy_match,
            fuzzy_threshold: opts.fuzzy_threshold,
        },
    )
    .map_err(|e| HrnError::Tool(format!("edit_file `{shown}`: {e}")))?;

    if opts.enforce_seen_lines {
        for &(start, end) in &applied.matched_ranges {
            tracker.verify_lines_seen(path, &shown, start, end)?;
        }
    }

    let diff = unified_diff(&shown, &file.text, &applied.text);
    let new_line_count = if new.is_empty() {
        0
    } else {
        new.strip_suffix('\n').unwrap_or(&new).split('\n').count()
    };
    let deltas: Vec<(usize, usize, usize)> = applied
        .matched_ranges
        .iter()
        .map(|&(start, end)| (start.saturating_sub(1), end, new_line_count))
        .collect();
    file.text = applied.text;
    write_atomic(path, &file.encode()).map_err(|e| io_err("write", &shown, &e))?;
    tracker.record_current(path);
    tracker.adjust_seen_after_edits(path, &deltas);

    let mut message = format!(
        "Edited `{shown}`: {} replacement(s) via {}.",
        applied.count,
        applied.step.label()
    );
    if file.mixed_eol {
        message.push_str(" The file mixed CRLF and LF; line endings were normalized.");
    }
    let mut out = ToolOutput::ok(message);
    out.diff = Some(diff);
    Ok(out)
}

/// Create or overwrite the target named by `raw`, which may address one archive member
/// (`a.zip:inner/file`) or one `SQLite` row (`db.sqlite:table[:key]`) instead of a whole file.
pub fn write_target_sync(
    tracker: &FileReadTracker,
    cwd: &Path,
    raw: &str,
    content: &str,
) -> Result<ToolOutput> {
    write_target_sync_with_opts(tracker, cwd, raw, content, &[])
}

/// Like [`write_target_sync`], refusing files matching `blocked_globs`.
pub fn write_target_sync_with_opts(
    tracker: &FileReadTracker,
    cwd: &Path,
    raw: &str,
    content: &str,
    blocked_globs: &[String],
) -> Result<ToolOutput> {
    if let Some(out) = crate::read_fmt::write_target(cwd, raw, content)? {
        return Ok(out);
    }
    let path = crate::paths::resolve(cwd, raw);
    write_file_sync_with_opts(tracker, cwd, &path, content, blocked_globs)
}

/// Create or overwrite `path` with `content`.
pub fn write_file_sync(
    tracker: &FileReadTracker,
    cwd: &Path,
    path: &Path,
    content: &str,
) -> Result<ToolOutput> {
    write_file_sync_with_opts(tracker, cwd, path, content, &[])
}

/// Create or overwrite `path` with `content`, refusing targets matching `blocked_globs`.
pub fn write_file_sync_with_opts(
    tracker: &FileReadTracker,
    cwd: &Path,
    path: &Path,
    content: &str,
    blocked_globs: &[String],
) -> Result<ToolOutput> {
    let shown = display_rel(cwd, path);
    check_generated_guard(path, cwd, &shown, blocked_globs)?;
    let existing = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_err("read", &shown, &e)),
    };
    if existing.is_some() {
        tracker.verify_fresh(path)?;
    }
    // Overwriting a text file keeps its BOM / line-ending style; anything else is written as is.
    let previous = existing.and_then(|bytes| TextFile::decode(bytes).ok());
    let new_text = normalize_eol(content);
    let (bytes, before) = match previous {
        Some(prev) => {
            let next = TextFile {
                text: new_text.clone(),
                ..prev.clone()
            };
            (next.encode(), Some(prev.text))
        }
        None => (content.as_bytes().to_vec(), None),
    };
    write_atomic(path, &bytes).map_err(|e| io_err("write", &shown, &e))?;
    tracker.record_current(path);
    tracker.record_seen_all(path, new_text.lines().count());

    let verb = if before.is_some() {
        "Overwrote"
    } else {
        "Created"
    };
    let mut out = ToolOutput::ok(format!(
        "{verb} `{shown}` ({} lines, {} bytes).",
        content.lines().count(),
        bytes.len()
    ));
    out.diff = Some(unified_diff(
        &shown,
        before.as_deref().unwrap_or_default(),
        &new_text,
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::time::{Duration, SystemTime};

    fn edit(tracker: &FileReadTracker, dir: &TempDir, old: &str, new: &str) -> Result<ToolOutput> {
        edit_file_sync(
            tracker,
            dir.path(),
            &dir.path().join("f.txt"),
            old,
            new,
            false,
        )
    }

    #[test]
    fn edit_keeps_bom_and_crlf_and_leaves_no_temp_files() {
        let dir = TempDir::new("edit-codec");
        let path = dir.write("f.txt", "\u{feff}one\r\ntwo\r\nthree\r\n");
        let tracker = FileReadTracker::new();
        // The model sends LF; the file is CRLF.
        edit(&tracker, &dir, "two\nthree\n", "TWO\nTHREE\n").unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            "\u{feff}one\r\nTWO\r\nTHREE\r\n".as_bytes()
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn edit_refuses_when_mtime_changed_since_read() {
        let dir = TempDir::new("edit-stale");
        let path = dir.write("f.txt", "alpha\nbeta\n");
        let tracker = FileReadTracker::new();
        crate::fs_read::read_sync(
            &tracker,
            &crate::testutil::test_ctx(dir.path().to_path_buf()),
            path.to_str().unwrap(),
        )
        .unwrap();

        // Someone else touches the file after our read.
        fs::write(&path, "alpha\nbeta changed\n").unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_mins(1))
            .unwrap();
        drop(file);

        let err = edit(&tracker, &dir, "alpha", "ALPHA")
            .unwrap_err()
            .to_string();
        assert!(err.contains("stale edit refused"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "alpha\nbeta changed\n");

        // Re-reading clears the staleness, and our own write does not make us stale.
        crate::fs_read::read_sync(
            &tracker,
            &crate::testutil::test_ctx(dir.path().to_path_buf()),
            path.to_str().unwrap(),
        )
        .unwrap();
        edit(&tracker, &dir, "alpha", "ALPHA").unwrap();
        edit(&tracker, &dir, "beta changed", "beta").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "ALPHA\nbeta\n");
    }

    #[test]
    fn miss_and_ambiguity_leave_the_file_untouched() {
        let dir = TempDir::new("edit-miss");
        let path = dir.write("f.txt", "x = 1\nx = 1\n");
        let tracker = FileReadTracker::new();
        let err = edit(&tracker, &dir, "x = 1", "x = 2")
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 places"), "{err}");
        let err = edit(&tracker, &dir, "y = 1", "y = 2")
            .unwrap_err()
            .to_string();
        assert!(err.contains("1| x = 1"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "x = 1\nx = 1\n");
    }

    #[test]
    fn empty_replacement_records_zero_new_lines_and_write_blocks_generated() {
        use std::fmt::Write as _;
        let dir = TempDir::new("edit-del-gen");
        let mut body = String::new();
        for i in 1..=20 {
            let _ = writeln!(body, "line{i}");
        }
        let path = dir.write("f.txt", &body);
        let tracker = FileReadTracker::new();
        // Read lines 1-2 (with context -> 1-5).
        crate::fs_read::read_sync(
            &tracker,
            &crate::testutil::test_ctx(dir.path().to_path_buf()),
            "f.txt:1-2",
        )
        .unwrap();
        let opts = EditOpts {
            enforce_seen_lines: true,
            blocked_globs: vec!["Cargo.lock".to_owned()],
            ..EditOpts::default()
        };
        // Delete lines 1-2 completely (`new = ""`): seen range 1-5 must shrink to 1-3 (NOT 1-4!).
        edit_file_sync_with_opts(
            &tracker,
            dir.path(),
            &path,
            "line1\nline2\n",
            "",
            false,
            &opts,
        )
        .unwrap();
        assert!(tracker.verify_lines_seen(&path, "f.txt", 1, 3).is_ok());
        assert!(
            tracker.verify_lines_seen(&path, "f.txt", 4, 4).is_err(),
            "deleting lines must not spuriously mark previously unseen line 6 (now line 4) as seen"
        );

        // `write_file_sync_with_opts` refuses auto-generated targets.
        let lock = dir.write("Cargo.lock", "version = 3\n");
        let err = write_file_sync_with_opts(
            &tracker,
            dir.path(),
            &lock,
            "version = 4\n",
            &opts.blocked_globs,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Cargo.lock"), "{err}");

        // Canonical guard catches symlinked alias to `Cargo.lock` when OS permits symlink creation.
        let alias = dir.path().join("alias.txt");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&lock, &alias).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&lock, &alias).is_ok();
        if linked {
            let sym_err =
                check_generated_guard(&alias, dir.path(), "alias.txt", &opts.blocked_globs)
                    .unwrap_err()
                    .to_string();
            assert!(sym_err.contains("Cargo.lock"), "{sym_err}");
        }
    }
}
