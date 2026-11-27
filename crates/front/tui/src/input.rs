//! The prompt editor: a multi-line buffer with a cursor, word-wrapped for display, readline-style
//! editing keys, atomic attachment placeholders, prompt history, and bounded cleared-draft recall.

use crate::theme;
use crate::vim_motion::{placeholder_at, placeholder_ending_at, snap_backward};
use crate::wrap::char_width;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::collections::VecDeque;
use std::ops::Range;

/// Maximum `Ctrl+C`-cleared drafts retained in memory for `Up` recall.
pub(crate) const MAX_CLEARED_DRAFTS: usize = 100;

struct BrowseState {
    /// Recall sequence in `Up` order (newest first): cleared drafts first, then submitted history.
    entries: Vec<String>,
    /// Current index into `entries` (`0` = newest).
    index: usize,
    /// Draft text that was in the buffer before browsing began.
    draft: Vec<char>,
}

#[derive(Default)]
pub(crate) struct Editor {
    chars: Vec<char>,
    /// Char index of the cursor.
    cursor: usize,
    history: Vec<String>,
    /// Bounded in-memory deque of `Ctrl+C`-cleared drafts (most recent at index 0).
    cleared_drafts: VecDeque<String>,
    /// Active Up/Down recall session.
    browsing: Option<BrowseState>,
}

/// The editor laid out for a given width.
pub(crate) struct Layout {
    pub(crate) lines: Vec<Line<'static>>,
    /// Cursor cell relative to the first returned line.
    pub(crate) cursor: (u16, u16),
}

fn clean(text: &str) -> impl Iterator<Item = char> + '_ {
    let mut chars = text.chars().peekable();
    std::iter::from_fn(move || {
        loop {
            let c = chars.next()?;
            match c {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    return Some('\n');
                }
                '\t' => return Some(' '),
                c if c.is_control() && c != '\n' => {}
                c => return Some(c),
            }
        }
    })
}

impl Editor {
    pub(crate) fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub(crate) fn chars(&self) -> &[char] {
        &self.chars
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn set_cursor(&mut self, pos: usize) {
        self.cursor = pos.min(self.chars.len());
    }

    pub(crate) fn is_blank(&self) -> bool {
        self.chars.iter().all(|c| c.is_whitespace())
    }

    pub(crate) fn set(&mut self, text: &str) {
        self.chars = clean(text).collect();
        self.cursor = self.chars.len();
        self.browsing = None;
    }

    pub(crate) fn replace_state(&mut self, chars: Vec<char>, cursor: usize) {
        self.chars = chars;
        self.cursor = cursor.min(self.chars.len());
        self.browsing = None;
    }

    pub(crate) fn splice_chars(&mut self, range: Range<usize>, replacement: &[char]) {
        let len = self.chars.len();
        let start = range.start.min(len);
        let end = range.end.min(len).max(start);
        self.chars.splice(start..end, replacement.iter().copied());
        self.cursor = self.cursor.min(self.chars.len());
        self.browsing = None;
    }

    /// Clear the current draft (`Ctrl+C`). When `record` is true and the draft is non-blank
    /// without inline secrets, push it to the bounded cleared-draft deque (skipping consecutive
    /// duplicates).
    pub(crate) fn clear_draft(&mut self, record: bool) {
        let text = self.text();
        if record
            && !text.trim().is_empty()
            && !carries_inline_secret(&text)
            && self.cleared_drafts.front().map(String::as_str) != Some(text.as_str())
        {
            self.cleared_drafts.push_front(text);
            while self.cleared_drafts.len() > MAX_CLEARED_DRAFTS {
                self.cleared_drafts.pop_back();
            }
        }
        self.chars.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    /// Take the text out, recording it in history unless it carries inline `/login` credentials
    /// (API keys or OAuth callback URLs).
    pub(crate) fn take(&mut self) -> String {
        let text = self.text();
        if !text.trim().is_empty()
            && !carries_inline_secret(&text)
            && self.history.last() != Some(&text)
        {
            self.history.push(text.clone());
        }
        self.chars.clear();
        self.cursor = 0;
        self.browsing = None;
        text
    }

    /// The recorded prompt history, oldest first.
    pub(crate) fn history(&self) -> &[String] {
        &self.history
    }

    pub(crate) fn insert(&mut self, text: &str) {
        let at = snap_backward(&self.chars, self.cursor.min(self.chars.len()));
        let added: Vec<char> = clean(text).collect();
        self.cursor = at + added.len();
        self.chars.splice(at..at, added);
        self.browsing = None;
    }

    pub(crate) fn insert_char(&mut self, c: char) {
        let at = snap_backward(&self.chars, self.cursor.min(self.chars.len()));
        self.chars.insert(at, c);
        self.cursor = at + 1;
        self.browsing = None;
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 || self.cursor > self.chars.len() {
            return;
        }
        if let Some(p) = placeholder_ending_at(&self.chars, self.cursor) {
            self.chars.drain(p.start..p.end);
            self.cursor = p.start;
        } else {
            let target = snap_backward(&self.chars, self.cursor - 1);
            if let Some(p) = placeholder_at(&self.chars, target) {
                self.chars.drain(p.start..p.end);
                self.cursor = p.start;
            } else {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
        }
        self.browsing = None;
    }

    pub(crate) fn delete(&mut self) {
        if self.cursor >= self.chars.len() {
            return;
        }
        if let Some(p) = placeholder_at(&self.chars, self.cursor) {
            self.chars.drain(p.start..p.end);
            self.cursor = p.start;
        } else {
            self.chars.remove(self.cursor);
        }
        self.browsing = None;
    }

    pub(crate) fn left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some(p) = placeholder_ending_at(&self.chars, self.cursor) {
            self.cursor = p.start;
        } else {
            self.cursor = snap_backward(&self.chars, self.cursor - 1);
        }
    }

    pub(crate) fn right(&mut self) {
        if self.cursor >= self.chars.len() {
            return;
        }
        if let Some(p) = placeholder_at(&self.chars, self.cursor) {
            self.cursor = p.end;
        } else {
            self.cursor = (self.cursor + 1).min(self.chars.len());
        }
    }

    fn is_space(&self, i: usize) -> bool {
        self.chars.get(i).is_none_or(|c| c.is_whitespace())
    }

    pub(crate) fn word_left(&mut self) {
        while self.cursor > 0 && self.is_space(self.cursor - 1) {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.is_space(self.cursor - 1) {
            self.cursor -= 1;
        }
        self.cursor = snap_backward(&self.chars, self.cursor);
    }

    pub(crate) fn word_right(&mut self) {
        let len = self.chars.len();
        while self.cursor < len && !self.is_space(self.cursor) {
            if let Some(p) = placeholder_at(&self.chars, self.cursor) {
                self.cursor = p.end;
            } else {
                self.cursor += 1;
            }
        }
        while self.cursor < len && self.is_space(self.cursor) {
            self.cursor += 1;
        }
    }

    pub(crate) fn delete_word(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.chars.drain(self.cursor..end);
        self.browsing = None;
    }

    /// Start and end (exclusive, at the `\n` or the text end) of the line holding `at`.
    fn line_bounds(&self, at: usize) -> (usize, usize) {
        let before = self.chars.get(..at).unwrap_or_default();
        let start = before.iter().rposition(|c| *c == '\n').map_or(0, |p| p + 1);
        let after = self.chars.get(at..).unwrap_or_default();
        let end = after
            .iter()
            .position(|c| *c == '\n')
            .map_or(self.chars.len(), |p| at + p);
        (start, end)
    }

    pub(crate) fn home(&mut self) {
        self.cursor = self.line_bounds(self.cursor).0;
    }

    pub(crate) fn end(&mut self) {
        self.cursor = self.line_bounds(self.cursor).1;
    }

    pub(crate) fn kill_to_start(&mut self) {
        let (start, _) = self.line_bounds(self.cursor);
        let end = snap_backward(&self.chars, self.cursor);
        self.chars.drain(start..end);
        self.cursor = start;
        self.browsing = None;
    }

    pub(crate) fn kill_to_end(&mut self) {
        let (_, end) = self.line_bounds(self.cursor);
        let start = snap_backward(&self.chars, self.cursor);
        self.chars.drain(start..end);
        self.cursor = start;
        self.browsing = None;
    }

    /// Move one line up; `false` on the first line (the caller may browse history instead).
    pub(crate) fn up(&mut self) -> bool {
        let (start, _) = self.line_bounds(self.cursor);
        if start == 0 {
            return false;
        }
        let column = self.cursor - start;
        let (prev_start, prev_end) = self.line_bounds(start - 1);
        self.cursor = snap_backward(&self.chars, prev_start + column.min(prev_end - prev_start));
        true
    }

    /// Move one line down; `false` on the last line.
    pub(crate) fn down(&mut self) -> bool {
        let (start, end) = self.line_bounds(self.cursor);
        if end >= self.chars.len() {
            return false;
        }
        let column = self.cursor - start;
        let (next_start, next_end) = self.line_bounds(end + 1);
        self.cursor = snap_backward(&self.chars, next_start + column.min(next_end - next_start));
        true
    }

    /// Browse backward: when `recall_cleared` is enabled and the prompt is currently blank,
    /// cleared drafts (most recent first) are recalled BEFORE submitted prompt history.
    pub(crate) fn history_prev_with(&mut self, recall_cleared: bool) {
        if let Some(state) = &mut self.browsing {
            if state.index + 1 >= state.entries.len() {
                return;
            }
            state.index += 1;
            let entry = state.entries.get(state.index).cloned().unwrap_or_default();
            self.chars = entry.chars().collect();
            self.cursor = self.chars.len();
            return;
        }
        let mut entries = Vec::new();
        if recall_cleared && self.is_blank() {
            entries.extend(self.cleared_drafts.iter().cloned());
        }
        entries.extend(self.history.iter().rev().cloned());
        if entries.is_empty() {
            return;
        }
        let draft = self.chars.clone();
        let entry = entries.first().cloned().unwrap_or_default();
        self.chars = entry.chars().collect();
        self.cursor = self.chars.len();
        self.browsing = Some(BrowseState {
            entries,
            index: 0,
            draft,
        });
    }

    pub(crate) fn history_next(&mut self) {
        let Some(mut state) = self.browsing.take() else {
            return;
        };
        if state.index > 0 {
            state.index -= 1;
            let entry = state.entries.get(state.index).cloned().unwrap_or_default();
            self.chars = entry.chars().collect();
            self.cursor = self.chars.len();
            self.browsing = Some(state);
        } else {
            self.chars = state.draft;
            self.cursor = self.chars.len();
        }
    }

    /// Lay the text out in `width` columns behind `prompt`, highlighting the optional char range
    /// `selection` (`[start, end)`) for Vim Visual / Visual-Line modes.
    pub(crate) fn layout_with_selection(
        &self,
        width: usize,
        prompt: &Span<'static>,
        style: Style,
        max_rows: usize,
        selection: Option<(usize, usize)>,
    ) -> Layout {
        let indent = prompt.width();
        let avail = width.saturating_sub(indent).max(1);
        let mut rows: Vec<(usize, usize)> = Vec::new();
        let mut cursor = (0, 0);
        let mut start = 0;
        for line in self.chars.split(|c| *c == '\n') {
            for (row_start, row_end) in break_rows(line, avail) {
                let (lo, hi) = (start + row_start, start + row_end);
                let is_last_row = row_end == line.len();
                if self.cursor >= lo && (self.cursor < hi || (is_last_row && self.cursor == hi)) {
                    let before = line.get(row_start..self.cursor - start).unwrap_or_default();
                    cursor = (
                        before.iter().map(|c| char_width(*c)).sum::<usize>(),
                        rows.len(),
                    );
                }
                rows.push((lo, hi));
            }
            start += line.len() + 1;
        }
        let skip = (cursor.1 + 1).saturating_sub(max_rows.max(1));
        let sel_style = style.bg(theme::SELECTED_BG);
        let lines = rows
            .into_iter()
            .enumerate()
            .skip(skip)
            .take(max_rows.max(1))
            .map(|(i, (lo, hi))| {
                let lead = if i == 0 {
                    prompt.clone()
                } else {
                    Span::raw(" ".repeat(indent))
                };
                let mut spans = vec![lead];
                spans.extend(styled_row_spans(
                    &self.chars,
                    lo,
                    hi,
                    style,
                    sel_style,
                    selection,
                ));
                Line::from(spans)
            })
            .collect();
        let x = (indent + cursor.0).min(width.saturating_sub(1));
        Layout {
            lines,
            cursor: (
                u16::try_from(x).unwrap_or(u16::MAX),
                u16::try_from(cursor.1 - skip).unwrap_or(u16::MAX),
            ),
        }
    }
}

fn styled_row_spans(
    chars: &[char],
    lo: usize,
    hi: usize,
    style: Style,
    sel_style: Style,
    selection: Option<(usize, usize)>,
) -> Vec<Span<'static>> {
    let slice = chars.get(lo..hi).unwrap_or_default();
    let Some((s_lo, s_hi)) = selection else {
        return vec![Span::styled(slice.iter().collect::<String>(), style)];
    };
    let ov_lo = s_lo.max(lo).min(hi);
    let ov_hi = s_hi.max(lo).min(hi);
    if ov_lo >= ov_hi {
        return vec![Span::styled(slice.iter().collect::<String>(), style)];
    }
    let mut out = Vec::new();
    if ov_lo > lo {
        let pre: String = chars.get(lo..ov_lo).unwrap_or_default().iter().collect();
        out.push(Span::styled(pre, style));
    }
    let mid: String = chars.get(ov_lo..ov_hi).unwrap_or_default().iter().collect();
    out.push(Span::styled(mid, sel_style));
    if ov_hi < hi {
        let post: String = chars.get(ov_hi..hi).unwrap_or_default().iter().collect();
        out.push(Span::styled(post, style));
    }
    out
}

/// Returns `true` for slash commands whose arguments may contain inline secrets (e.g.
/// `/login <provider> <api-key>` or `/login <redirect-url>`) so they never enter prompt history.
fn carries_inline_secret(text: &str) -> bool {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return false;
    };
    let Some((cmd, args)) = rest.split_once(char::is_whitespace) else {
        return false;
    };
    cmd.eq_ignore_ascii_case("login") && !args.trim().is_empty()
}

/// Split one logical line into rows of at most `avail` columns, breaking after spaces where
/// possible. Trailing spaces may hang past the edge rather than start a row.
fn break_rows(line: &[char], avail: usize) -> Vec<(usize, usize)> {
    let mut rows = Vec::new();
    let mut start = 0;
    let mut column = 0;
    let mut last_break = None;
    let mut i = 0;
    while let Some(&c) = line.get(i) {
        let w = char_width(c);
        if column + w > avail && column > 0 && c != ' ' {
            let at = last_break.filter(|b| *b > start).unwrap_or(i);
            rows.push((start, at));
            start = at;
            column = line
                .get(at..i)
                .unwrap_or_default()
                .iter()
                .map(|c| char_width(*c))
                .sum();
            last_break = None;
            continue;
        }
        if c == ' ' {
            last_break = Some(i + 1);
        }
        column += w;
        i += 1;
    }
    rows.push((start, line.len()));
    rows
}
