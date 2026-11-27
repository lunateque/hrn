//! Turning parsed operations into line edits against one original snapshot, and applying them.

use super::block::{self, Syntax};
use super::parse::{Gap, Op, Source, Target};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Registers a `CUT` fills and a register-backed `PUT` reads. The anonymous register is
/// batch-local; named ones outlive the call (the tool publishes them once the writes landed).
#[derive(Default)]
pub(super) struct Registers {
    pub(super) anon: Option<Vec<String>>,
    pub(super) named: HashMap<String, Vec<String>>,
}

/// Replace original lines `lo..hi` (0-based, half-open) with `body`; `lo == hi` inserts.
#[derive(Debug)]
pub(super) struct Edit {
    lo: usize,
    hi: usize,
    body: Vec<String>,
    /// 1-based inclusive line span that must have been seen before this edit (`None` = file-seen
    /// only, e.g. appending into an empty file).
    seen_range: Option<(usize, usize)>,
}

/// The edits of every section of one file, all numbered against its original snapshot.
pub(super) struct Plan<'a> {
    lines: &'a [String],
    syn: Syntax,
    edits: Vec<Edit>,
    /// Block resolutions worth telling the model (`N*` -> `N-M`).
    pub(super) notes: Vec<String>,
    pub(super) warnings: Vec<String>,
}

impl<'a> Plan<'a> {
    pub(super) fn new(lines: &'a [String], path: &str) -> Self {
        Self {
            lines,
            syn: Syntax::for_path(path),
            edits: Vec::new(),
            notes: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.lines.len()
    }

    fn outside(&self, what: &str) -> String {
        let len = self.len();
        if len == 0 {
            format!("{what} is outside the file: it is empty (use `PUT >$:` to add rows)")
        } else {
            format!("{what} is outside the file ({len} lines)")
        }
    }

    fn block(&mut self, n: usize) -> Result<usize, String> {
        if n > self.len() {
            return Err(self.outside(&format!("line {n}")));
        }
        block::resolve(self.lines, n, self.syn)
    }

    fn span(&mut self, target: Target) -> Result<(usize, usize), String> {
        match target {
            Target::Range(a, b) => {
                if b > self.len() {
                    return Err(self.outside(&format!("range {a}.={b}")));
                }
                Ok((a - 1, b))
            }
            Target::Block(n) => {
                let end = self
                    .block(n)
                    .map_err(|e| format!("`{n}*` does not resolve: {e}"))?;
                self.notes
                    .push(format!("`{n}*` resolved to lines {n}-{end}"));
                Ok((n - 1, end))
            }
        }
    }

    fn gap(&mut self, gap: Gap) -> Result<(usize, Option<(usize, usize)>), String> {
        let len = self.len();
        match gap {
            Gap::End => Ok((len, (len > 0).then_some((len, len)))),
            Gap::Before(n) if n <= len.max(1) => Ok((n - 1, (len > 0).then_some((n, n)))),
            Gap::After(n) if n <= len => Ok((n, Some((n, n)))),
            Gap::AfterBlock(n) if n <= len => Ok(match self.block(n) {
                Ok(end) => {
                    self.notes
                        .push(format!("`{n}*` resolved to lines {n}-{end}"));
                    (end, Some((n, end)))
                }
                Err(why) => {
                    self.warnings
                        .push(format!("`PUT >{n}*:` inserted after line {n} only: {why}"));
                    (n, Some((n, n)))
                }
            }),
            Gap::Before(n) | Gap::After(n) | Gap::AfterBlock(n) => {
                Err(self.outside(&format!("line {n}")))
            }
        }
    }

    fn rows(src: &Source, regs: &Registers) -> Result<Vec<String>, String> {
        match src {
            Source::Body(rows) if rows.is_empty() => Err(
                "a `PUT ...:` needs at least one `+TEXT` row (use `CUT` to delete, `+` alone for a blank line)"
                    .to_owned(),
            ),
            Source::Body(rows) => Ok(rows.clone()),
            Source::Register(None) => regs.anon.clone().ok_or_else(|| {
                "the anonymous register is empty: `CUT` something earlier in this call".to_owned()
            }),
            Source::Register(Some(name)) => regs
                .named
                .get(name)
                .cloned()
                .ok_or_else(|| format!("unknown register `@{name}`: `CUT ... @{name}` first")),
        }
    }

    /// Resolve `op` against the snapshot and queue its edit.
    pub(super) fn add(&mut self, op: &Op, regs: &mut Registers) -> Result<(), String> {
        match op {
            Op::Replace { target, src } => {
                let (lo, hi) = self.span(*target)?;
                let body = Self::rows(src, regs)?;
                self.edits.push(Edit {
                    lo,
                    hi,
                    body,
                    seen_range: Some((lo + 1, hi)),
                });
            }
            Op::Insert { gap, src } => {
                let (at, seen_range) = self.gap(*gap)?;
                let body = Self::rows(src, regs)?;
                self.edits.push(Edit {
                    lo: at,
                    hi: at,
                    body,
                    seen_range,
                });
            }
            Op::Cut { target, name } => {
                let (lo, hi) = self.span(*target)?;
                let cut = self.lines.get(lo..hi).unwrap_or_default().to_vec();
                if let Some(name) = name {
                    regs.named.insert(name.clone(), cut.clone());
                }
                regs.anon = Some(cut);
                self.edits.push(Edit {
                    lo,
                    hi,
                    body: Vec::new(),
                    seen_range: Some((lo + 1, hi)),
                });
            }
        }
        Ok(())
    }

    /// Apply every queued edit; fails when two of them touch the same lines.
    pub(super) fn finish(mut self) -> Result<Applied, String> {
        self.edits.sort_by_key(|e| (e.lo, e.hi));
        for pair in self.edits.windows(2) {
            if let [prev, next] = pair
                && next.lo < prev.hi
            {
                return Err(format!(
                    "overlapping edits: lines {}-{} and {}-{} of the original touch the same rows; \
                     merge them into one hunk",
                    prev.lo + 1,
                    prev.hi,
                    next.lo + 1,
                    next.hi.max(next.lo + 1)
                ));
            }
        }
        let mut lines = Vec::with_capacity(self.lines.len());
        let mut hunks = Vec::with_capacity(self.edits.len());
        let mut seen_ranges = Vec::with_capacity(self.edits.len());
        let mut edit_deltas = Vec::with_capacity(self.edits.len());
        let mut cursor = 0;
        for edit in self.edits {
            lines.extend_from_slice(self.lines.get(cursor..edit.lo).unwrap_or_default());
            hunks.push((lines.len(), edit.body.len()));
            seen_ranges.push(edit.seen_range);
            edit_deltas.push((edit.lo, edit.hi, edit.body.len()));
            lines.extend(edit.body);
            cursor = edit.hi;
        }
        lines.extend_from_slice(self.lines.get(cursor..).unwrap_or_default());
        Ok(Applied {
            lines,
            hunks,
            seen_ranges,
            edit_deltas,
            notes: self.notes,
            warnings: self.warnings,
        })
    }
}

/// An edited file: its new lines and where the hunks landed.
pub(super) struct Applied {
    pub(super) lines: Vec<String>,
    /// `(first new line index, new line count)` per edit, ascending.
    pub(super) hunks: Vec<(usize, usize)>,
    /// Required 1-based seen line spans for each edit (`None` = file observed).
    pub(super) seen_ranges: Vec<Option<(usize, usize)>>,
    /// `(lo, hi, new_len)` per edit for adjusting [`crate::FileReadTracker`] seen ranges.
    pub(super) edit_deltas: Vec<(usize, usize, usize)>,
    pub(super) notes: Vec<String>,
    pub(super) warnings: Vec<String>,
}

const CONTEXT: usize = 2;
/// Rows of a post-edit preview before it is cut.
const PREVIEW_ROWS: usize = 80;

/// `LINE:TEXT` rows of `lines` around `(index, count)` windows; gaps between windows read `…`.
pub(super) fn rows_around(lines: &[String], windows: &[(usize, usize)]) -> String {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for &(start, count) in windows {
        let lo = start.saturating_sub(CONTEXT);
        let hi = (start + count + CONTEXT).min(lines.len());
        match spans.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => spans.push((lo, hi)),
        }
    }
    let mut out = String::new();
    let mut shown = 0;
    for (k, &(lo, hi)) in spans.iter().enumerate() {
        if k > 0 {
            out.push_str("…\n");
        }
        for (i, text) in lines.iter().enumerate().take(hi).skip(lo) {
            if shown == PREVIEW_ROWS {
                out.push_str("… (preview cut; read the file for the rest)\n");
                return out;
            }
            let _ = writeln!(out, "{}:{text}", i + 1);
            shown += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("l{i}")).collect()
    }

    fn body(rows: &[&str]) -> Source {
        Source::Body(rows.iter().map(|r| (*r).to_owned()).collect())
    }

    fn run(src: &[String], ops: &[Op]) -> Result<Vec<String>, String> {
        let mut plan = Plan::new(src, "f.txt");
        let mut regs = Registers::default();
        for op in ops {
            plan.add(op, &mut regs)?;
        }
        plan.finish().map(|a| a.lines)
    }

    #[test]
    fn all_numbers_refer_to_the_original_snapshot() {
        let got = run(
            &lines(5),
            &[
                Op::Replace {
                    target: Target::Range(2, 3),
                    src: body(&["A"]),
                },
                Op::Insert {
                    gap: Gap::After(5),
                    src: body(&["end"]),
                },
                Op::Insert {
                    gap: Gap::Before(1),
                    src: body(&["top"]),
                },
                Op::Cut {
                    target: Target::Range(4, 4),
                    name: None,
                },
            ],
        )
        .unwrap();
        assert_eq!(got, ["top", "l1", "A", "l5", "end"]);
    }

    #[test]
    fn cut_then_paste_moves_rows() {
        let got = run(
            &lines(4),
            &[
                Op::Cut {
                    target: Target::Range(1, 2),
                    name: Some("x".into()),
                },
                Op::Insert {
                    gap: Gap::End,
                    src: Source::Register(Some("x".into())),
                },
                Op::Insert {
                    gap: Gap::End,
                    src: Source::Register(None),
                },
            ],
        )
        .unwrap();
        assert_eq!(got, ["l3", "l4", "l1", "l2", "l1", "l2"]);
    }

    #[test]
    fn conflicts_and_bounds_are_refused() {
        let overlap = run(
            &lines(5),
            &[
                Op::Replace {
                    target: Target::Range(2, 4),
                    src: body(&["x"]),
                },
                Op::Insert {
                    gap: Gap::After(2),
                    src: body(&["y"]),
                },
            ],
        );
        assert!(overlap.unwrap_err().contains("overlapping"));
        let past = run(
            &lines(2),
            &[Op::Replace {
                target: Target::Range(2, 3),
                src: body(&["x"]),
            }],
        );
        assert!(past.unwrap_err().contains("outside the file (2 lines)"));
        let empty = run(
            &lines(1),
            &[Op::Replace {
                target: Target::Range(1, 1),
                src: body(&[]),
            }],
        );
        assert!(empty.unwrap_err().contains("at least one"));
        let missing = run(
            &lines(1),
            &[Op::Insert {
                gap: Gap::End,
                src: Source::Register(None),
            }],
        );
        assert!(missing.unwrap_err().contains("anonymous register"));
    }

    #[test]
    fn empty_files_take_head_and_tail_inserts() {
        for gap in [Gap::Before(1), Gap::End] {
            let got = run(
                &[],
                &[Op::Insert {
                    gap,
                    src: body(&["a"]),
                }],
            )
            .unwrap();
            assert_eq!(got, ["a"]);
        }
    }

    #[test]
    fn preview_merges_windows_and_marks_gaps() {
        let src = lines(20);
        let text = rows_around(&src, &[(1, 1), (3, 1), (15, 1)]);
        assert!(text.starts_with("1:l1\n"), "{text}");
        assert!(text.contains("…\n14:l14\n"), "{text}");
        assert!(text.contains("2:l2\n3:l3\n4:l4\n5:l5\n6:l6\n"), "{text}");
    }
}
