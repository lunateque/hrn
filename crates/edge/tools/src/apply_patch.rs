//! `apply_patch`: multi-file Codex-format patches (`*** Begin Patch` ... `*** End Patch`).
//!
//! The whole patch is planned in memory first (hunks go through the same match ladder as
//! `edit_file`, restricted to whole lines) and only written once every hunk applied, so a bad
//! hunk never leaves a half-applied patch behind.

use crate::edit_ladder::{self, Opts};
use crate::fs_edit::{EditOpts, check_generated_guard, unified_diff};
use crate::fs_read::FileReadTracker;
use crate::paths::{display_rel, resolve};
use crate::seen_lines::SeenRanges;
use crate::text_codec::{TextFile, normalize_eol, write_atomic};
use hrn_kernel::{HrnError, Result, ToolOutput};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const END_OF_FILE: &str = "*** End of File";

#[derive(Debug)]
struct Hunk {
    old: Vec<String>,
    new: Vec<String>,
}

#[derive(Debug)]
enum Op {
    Add {
        path: PathBuf,
        lines: Vec<String>,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_to: Option<PathBuf>,
        hunks: Vec<Hunk>,
    },
}

fn is_directive(line: &str) -> bool {
    line.starts_with("*** ") && line != END_OF_FILE
}

fn parse_hunks(lines: &[&str]) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut cur = Hunk {
        old: Vec::new(),
        new: Vec::new(),
    };
    let mut flush = |cur: &mut Hunk| {
        if !cur.old.is_empty() || !cur.new.is_empty() {
            hunks.push(std::mem::replace(
                cur,
                Hunk {
                    old: Vec::new(),
                    new: Vec::new(),
                },
            ));
        }
    };
    for &line in lines {
        if line.starts_with("@@") {
            flush(&mut cur);
        } else if line == END_OF_FILE || line.starts_with("\\ ") {
            // Markers only.
        } else if let Some(rest) = line.strip_prefix('-') {
            cur.old.push(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix('+') {
            cur.new.push(rest.to_owned());
        } else {
            // Context line; models routinely drop the leading space (and blank lines' space).
            let rest = line.strip_prefix(' ').unwrap_or(line);
            cur.old.push(rest.to_owned());
            cur.new.push(rest.to_owned());
        }
    }
    flush(&mut cur);
    hunks
}

fn parse(cwd: &Path, patch: &str) -> Result<Vec<Op>> {
    let normalized = normalize_eol(patch);
    let lines: Vec<&str> = normalized.lines().collect();
    let mut ops = Vec::new();
    let mut idx = 0usize;
    while let Some(&line) = lines.get(idx) {
        idx += 1;
        let section_end = |from: usize| {
            lines
                .iter()
                .enumerate()
                .skip(from)
                .find(|(_, l)| is_directive(l))
                .map_or(lines.len(), |(i, _)| i)
        };
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let end = section_end(idx);
            let body = lines.get(idx..end).unwrap_or_default();
            ops.push(Op::Add {
                path: resolve(cwd, path.trim()),
                lines: body
                    .iter()
                    .map(|l| l.strip_prefix('+').unwrap_or(l).to_owned())
                    .collect(),
            });
            idx = end;
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(Op::Delete {
                path: resolve(cwd, path.trim()),
            });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            let move_to = lines
                .get(idx)
                .and_then(|l| l.strip_prefix("*** Move to: "))
                .map(|p| resolve(cwd, p.trim()));
            if move_to.is_some() {
                idx += 1;
            }
            let end = section_end(idx);
            let hunks = parse_hunks(lines.get(idx..end).unwrap_or_default());
            idx = end;
            ops.push(Op::Update {
                path: resolve(cwd, path.trim()),
                move_to,
                hunks,
            });
        }
    }
    if ops.is_empty() {
        return Err(HrnError::Tool(
            "apply_patch: no `*** Add File:` / `*** Update File:` / `*** Delete File:` directives found"
                .into(),
        ));
    }
    Ok(ops)
}

/// How seen-line tracking should be updated for a written file once the patch commits.
#[derive(Clone, Debug)]
enum SeenPlan {
    All,
    Snapshot(SeenRanges),
}

/// In-memory view of the files a patch touches (`None` = deleted by an earlier operation).
struct Plan<'a> {
    tracker: &'a FileReadTracker,
    cwd: &'a Path,
    opts: &'a EditOpts,
    files: BTreeMap<PathBuf, Option<(TextFile, SeenPlan)>>,
    report: Vec<String>,
    diffs: Vec<String>,
}

impl Plan<'_> {
    fn shown(&self, path: &Path) -> String {
        display_rel(self.cwd, path)
    }

    fn exists(&self, path: &Path) -> bool {
        match self.files.get(path) {
            Some(slot) => slot.is_some(),
            None => path.exists(),
        }
    }

    fn load(&self, path: &Path) -> Result<(TextFile, SeenRanges)> {
        let shown = self.shown(path);
        match self.files.get(path) {
            Some(Some((file, seen))) => {
                let ranges = match seen {
                    SeenPlan::All => {
                        let mut s = SeenRanges::default();
                        let count = file.text.lines().count();
                        if count == 0 {
                            s.mark_observed();
                        } else {
                            s.add(1, count);
                        }
                        s
                    }
                    SeenPlan::Snapshot(s) => s.clone(),
                };
                return Ok((file.clone(), ranges));
            }
            Some(None) => {
                return Err(HrnError::Tool(format!(
                    "apply_patch: `{shown}` was deleted earlier in this patch"
                )));
            }
            None => {}
        }
        self.tracker.verify_fresh(path)?;
        let bytes = fs::read(path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => {
                HrnError::Tool(format!("apply_patch: `{shown}` does not exist"))
            }
            _ => HrnError::Tool(format!("apply_patch: cannot read `{shown}`: {e}")),
        })?;
        let file = TextFile::decode(bytes)
            .map_err(|e| HrnError::Tool(format!("apply_patch: `{shown}`: {e}")))?;
        Ok((file, self.tracker.seen_snapshot(path)))
    }

    fn apply(&mut self, op: Op) -> Result {
        match op {
            Op::Add { path, lines } => {
                let shown = self.shown(&path);
                check_generated_guard(&path, self.cwd, &shown, &self.opts.blocked_globs)?;
                if self.exists(&path) {
                    return Err(HrnError::Tool(format!(
                        "apply_patch: cannot add `{shown}`: it already exists"
                    )));
                }
                let text = if lines.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", lines.join("\n"))
                };
                self.diffs.push(unified_diff(&shown, "", &text));
                self.report.push(format!("A {shown}"));
                self.files.insert(
                    path,
                    Some((
                        TextFile {
                            bom: false,
                            crlf: false,
                            mixed_eol: false,
                            text,
                        },
                        SeenPlan::All,
                    )),
                );
            }
            Op::Delete { path } => {
                let shown = self.shown(&path);
                check_generated_guard(&path, self.cwd, &shown, &self.opts.blocked_globs)?;
                let (_, seen) = self.load(&path)?;
                if self.opts.enforce_seen_lines && !seen.is_observed() {
                    return Err(HrnError::Tool(format!(
                        "unseen lines in `{shown}`: file has not been read yet (seen: none); read it before editing"
                    )));
                }
                self.report.push(format!("D {shown}"));
                self.files.insert(path, None);
            }
            Op::Update {
                path,
                move_to,
                hunks,
            } => self.apply_update(path, move_to, &hunks)?,
        }
        Ok(())
    }

    fn apply_update(&mut self, path: PathBuf, move_to: Option<PathBuf>, hunks: &[Hunk]) -> Result {
        let shown = self.shown(&path);
        check_generated_guard(&path, self.cwd, &shown, &self.opts.blocked_globs)?;
        if let Some(dest) = &move_to {
            let dest_shown = self.shown(dest);
            check_generated_guard(dest, self.cwd, &dest_shown, &self.opts.blocked_globs)?;
        }
        let (mut file, mut sim_seen) = self.load(&path)?;
        let before = file.text.clone();
        for (n, hunk) in hunks.iter().enumerate() {
            let prev_lines = file.text.lines().count();
            let (next_text, ranges) = apply_hunk(&file.text, hunk, self.opts).map_err(|e| {
                HrnError::Tool(format!("apply_patch: hunk {} of `{shown}`: {e}", n + 1))
            })?;
            if self.opts.enforce_seen_lines {
                verify_ranges_in_snapshot(&sim_seen, &shown, &ranges)?;
            }
            let deltas: Vec<(usize, usize, usize)> = if ranges.is_empty() {
                vec![(prev_lines, prev_lines, hunk.new.len())]
            } else {
                ranges
                    .iter()
                    .map(|&(start, end)| (start.saturating_sub(1), end, hunk.new.len()))
                    .collect()
            };
            sim_seen.adjust_after_edits(&deltas);
            file.text = next_text;
        }
        if self.opts.enforce_seen_lines && hunks.is_empty() && !sim_seen.is_observed() {
            return Err(HrnError::Tool(format!(
                "unseen lines in `{shown}`: file has not been read yet (seen: none); read it before editing"
            )));
        }
        self.diffs.push(unified_diff(&shown, &before, &file.text));
        if let Some(dest) = move_to {
            if self.exists(&dest) {
                return Err(HrnError::Tool(format!(
                    "apply_patch: cannot move `{shown}` to `{}`: destination exists",
                    self.shown(&dest)
                )));
            }
            self.report
                .push(format!("R {shown} -> {}", self.shown(&dest)));
            self.files.insert(path, None);
            self.files
                .insert(dest, Some((file, SeenPlan::Snapshot(sim_seen))));
        } else {
            self.report.push(format!("M {shown}"));
            self.files
                .insert(path, Some((file, SeenPlan::Snapshot(sim_seen))));
        }
        Ok(())
    }
}

fn verify_ranges_in_snapshot(seen: &SeenRanges, shown: &str, ranges: &[(usize, usize)]) -> Result {
    if ranges.is_empty() {
        if seen.is_observed() {
            return Ok(());
        }
        return Err(HrnError::Tool(format!(
            "unseen lines in `{shown}`: file has not been read yet (seen: none); read it before editing"
        )));
    }
    for &(start, end) in ranges {
        if !seen.covers(start, end) {
            return Err(HrnError::Tool(format!(
                "unseen lines in `{shown}`: edit targets lines {start}-{end}, but only seen {}; \
                 read those lines before editing",
                seen.describe()
            )));
        }
    }
    Ok(())
}

fn apply_hunk(
    text: &str,
    hunk: &Hunk,
    opts: &EditOpts,
) -> std::result::Result<(String, Vec<(usize, usize)>), String> {
    let terminated = |lines: &[String]| {
        if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        }
    };
    if hunk.old.is_empty() {
        // Pure insertion without context appends.
        let mut out = text.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&terminated(&hunk.new));
        return Ok((out, Vec::new()));
    }
    edit_ladder::apply(
        text,
        &terminated(&hunk.old),
        &terminated(&hunk.new),
        Opts {
            replace_all: false,
            whole_lines: true,
            fuzzy_match: opts.fuzzy_match,
            fuzzy_threshold: opts.fuzzy_threshold,
        },
    )
    .map(|applied| (applied.text, applied.matched_ranges))
    .map_err(|e| e.to_string())
}

/// Apply a Codex-format multi-file patch under `cwd`, all or nothing, with default options.
pub fn apply_patch_sync(tracker: &FileReadTracker, cwd: &Path, patch: &str) -> Result<ToolOutput> {
    apply_patch_sync_with_opts(tracker, cwd, patch, &EditOpts::default())
}

/// Apply a Codex-format multi-file patch under `cwd`, enforcing `opts`.
pub fn apply_patch_sync_with_opts(
    tracker: &FileReadTracker,
    cwd: &Path,
    patch: &str,
    opts: &EditOpts,
) -> Result<ToolOutput> {
    let actions = parse(cwd, patch)?;
    let mut plan = Plan {
        tracker,
        cwd,
        opts,
        files: BTreeMap::new(),
        report: Vec::new(),
        diffs: Vec::new(),
    };
    for op in actions {
        plan.apply(op)?;
    }

    for (written, (path, slot)) in plan.files.iter().enumerate() {
        let shown = display_rel(cwd, path);
        let result = match slot {
            Some((file, seen_plan)) => write_atomic(path, &file.encode()).map(|()| {
                tracker.record_current(path);
                match seen_plan {
                    SeenPlan::All => tracker.record_seen_all(path, file.text.lines().count()),
                    SeenPlan::Snapshot(ranges) => tracker.set_seen_snapshot(path, ranges.clone()),
                }
            }),
            None => fs::remove_file(path).map(|()| tracker.forget(path)),
        };
        result.map_err(|e| {
            HrnError::Tool(format!(
                "apply_patch: failed writing `{shown}` after {written} of {} file(s): {e}",
                plan.files.len()
            ))
        })?;
    }

    let mut out = ToolOutput::ok(format!("Patch applied:\n{}", plan.report.join("\n")));
    out.diff = Some(plan.diffs.concat());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn failing_hunk_writes_nothing() {
        let dir = TempDir::new("patch-atomic");
        let a = dir.write("a.txt", "one\ntwo\n");
        let b = dir.write("b.txt", "keep\n");
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-two\n+TWO\n\
                     *** Update File: b.txt\n@@\n-missing\n+x\n*** End Patch";
        let tracker = FileReadTracker::new();
        let err = apply_patch_sync(&tracker, dir.path(), patch).unwrap_err();
        assert!(err.to_string().contains("b.txt"), "{err}");
        assert_eq!(fs::read_to_string(&a).unwrap(), "one\ntwo\n");
        assert_eq!(fs::read_to_string(&b).unwrap(), "keep\n");
    }

    #[test]
    fn add_update_delete_and_move() {
        let dir = TempDir::new("patch-ops");
        let up = dir.write("up.txt", "a\r\nb\r\nc\r\n");
        let del = dir.write("del.txt", "x\n");
        let mv = dir.write("old.txt", "m\n");
        let patch = "*** Begin Patch\n*** Add File: new/n.txt\n+hello\n+world\n\
                     *** Update File: up.txt\n@@\n a\n-b\n+B\n c\n\
                     *** Delete File: del.txt\n\
                     *** Update File: old.txt\n*** Move to: moved.txt\n@@\n-m\n+M\n*** End Patch";
        let tracker = FileReadTracker::new();
        apply_patch_sync(&tracker, dir.path(), patch).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("new/n.txt")).unwrap(),
            "hello\nworld\n"
        );
        assert_eq!(fs::read(&up).unwrap(), b"a\r\nB\r\nc\r\n");
        assert!(!del.exists() && !mv.exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("moved.txt")).unwrap(),
            "M\n"
        );
    }

    #[test]
    fn partial_update_does_not_mark_unread_lines_as_seen() {
        use std::fmt::Write as _;
        let dir = TempDir::new("patch-seen");
        let mut body = String::new();
        for i in 1..=30 {
            let _ = writeln!(body, "line {i}");
        }
        let path = dir.write("f.txt", &body);
        let tracker = FileReadTracker::new();
        let ctx = crate::testutil::test_ctx(dir.path().to_path_buf());
        // Read only lines 1-3 (with context -> 1-6).
        crate::fs_read::read_sync(&tracker, &ctx, "f.txt:1-3").unwrap();
        let opts = EditOpts {
            enforce_seen_lines: true,
            ..EditOpts::default()
        };
        let patch1 = "*** Begin Patch\n*** Update File: f.txt\n@@\n-line 2\n+LINE 2\n*** End Patch";
        apply_patch_sync_with_opts(&tracker, dir.path(), patch1, &opts).unwrap();

        // Line 25 was NEVER read — a second patch targeting line 25 MUST still be refused.
        let patch2 =
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-line 25\n+LINE 25\n*** End Patch";
        let err = apply_patch_sync_with_opts(&tracker, dir.path(), patch2, &opts)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unseen") && err.contains("25-25"), "{err}");
        assert!(fs::read_to_string(&path).unwrap().contains("line 25\n"));
    }
}
