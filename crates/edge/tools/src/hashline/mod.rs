//! `edit` (hashline mode): line-anchored patches against content-tagged file snapshots.
//!
//! `read_file` stamps its output with `[path#TAG]`, a 4-hex tag of the file's content. An edit
//! names the tag it was written against and addresses lines by their numbers in that snapshot;
//! when the file changed since, the tag no longer matches and the edit is refused with the
//! current tag and rows instead of landing on shifted lines. The grammar follows omp's:
//! `PUT N.=M:` / `PUT N*:` / `PUT <N:` / `PUT >N:` / `PUT >N*:` / `PUT >$:` with `+TEXT` rows,
//! `CUT`, register pastes, `REM` and `MV`.

mod apply;
mod block;
mod parse;

use crate::edit_ladder::{DEFAULT_FUZZY_THRESHOLD, nearest_above_threshold};
use crate::fs_edit::{check_generated_guard, unified_diff};
use crate::fs_read::FileReadTracker;
use crate::paths::{display_rel, resolve};
use crate::text_codec::{TextFile, write_atomic};
use apply::{Plan, Registers, rows_around};
use hrn_kernel::{HrnError, Result, ToolContext, ToolOutput, UrlBody, UrlRouter};
use parse::{Op, Section, Source};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The 4-uppercase-hex tag of a file snapshot: FNV-1a over its content with CRLF folded to LF
/// (and the BOM already removed by the caller), folded to 16 bits.
pub fn tag_of(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut hash: u32 = 0x811c_9dc5;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            continue;
        }
        hash = (hash ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    format!("{:04X}", (hash ^ (hash >> 16)) & 0xFFFF)
}

fn fail(msg: impl Into<String>) -> HrnError {
    HrnError::Tool(msg.into())
}

/// Runtime policy knobs for [`HashlineEditor`].
#[derive(Clone, Debug)]
pub struct HashlineOpts {
    /// Refuse edits touching lines that were not displayed by a prior `read`.
    pub enforce_seen_lines: bool,
    /// Include fuzzy near-miss hints on stale-tag refusals.
    pub fuzzy_match: bool,
    /// Minimum similarity threshold (`0.0..=1.0`) for hashline near-miss hints.
    pub fuzzy_threshold: f32,
    /// Auto-generated / lockfile globs refused when non-empty.
    pub blocked_globs: Vec<String>,
}

impl Default for HashlineOpts {
    fn default() -> Self {
        Self {
            enforce_seen_lines: false,
            fuzzy_match: true,
            fuzzy_threshold: DEFAULT_FUZZY_THRESHOLD,
            blocked_globs: Vec::new(),
        }
    }
}

/// The hashline editor: one read tracker plus the named registers that persist across calls.
pub struct HashlineEditor {
    tracker: Arc<FileReadTracker>,
    opts: HashlineOpts,
    registers: Mutex<HashMap<String, Vec<String>>>,
}

impl HashlineEditor {
    /// An editor that checks files against reads recorded in `tracker` using default options.
    pub fn new(tracker: Arc<FileReadTracker>) -> Self {
        Self::with_opts(tracker, HashlineOpts::default())
    }

    /// An editor configured with explicit [`HashlineOpts`].
    pub fn with_opts(tracker: Arc<FileReadTracker>, opts: HashlineOpts) -> Self {
        Self {
            tracker,
            opts,
            registers: Mutex::default(),
        }
    }

    /// Apply the `[PATH#TAG]` sections of `input`, all or nothing. Blocking.
    ///
    /// # Errors
    ///
    /// When the input is malformed, a tag is stale, a line is unseen, a path is auto-generated,
    /// or a file cannot be written.
    pub fn edit(&self, ctx: &ToolContext, input: &str) -> Result<ToolOutput> {
        edit_sync(&self.tracker, &self.registers, ctx, input, &self.opts)
    }
}

/// The file paths named by the `[PATH#TAG]` headers of an edit `input`.
pub fn header_paths(input: &str) -> Vec<String> {
    parse::headers(input)
}

/// A file addressed by one or more sections, decoded and tagged.
struct Loaded {
    path: PathBuf,
    shown: String,
    file: TextFile,
    tag: String,
    lines: Vec<String>,
    /// The text ends with a newline (an empty file counts: new rows get one).
    final_newline: bool,
}

impl Loaded {
    fn open(path: PathBuf, shown: String) -> Result<Self> {
        let bytes = fs::read(&path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => fail(format!(
                "`{shown}` does not exist; use `write_file` to create it"
            )),
            _ => fail(format!("cannot read `{shown}`: {e}")),
        })?;
        if !fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            return Err(fail(format!("`{shown}` is not a regular file")));
        }
        let file = TextFile::decode(bytes).map_err(|e| fail(format!("`{shown}`: {e}")))?;
        let tag = tag_of(&file.text);
        let final_newline = file.text.is_empty() || file.text.ends_with('\n');
        let body = file.text.strip_suffix('\n').unwrap_or(&file.text);
        let lines = if file.text.is_empty() {
            Vec::new()
        } else {
            body.split('\n').map(str::to_owned).collect()
        };
        Ok(Self {
            path,
            shown,
            file,
            tag,
            lines,
            final_newline,
        })
    }

    fn render(&self, lines: &[String]) -> String {
        let mut text = lines.join("\n");
        if self.final_newline && !lines.is_empty() {
            text.push('\n');
        }
        text
    }
}

/// Resolve the path of a section or `MV` target: a file path against `cwd`, or a writable
/// file-backed internal URL (`local://`). Read-only internal URL schemes (`attachment://`,
/// `skill://`, etc.) are explicitly refused.
fn locate(ctx: &ToolContext, raw: &str) -> Result<(PathBuf, String)> {
    if let Some((scheme, rest)) = UrlRouter::split(raw) {
        if !scheme.eq_ignore_ascii_case("local") {
            return Err(fail(format!(
                "`{raw}` (`{scheme}://`) is read-only and cannot be edited, moved, or removed"
            )));
        }
        return match ctx.urls.read(ctx, scheme, rest)? {
            UrlBody::Path(path) => Ok((path, raw.to_owned())),
            UrlBody::Text(_) => Err(fail(format!(
                "`{raw}` is not file-backed and cannot be edited"
            ))),
        };
    }
    let path = resolve(&ctx.cwd, raw);
    let shown = display_rel(&ctx.cwd, &path);
    Ok((path, shown))
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

/// The refusal for a section whose tag is a placeholder or no longer matches.
fn tag_refusal(loaded: &Loaded, section: &Section, opts: &HashlineOpts) -> Option<String> {
    let (shown, current) = (&loaded.shown, &loaded.tag);
    let given = section.tag.trim_start_matches('#');
    let well_formed = given.len() == 4 && given.bytes().all(|b| b.is_ascii_hexdigit());
    if well_formed && given.eq_ignore_ascii_case(current) {
        return None;
    }
    if !well_formed {
        return Some(format!(
            "`{shown}`: `#{given}` is not a snapshot tag; copy the 4-hex tag from the latest read. \
             The current tag is #{current}."
        ));
    }
    let windows: Vec<(usize, usize)> = section
        .ops
        .iter()
        .flat_map(parse::Op::anchors)
        .map(|n| (n.saturating_sub(1), 1))
        .collect();
    let mut msg = format!(
        "stale tag for `{shown}`: your section says #{} but the file is now #{current}, so it \
         changed after your read (or the tag was copied wrongly).",
        given.to_ascii_uppercase()
    );
    if !windows.is_empty() {
        let _ = write!(
            msg,
            " Current rows near your anchors:\n[{shown}#{current}]\n{}",
            rows_around(&loaded.lines, &windows)
        );
    }
    if opts.fuzzy_match
        && let Some(hint) = near_miss_hint(&loaded.lines, section, opts.fuzzy_threshold)
    {
        let _ = write!(msg, "\nNear-miss match(es) in current file:\n{hint}");
    }
    let _ = write!(
        msg,
        "\nRe-issue the edit with #{current} and line numbers from the current content \
         (read the file again if unsure)."
    );
    Some(msg)
}

fn near_miss_hint(lines: &[String], section: &Section, threshold: f32) -> Option<String> {
    let body_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    for op in &section.ops {
        let (Op::Replace {
            src: Source::Body(rows),
            ..
        }
        | Op::Insert {
            src: Source::Body(rows),
            ..
        }) = op
        else {
            continue;
        };
        let needle: Vec<&str> = rows.iter().map(String::as_str).collect();
        if let Some(found) = nearest_above_threshold(&body_refs, &needle, threshold, 2) {
            return Some(found);
        }
    }
    None
}

/// Where a file ends up: deleted, or written (to `moved_to` when renamed).
enum Outcome {
    Removed,
    Written { text: String, moved_to: Moved },
}

type Moved = Option<(PathBuf, String)>;

/// One edited file with what its report needs.
struct Done<'a> {
    file: &'a Loaded,
    outcome: Outcome,
    notes: Vec<String>,
    /// The new lines and where the hunks landed, for the preview.
    lines: Vec<String>,
    hunks: Vec<(usize, usize)>,
    seen_ranges: Vec<Option<(usize, usize)>>,
    edit_deltas: Vec<(usize, usize, usize)>,
}

/// Load every file the sections address (once each; sections of one file share its snapshot)
/// and check their tags and auto-generated guards.
fn load_sections(
    ctx: &ToolContext,
    sections: &[Section],
    opts: &HashlineOpts,
) -> Result<(Vec<Loaded>, Vec<usize>)> {
    let mut loaded: Vec<Loaded> = Vec::new();
    let mut slots: Vec<usize> = Vec::with_capacity(sections.len());
    for section in sections {
        let (path, shown) = locate(ctx, &section.path)?;
        check_generated_guard(&path, &ctx.cwd, &shown, &opts.blocked_globs)?;
        let slot = if let Some(slot) = loaded.iter().position(|l| same_file(&l.path, &path)) {
            slot
        } else {
            loaded.push(Loaded::open(path, shown)?);
            loaded.len() - 1
        };
        slots.push(slot);
    }
    let refusals: Vec<String> = sections
        .iter()
        .zip(&slots)
        .filter_map(|(section, &slot)| tag_refusal(loaded.get(slot)?, section, opts))
        .collect();
    if refusals.is_empty() {
        Ok((loaded, slots))
    } else {
        Err(fail(refusals.join("\n\n")))
    }
}

fn verify_seen_done(tracker: &FileReadTracker, done: &[Done<'_>]) -> Result {
    for item in done {
        let path = &item.file.path;
        let shown = &item.file.shown;
        if matches!(item.outcome, Outcome::Removed) || item.seen_ranges.is_empty() {
            tracker.verify_file_seen(path, shown)?;
            continue;
        }
        for range in &item.seen_ranges {
            match *range {
                Some((start, end)) => tracker.verify_lines_seen(path, shown, start, end)?,
                None => tracker.verify_file_seen(path, shown)?,
            }
        }
    }
    Ok(())
}

/// Resolve every operation against its file's snapshot; nothing is written yet.
#[expect(
    clippy::too_many_arguments,
    reason = "internal planner threading tracker, sections, slots, registers, warnings, and opts"
)]
fn plan_files<'a>(
    ctx: &ToolContext,
    tracker: &FileReadTracker,
    sections: &[Section],
    slots: &[usize],
    loaded: &'a [Loaded],
    regs: &mut Registers,
    warnings: &mut Vec<String>,
    opts: &HashlineOpts,
) -> Result<Vec<Done<'a>>> {
    let mut plans: Vec<Plan<'_>> = loaded
        .iter()
        .map(|l| Plan::new(&l.lines, &l.shown))
        .collect();
    let mut removed = vec![false; loaded.len()];
    let mut moves: Vec<Moved> = vec![None; loaded.len()];
    for (section, &slot) in sections.iter().zip(slots) {
        let (Some(plan), Some(file)) = (plans.get_mut(slot), loaded.get(slot)) else {
            continue;
        };
        let shown = &file.shown;
        if section.remove {
            if !section.ops.is_empty() || section.mv.is_some() {
                return Err(fail(format!(
                    "`{shown}`: `REM` deletes the file and cannot be combined with other operations"
                )));
            }
            if let Some(flag) = removed.get_mut(slot) {
                *flag = true;
            }
            continue;
        }
        if section.ops.is_empty() && section.mv.is_none() {
            return Err(fail(format!(
                "`{shown}`: the section has a header but no operations (current tag #{})",
                file.tag
            )));
        }
        for op in &section.ops {
            plan.add(op, regs)
                .map_err(|e| fail(format!("`{shown}`: {e}")))?;
        }
        if let Some(dest) = &section.mv {
            let target = locate(ctx, dest)?;
            check_generated_guard(&target.0, &ctx.cwd, &target.1, &opts.blocked_globs)?;
            if same_file(&target.0, &file.path) {
                return Err(fail(format!("`{shown}`: `MV` destination is the source")));
            }
            if target.0.exists() {
                tracker.verify_fresh(&target.0)?;
                tracker.verify_file_seen(&target.0, &target.1)?;
            }
            if let Some(slot) = moves.get_mut(slot) {
                *slot = Some(target);
            }
        }
    }

    let mut done = Vec::with_capacity(loaded.len());
    for (((plan, file), gone), moved_to) in plans.into_iter().zip(loaded).zip(removed).zip(moves) {
        if gone {
            done.push(Done {
                file,
                outcome: Outcome::Removed,
                notes: Vec::new(),
                lines: Vec::new(),
                hunks: Vec::new(),
                seen_ranges: Vec::new(),
                edit_deltas: Vec::new(),
            });
            continue;
        }
        let applied = plan
            .finish()
            .map_err(|e| fail(format!("`{}`: {e}", file.shown)))?;
        if applied.lines == file.lines && moved_to.is_none() {
            return Err(fail(format!(
                "`{}`: no change; the edit produces the content the file already has",
                file.shown
            )));
        }
        warnings.extend(
            applied
                .warnings
                .iter()
                .map(|w| format!("`{}`: {w}", file.shown)),
        );
        done.push(Done {
            file,
            outcome: Outcome::Written {
                text: file.render(&applied.lines),
                moved_to,
            },
            notes: applied.notes,
            lines: applied.lines,
            hunks: applied.hunks,
            seen_ranges: applied.seen_ranges,
            edit_deltas: applied.edit_deltas,
        });
    }
    Ok(done)
}

/// Write in section order; a failure leaves the already-written prefix in place.
fn write_files(tracker: &FileReadTracker, done: &[Done<'_>]) -> Result {
    for (n, item) in done.iter().enumerate() {
        let file = item.file;
        let result = match &item.outcome {
            Outcome::Removed => fs::remove_file(&file.path).map(|()| tracker.forget(&file.path)),
            Outcome::Written { text, moved_to } => {
                let bytes = TextFile {
                    text: text.clone(),
                    ..file.file.clone()
                }
                .encode();
                let target = moved_to.as_ref().map_or(&file.path, |(p, _)| p);
                write_atomic(target, &bytes).and_then(|()| {
                    tracker.record_current(target);
                    if moved_to.is_some() {
                        tracker.transfer_seen(&file.path, target, &item.edit_deltas);
                        fs::remove_file(&file.path).map(|()| tracker.forget(&file.path))
                    } else {
                        tracker.adjust_seen_after_edits(target, &item.edit_deltas);
                        Ok(())
                    }
                })
            }
        };
        result.map_err(|e| {
            fail(format!(
                "failed writing `{}` after {n} of {} file(s): {e}",
                file.shown,
                done.len()
            ))
        })?;
    }
    Ok(())
}

/// Per file the fresh `[path#TAG]` header, notes and the edited region; plus the unified diff.
fn report(done: &[Done<'_>], warnings: &[String]) -> ToolOutput {
    let mut content = String::new();
    let mut diff = String::new();
    for item in done {
        let file = item.file;
        match &item.outcome {
            Outcome::Removed => {
                let _ = writeln!(content, "Removed `{}`.", file.shown);
                diff.push_str(&unified_diff(&file.shown, &file.file.text, ""));
            }
            Outcome::Written { text, moved_to } => {
                let shown = moved_to.as_ref().map_or(&file.shown, |(_, s)| s);
                let _ = writeln!(content, "[{shown}#{}]", tag_of(text));
                if moved_to.is_some() {
                    let _ = writeln!(content, "Moved from `{}`.", file.shown);
                }
                for note in &item.notes {
                    let _ = writeln!(content, "{note}");
                }
                content.push_str(&rows_around(&item.lines, &item.hunks));
                if file.file.mixed_eol {
                    content.push_str("The file mixed CRLF and LF; line endings were normalized.\n");
                }
                diff.push_str(&unified_diff(shown, &file.file.text, text));
            }
        }
    }
    if !warnings.is_empty() {
        let _ = write!(content, "Warnings:\n{}\n", warnings.join("\n"));
    }
    let mut out = ToolOutput::ok(content.trim_end());
    out.diff = Some(diff);
    out
}

pub(crate) fn edit_sync(
    tracker: &FileReadTracker,
    store: &Mutex<HashMap<String, Vec<String>>>,
    ctx: &ToolContext,
    input: &str,
    opts: &HashlineOpts,
) -> Result<ToolOutput> {
    let sections = parse::parse(input).map_err(|e| fail(format!("edit: {e}")))?;
    if sections.is_empty() {
        return Err(fail(
            "edit: no `[PATH#TAG]` section found; start each file with the header from the \
             latest read, then `PUT` / `CUT` / `REM` / `MV` operations",
        ));
    }
    let (loaded, slots) = load_sections(ctx, &sections, opts)?;
    let mut regs = Registers {
        anon: None,
        named: store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    };
    let mut warnings = Vec::new();
    let done = plan_files(
        ctx,
        tracker,
        &sections,
        &slots,
        &loaded,
        &mut regs,
        &mut warnings,
        opts,
    )?;
    if opts.enforce_seen_lines {
        verify_seen_done(tracker, &done)?;
    }
    write_files(tracker, &done)?;
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend(regs.named);
    Ok(report(&done, &warnings))
}

#[cfg(test)]
mod tests;
