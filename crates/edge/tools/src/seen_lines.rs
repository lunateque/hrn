//! Per-file 1-based inclusive line-range tracking for `[edit] enforce_seen_lines`.

use std::fmt::Write as _;

/// Sorted, coalesced 1-based inclusive line intervals seen in one file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SeenRanges {
    /// Sorted, non-overlapping, non-adjacent `(start, end)` intervals (`1 <= start <= end`).
    ranges: Vec<(usize, usize)>,
    /// True once the file has been observed (even if it was empty).
    observed: bool,
}

impl SeenRanges {
    /// Mark an empty file as observed.
    pub(crate) fn mark_observed(&mut self) {
        self.observed = true;
    }

    /// Record a 1-based inclusive range `(start, end)` as seen and coalesce intervals.
    pub(crate) fn add(&mut self, start: usize, end: usize) {
        self.observed = true;
        if start == 0 || end < start {
            return;
        }
        self.ranges.push((start, end));
        self.coalesce();
    }

    /// Whether the file was observed at least once.
    pub(crate) const fn is_observed(&self) -> bool {
        self.observed
    }

    /// Whether every 1-based line in `start..=end` is covered by the seen ranges.
    pub(crate) fn covers(&self, start: usize, end: usize) -> bool {
        if start == 0 || end < start {
            return self.observed;
        }
        // Intervals are sorted and non-adjacent (gap >= 2), so a contiguous range `start..=end`
        // is completely covered iff a single merged interval contains both endpoints.
        self.ranges.iter().any(|&(a, b)| a <= start && b >= end)
    }

    /// Format the seen intervals compactly for refusal diagnostics (e.g. `1-8, 40-50` or `none`).
    pub(crate) fn describe(&self) -> String {
        if self.ranges.is_empty() {
            return if self.observed {
                "empty file".to_owned()
            } else {
                "none".to_owned()
            };
        }
        let mut out = String::new();
        for (i, &(a, b)) in self.ranges.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{a}-{b}");
        }
        out
    }

    /// Adjust tracked intervals after applying ascending non-overlapping edits `(lo, hi, new_len)`
    /// where `lo..hi` is a 0-based half-open original line span replaced by `new_len` lines.
    pub(crate) fn adjust_after_edits(&mut self, edits: &[(usize, usize, usize)]) {
        if !self.observed || edits.is_empty() {
            return;
        }
        let mut next: Vec<(usize, usize)> = Vec::with_capacity(self.ranges.len() + edits.len());
        let mut delta: isize = 0;
        let mut adjusted_existing = Vec::with_capacity(self.ranges.len());
        for &(a, b) in &self.ranges {
            let new_a = shift_point(a, edits, false);
            let new_b = shift_point(b, edits, true);
            if new_a >= 1 && new_b >= new_a {
                adjusted_existing.push((new_a, new_b));
            }
        }
        next.extend(adjusted_existing);
        for &(lo, hi, new_len) in edits {
            let old_len = hi.saturating_sub(lo);
            let start_0 = isize::try_from(lo).unwrap_or(0).saturating_add(delta);
            if new_len > 0 && start_0 >= 0 {
                let s1 = usize::try_from(start_0).unwrap_or(0) + 1;
                next.push((s1, s1 + new_len - 1));
            }
            delta = delta
                .saturating_add(isize::try_from(new_len).unwrap_or(0))
                .saturating_sub(isize::try_from(old_len).unwrap_or(0));
        }
        self.ranges = next;
        self.coalesce();
    }

    fn coalesce(&mut self) {
        if self.ranges.len() <= 1 {
            return;
        }
        self.ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(self.ranges.len());
        for (a, b) in self.ranges.drain(..) {
            match merged.last_mut() {
                Some(last) if a <= last.1.saturating_add(1) => {
                    last.1 = last.1.max(b);
                }
                _ => merged.push((a, b)),
            }
        }
        self.ranges = merged;
    }
}

/// Shift one 1-based endpoint across ascending non-overlapping edits `(lo, hi, new_len)`.
fn shift_point(line: usize, edits: &[(usize, usize, usize)], is_end: bool) -> usize {
    let mut pos = isize::try_from(line).unwrap_or(0);
    let mut delta: isize = 0;
    for &(lo, hi, new_len) in edits {
        let edit_start_1 = lo + 1;
        let old_len = hi.saturating_sub(lo);
        let diff = isize::try_from(new_len)
            .unwrap_or(0)
            .saturating_sub(isize::try_from(old_len).unwrap_or(0));
        if hi == lo {
            // Pure insertion before 1-based `lo + 1`.
            if line >= edit_start_1 {
                delta = delta.saturating_add(diff);
            }
        } else if line > hi {
            delta = delta.saturating_add(diff);
        } else if line >= edit_start_1 {
            // Endpoint fell inside `[edit_start_1, hi]`.
            let new_start = isize::try_from(edit_start_1)
                .unwrap_or(1)
                .saturating_add(delta);
            pos = if is_end {
                new_start
                    .saturating_add(isize::try_from(new_len).unwrap_or(0))
                    .saturating_sub(1)
            } else {
                new_start
            };
            return usize::try_from(pos.max(0)).unwrap_or(0);
        }
    }
    usize::try_from(pos.saturating_add(delta).max(0)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_adjacent_and_overlapping_ranges() {
        let mut s = SeenRanges::default();
        s.add(10, 20);
        s.add(1, 5);
        s.add(6, 9);
        assert!(s.covers(1, 20));
        assert!(!s.covers(1, 21));
        assert_eq!(s.describe(), "1-20");
    }

    #[test]
    fn shifts_intervals_after_insert_and_delete() {
        let mut s = SeenRanges::default();
        s.add(1, 10);
        s.add(30, 40);
        // Replace 1-based line 5 (0-based 4..5) with 3 lines (+2 net).
        s.adjust_after_edits(&[(4, 5, 3)]);
        assert!(s.covers(1, 12));
        assert!(!s.covers(13, 13));
        assert!(s.covers(32, 42));
    }
}
