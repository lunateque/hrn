//! Pure Vim motions and atomic attachment placeholder helpers for [`crate::vim`].
//!
//! Attachment placeholders such as `[Image #1]` or `[file path]` are treated as indivisible
//! tokens: cursor movement snaps across them and edit ranges expand to include any overlapping
//! placeholder whole rather than splitting it mid-token.

use std::ops::Range;

/// Direction and inclusiveness of a resolved motion for operator application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MotionResult {
    pub(crate) target: usize,
    pub(crate) inclusive: bool,
    pub(crate) linewise: bool,
}

/// Last character-find motion (`f`/`F`/`t`/`T`) remembered for `;` and `,`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CharFind {
    pub(crate) ch: char,
    pub(crate) forward: bool,
    pub(crate) till: bool,
}

/// Start and exclusive end (at `\n` or buffer end) of the line containing `at`.
pub(crate) fn line_bounds(chars: &[char], at: usize) -> (usize, usize) {
    let at = at.min(chars.len());
    let before = chars.get(..at).unwrap_or_default();
    let start = before.iter().rposition(|c| *c == '\n').map_or(0, |p| p + 1);
    let after = chars.get(at..).unwrap_or_default();
    let end = after
        .iter()
        .position(|c| *c == '\n')
        .map_or(chars.len(), |p| at + p);
    (start, end)
}

/// Last valid cursor index on the line containing `at` in Normal mode (start on an empty line).
pub(crate) fn normal_line_end(chars: &[char], at: usize) -> usize {
    let (start, end) = line_bounds(chars, at);
    if end <= start {
        start
    } else {
        snap_backward(chars, end - 1)
    }
}

/// Clamp `pos` so it stays on a valid Normal-mode character on its line and never inside a
/// placeholder token.
pub(crate) fn clamp_normal(chars: &[char], pos: usize) -> usize {
    if chars.is_empty() {
        return 0;
    }
    let pos = pos.min(chars.len().saturating_sub(1));
    let (start, end) = line_bounds(chars, pos);
    let clamped = if end <= start {
        start
    } else {
        pos.clamp(start, end - 1)
    };
    snap_backward(chars, clamped)
}

/// Locate all atomic attachment placeholder spans (`[Image #N]`, `[file ...]`, `[attachment #N]`,
/// `[paste #N]`, case-insensitive) in `chars`.
pub(crate) fn placeholder_spans(chars: &[char]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars.get(i) != Some(&'[') {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while let Some(&c) = chars.get(j) {
            if c == ']' || c == '\n' || c == '[' {
                break;
            }
            j += 1;
        }
        if chars.get(j) == Some(&']')
            && is_attachment_inner(chars.get(i + 1..j).unwrap_or_default())
        {
            out.push(i..j + 1);
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

fn is_attachment_inner(inner: &[char]) -> bool {
    let text: String = inner.iter().collect();
    let low = text.to_ascii_lowercase();
    if let Some(rest) = low
        .strip_prefix("image #")
        .or_else(|| low.strip_prefix("img #"))
        .or_else(|| low.strip_prefix("paste #"))
        .or_else(|| low.strip_prefix("attachment #"))
    {
        return rest.chars().next().is_some_and(|c| c.is_ascii_digit());
    }
    if let Some(rest) = low
        .strip_prefix("file ")
        .or_else(|| low.strip_prefix("file:"))
        .or_else(|| low.strip_prefix("file #"))
    {
        return !rest.trim().is_empty();
    }
    false
}

/// Placeholder span containing `pos` (`start <= pos < end`), if any.
pub(crate) fn placeholder_at(chars: &[char], pos: usize) -> Option<Range<usize>> {
    placeholder_spans(chars)
        .into_iter()
        .find(|p| p.start <= pos && pos < p.end)
}

/// Placeholder span whose exclusive end equals `pos`, if any (used by Backspace / Left).
pub(crate) fn placeholder_ending_at(chars: &[char], pos: usize) -> Option<Range<usize>> {
    placeholder_spans(chars).into_iter().find(|p| p.end == pos)
}

/// If `pos` sits strictly inside an atomic placeholder, snap backward to its start.
pub(crate) fn snap_backward(chars: &[char], pos: usize) -> usize {
    match placeholder_at(chars, pos) {
        Some(p) if pos > p.start => p.start,
        _ => pos,
    }
}

/// Expand `[start, end)` so it never starts or ends strictly inside any attachment placeholder.
pub(crate) fn expand_range_for_placeholders(
    chars: &[char],
    mut start: usize,
    mut end: usize,
) -> (usize, usize) {
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    for p in placeholder_spans(chars) {
        if p.start < end && start < p.end {
            start = start.min(p.start);
            end = end.max(p.end);
        }
    }
    (start.min(chars.len()), end.min(chars.len()))
}

/// Move one unit left within the current line, jumping across atomic placeholders.
pub(crate) fn move_left(chars: &[char], pos: usize, count: usize) -> usize {
    let (start, _) = line_bounds(chars, pos);
    let mut cur = snap_backward(chars, pos);
    for _ in 0..count.max(1) {
        if cur <= start {
            break;
        }
        cur = snap_backward(chars, cur - 1).max(start);
    }
    cur
}

/// Move one unit right within the current line, jumping across atomic placeholders.
pub(crate) fn move_right(chars: &[char], pos: usize, count: usize, allow_eol: bool) -> usize {
    let (_, end) = line_bounds(chars, pos);
    let limit = if allow_eol {
        end
    } else {
        normal_line_end(chars, pos)
    };
    let mut cur = snap_backward(chars, pos);
    for _ in 0..count.max(1) {
        if let Some(p) = placeholder_at(chars, cur) {
            if p.end <= limit {
                cur = p.end;
            } else {
                break;
            }
        } else if cur < limit {
            cur += 1;
        } else {
            break;
        }
    }
    cur
}

/// First non-whitespace character on the line containing `at` (or line start if blank).
pub(crate) fn first_non_blank(chars: &[char], at: usize) -> usize {
    let (start, end) = line_bounds(chars, at);
    for i in start..end {
        if chars.get(i).is_some_and(|c| !c.is_whitespace()) {
            return snap_backward(chars, i);
        }
    }
    start
}

/// Move up (`delta < 0`) or down (`delta > 0`) by `count` lines, trying to preserve `col`.
pub(crate) fn move_vertical(chars: &[char], pos: usize, down: bool, count: usize) -> usize {
    let (mut start, mut end) = line_bounds(chars, pos);
    let col = pos.saturating_sub(start);
    for _ in 0..count.max(1) {
        if down {
            if end >= chars.len() {
                break;
            }
            (start, end) = line_bounds(chars, end + 1);
        } else {
            if start == 0 {
                break;
            }
            (start, end) = line_bounds(chars, start - 1);
        }
    }
    let last = end.saturating_sub(1).max(start);
    let target = if end <= start {
        start
    } else {
        (start + col).min(last)
    };
    snap_backward(chars, target)
}

/// 1-based line number start index (`line_no = 1` is the first line; clamps to last line).
pub(crate) fn goto_line(chars: &[char], line_no: usize) -> usize {
    let target_line = line_no.saturating_sub(1);
    let mut current = 0usize;
    let mut start = 0usize;
    for (i, &c) in chars.iter().enumerate() {
        if current == target_line {
            return first_non_blank(chars, start);
        }
        if c == '\n' {
            current += 1;
            start = i + 1;
        }
    }
    first_non_blank(chars, start)
}

/// Go to the last line's first non-blank character.
pub(crate) fn goto_last_line(chars: &[char]) -> usize {
    let last_start = chars.iter().rposition(|c| *c == '\n').map_or(0, |p| p + 1);
    first_non_blank(chars, last_start)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Space,
    Newline,
    Word,
    Punct,
    Placeholder,
}

fn token_at(chars: &[char], idx: usize, spans: &[Range<usize>]) -> (TokenKind, Range<usize>) {
    if let Some(p) = spans.iter().find(|p| p.start <= idx && idx < p.end) {
        return (TokenKind::Placeholder, p.clone());
    }
    match chars.get(idx) {
        None | Some('\n') => (TokenKind::Newline, idx..idx.saturating_add(1)),
        Some(c) if c.is_whitespace() => (TokenKind::Space, idx..idx + 1),
        Some(c) if c.is_alphanumeric() || *c == '_' => (TokenKind::Word, idx..idx + 1),
        Some(_) => (TokenKind::Punct, idx..idx + 1),
    }
}

fn kind_matches(a: TokenKind, b: TokenKind, big_word: bool) -> bool {
    if a == TokenKind::Placeholder || b == TokenKind::Placeholder {
        return false;
    }
    if big_word {
        !matches!(a, TokenKind::Space | TokenKind::Newline)
            && !matches!(b, TokenKind::Space | TokenKind::Newline)
    } else {
        a == b
    }
}

/// `w` / `W`: forward to the start of the next word / WORD.
pub(crate) fn word_forward(chars: &[char], pos: usize, count: usize, big_word: bool) -> usize {
    let spans = placeholder_spans(chars);
    let mut cur = snap_backward(chars, pos.min(chars.len()));
    for _ in 0..count.max(1) {
        if cur >= chars.len() {
            break;
        }
        let (start_kind, first_span) = token_at(chars, cur, &spans);
        if start_kind == TokenKind::Placeholder {
            cur = first_span.end;
        } else if !matches!(start_kind, TokenKind::Space | TokenKind::Newline) {
            cur = first_span.end;
            while cur < chars.len() {
                let (k, r) = token_at(chars, cur, &spans);
                if !kind_matches(start_kind, k, big_word) {
                    break;
                }
                cur = r.end;
            }
        }
        while cur < chars.len() {
            let (k, r) = token_at(chars, cur, &spans);
            if !matches!(k, TokenKind::Space | TokenKind::Newline) {
                break;
            }
            cur = r.end;
        }
    }
    cur.min(chars.len())
}

/// `b` / `B`: backward to the start of the previous word / WORD.
pub(crate) fn word_backward(chars: &[char], pos: usize, count: usize, big_word: bool) -> usize {
    let spans = placeholder_spans(chars);
    let mut cur = snap_backward(chars, pos.min(chars.len()));
    for _ in 0..count.max(1) {
        if cur == 0 {
            break;
        }
        cur = step_token_prev(chars, cur, &spans);
        while cur > 0 {
            let (k, _) = token_at(chars, cur, &spans);
            if !matches!(k, TokenKind::Space | TokenKind::Newline) {
                break;
            }
            cur = step_token_prev(chars, cur, &spans);
        }
        let (target_kind, _) = token_at(chars, cur, &spans);
        if target_kind == TokenKind::Placeholder {
            continue;
        }
        while cur > 0 {
            let prev = step_token_prev(chars, cur, &spans);
            let (pk, _) = token_at(chars, prev, &spans);
            if !kind_matches(target_kind, pk, big_word) {
                break;
            }
            cur = prev;
        }
    }
    cur
}

fn step_token_prev(chars: &[char], pos: usize, spans: &[Range<usize>]) -> usize {
    if pos == 0 {
        return 0;
    }
    let prev = pos - 1;
    if let Some(p) = spans.iter().find(|p| p.start <= prev && prev < p.end) {
        p.start
    } else {
        prev.min(chars.len().saturating_sub(1))
    }
}

/// `e` / `E`: forward to the end of the current/next word / WORD (inclusive).
pub(crate) fn word_end(chars: &[char], pos: usize, count: usize, big_word: bool) -> usize {
    let spans = placeholder_spans(chars);
    let mut cur = snap_backward(chars, pos.min(chars.len()));
    for _ in 0..count.max(1) {
        if cur >= chars.len() {
            break;
        }
        let (_, first_span) = token_at(chars, cur, &spans);
        cur = first_span.end;
        while cur < chars.len() {
            let (k, r) = token_at(chars, cur, &spans);
            if !matches!(k, TokenKind::Space | TokenKind::Newline) {
                break;
            }
            cur = r.end;
        }
        if cur >= chars.len() {
            return chars.len().saturating_sub(1);
        }
        let (word_kind, span) = token_at(chars, cur, &spans);
        if word_kind == TokenKind::Placeholder {
            cur = span.start;
            continue;
        }
        let mut last = cur;
        let mut next = span.end;
        while next < chars.len() {
            let (nk, nr) = token_at(chars, next, &spans);
            if !kind_matches(word_kind, nk, big_word) {
                break;
            }
            last = next;
            next = nr.end;
        }
        cur = last;
    }
    cur.min(chars.len().saturating_sub(1))
}

/// `f` / `F` / `t` / `T` character find on the current line, skipping atomic placeholders.
pub(crate) fn find_char(chars: &[char], pos: usize, find: CharFind, count: usize) -> Option<usize> {
    let (start, end) = line_bounds(chars, pos);
    let spans = placeholder_spans(chars);
    let in_placeholder = |idx: usize| spans.iter().any(|p| p.start <= idx && idx < p.end);
    let mut cur = snap_backward(chars, pos);
    for _ in 0..count.max(1) {
        let found = if find.forward {
            let search_start = match spans.iter().find(|p| p.start <= cur && cur < p.end) {
                Some(p) => p.end,
                None => cur + 1,
            };
            (search_start..end).find(|&i| !in_placeholder(i) && chars.get(i) == Some(&find.ch))
        } else {
            (start..cur)
                .rev()
                .find(|&i| !in_placeholder(i) && chars.get(i) == Some(&find.ch))
        };
        let hit = found?;
        cur = if find.till {
            if find.forward {
                hit.saturating_sub(1)
            } else {
                (hit + 1).min(end.saturating_sub(1))
            }
        } else {
            hit
        };
    }
    Some(snap_backward(chars, cur))
}

/// `%`: jump to matching `()`, `[]`, `{}`, `<>` bracket, skipping brackets inside placeholders.
pub(crate) fn match_bracket(chars: &[char], pos: usize) -> Option<usize> {
    let spans = placeholder_spans(chars);
    let in_ph = |i: usize| spans.iter().any(|p| p.start <= i && i < p.end);
    let (_, line_end) = line_bounds(chars, pos);
    let idx = (pos..line_end).find(|&i| {
        !in_ph(i)
            && chars
                .get(i)
                .is_some_and(|c| matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'))
    })?;
    let &ch = chars.get(idx)?;
    let (open, close, forward) = match ch {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        '<' => ('<', '>', true),
        '>' => ('<', '>', false),
        _ => return None,
    };
    let mut depth = 0usize;
    if forward {
        for i in idx..chars.len() {
            if in_ph(i) {
                continue;
            }
            if chars.get(i) == Some(&open) {
                depth += 1;
            } else if chars.get(i) == Some(&close) {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    } else {
        for i in (0..=idx).rev() {
            if in_ph(i) {
                continue;
            }
            if chars.get(i) == Some(&close) {
                depth += 1;
            } else if chars.get(i) == Some(&open) {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}

fn is_blank_line(chars: &[char], start: usize, end: usize) -> bool {
    chars
        .get(start..end)
        .unwrap_or_default()
        .iter()
        .all(|c| c.is_whitespace())
}

/// `{` / `}`: jump to previous or next paragraph boundary (blank line or buffer start/end).
pub(crate) fn move_paragraph(chars: &[char], pos: usize, forward: bool, count: usize) -> usize {
    let mut cur = pos.min(chars.len());
    for _ in 0..count.max(1) {
        let (mut start, mut end) = line_bounds(chars, cur);
        let mut saw_non_blank = !is_blank_line(chars, start, end);
        loop {
            if forward {
                if end >= chars.len() {
                    cur = normal_line_end(chars, end);
                    break;
                }
                (start, end) = line_bounds(chars, end + 1);
            } else {
                if start == 0 {
                    cur = 0;
                    break;
                }
                (start, end) = line_bounds(chars, start - 1);
            }
            let blank = is_blank_line(chars, start, end);
            if blank && saw_non_blank {
                cur = start;
                break;
            }
            if !blank {
                saw_non_blank = true;
            }
        }
    }
    snap_backward(chars, cur)
}
