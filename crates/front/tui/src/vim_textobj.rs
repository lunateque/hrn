//! Vim text objects (`iw`/`aw`, `iW`/`aW`, quotes `"'` + backtick, pairs `()[]{}<>` and `b`/`B`
//! aliases, paragraphs `ip`/`ap`), preserving atomic attachment placeholders.

use crate::vim_motion::{
    expand_range_for_placeholders, line_bounds, placeholder_at, placeholder_spans,
};

/// A resolved character range for an operator or visual selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) linewise: bool,
}

/// Resolve a text object specified by `(around, kind)` at `cursor`.
///
/// Supported `kind`:
/// `w`, `W`, `"`, `'`, `` ` ``, `(`, `)`, `b`, `[`, `]`, `{`, `}`, `B`, `<`, `>`, `p`.
pub(crate) fn resolve(
    chars: &[char],
    cursor: usize,
    around: bool,
    kind: char,
) -> Option<TextRange> {
    if chars.is_empty() {
        return None;
    }
    let pos = cursor.min(chars.len().saturating_sub(1));
    let raw = match kind {
        'w' => word_object(chars, pos, around, false),
        'W' => word_object(chars, pos, around, true),
        '"' | '\'' | '`' => quote_object(chars, pos, around, kind),
        '(' | ')' | 'b' => pair_object(chars, pos, around, '(', ')'),
        '[' | ']' => pair_object(chars, pos, around, '[', ']'),
        '{' | '}' | 'B' => pair_object(chars, pos, around, '{', '}'),
        '<' | '>' => pair_object(chars, pos, around, '<', '>'),
        'p' => Some(paragraph_object(chars, pos, around)),
        _ => None,
    }?;
    if raw.linewise {
        return Some(raw);
    }
    let (start, end) = expand_range_for_placeholders(chars, raw.start, raw.end);
    Some(TextRange {
        start,
        end,
        linewise: false,
    })
}

fn word_class(c: char, big_word: bool) -> u8 {
    if c == '\n' {
        0
    } else if c.is_whitespace() {
        1
    } else if big_word || c.is_alphanumeric() || c == '_' {
        2
    } else {
        3
    }
}

fn word_object(chars: &[char], pos: usize, around: bool, big_word: bool) -> Option<TextRange> {
    let (line_start, line_end) = line_bounds(chars, pos);
    if line_start >= line_end {
        return None;
    }
    let (mut start, mut end) = if let Some(p) = placeholder_at(chars, pos) {
        (p.start, p.end)
    } else {
        let spans = placeholder_spans(chars);
        let in_ph = |i: usize| spans.iter().any(|p| p.start <= i && i < p.end);
        let cls = word_class(*chars.get(pos)?, big_word);
        let mut s = pos;
        while s > line_start && !in_ph(s - 1) {
            if chars.get(s - 1).map(|c| word_class(*c, big_word)) != Some(cls) {
                break;
            }
            s -= 1;
        }
        let mut e = pos + 1;
        while e < line_end && !in_ph(e) {
            if chars.get(e).map(|c| word_class(*c, big_word)) != Some(cls) {
                break;
            }
            e += 1;
        }
        (s, e)
    };

    if around {
        let is_space_obj = chars
            .get(start)
            .is_some_and(|c| c.is_whitespace() && *c != '\n');
        if is_space_obj {
            // On whitespace: include the following word.
            while end < line_end && chars.get(end).is_some_and(|c| !c.is_whitespace()) {
                end += 1;
            }
        } else {
            let mut after = end;
            while after < line_end
                && chars
                    .get(after)
                    .is_some_and(|c| c.is_whitespace() && *c != '\n')
            {
                after += 1;
            }
            if after > end {
                end = after;
            } else {
                while start > line_start
                    && chars
                        .get(start - 1)
                        .is_some_and(|c| c.is_whitespace() && *c != '\n')
                {
                    start -= 1;
                }
            }
        }
    }
    Some(TextRange {
        start,
        end,
        linewise: false,
    })
}

fn quote_object(chars: &[char], pos: usize, around: bool, quote: char) -> Option<TextRange> {
    let (line_start, line_end) = line_bounds(chars, pos);
    let spans = placeholder_spans(chars);
    let in_ph = |i: usize| spans.iter().any(|p| p.start <= i && i < p.end);
    let mut quotes = Vec::new();
    let mut i = line_start;
    while i < line_end {
        if !in_ph(i) && chars.get(i) == Some(&quote) {
            let escaped = i > line_start && chars.get(i - 1) == Some(&'\\');
            if !escaped {
                quotes.push(i);
            }
        }
        i += 1;
    }
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut idx = 0;
    while idx + 1 < quotes.len() {
        if let (Some(&open), Some(&close)) = (quotes.get(idx), quotes.get(idx + 1)) {
            pairs.push((open, close));
        }
        idx += 2;
    }
    // Prefer a pair containing `pos`, otherwise the first pair starting after `pos` on the line.
    let (open, close) = pairs
        .iter()
        .copied()
        .find(|&(o, c)| o <= pos && pos <= c)
        .or_else(|| pairs.iter().copied().find(|&(o, _)| o >= pos))?;
    let (start, end) = if around {
        (open, close + 1)
    } else {
        (open + 1, close)
    };
    Some(TextRange {
        start,
        end,
        linewise: false,
    })
}

fn pair_object(
    chars: &[char],
    pos: usize,
    around: bool,
    open_ch: char,
    close_ch: char,
) -> Option<TextRange> {
    let spans = placeholder_spans(chars);
    let in_ph = |i: usize| spans.iter().any(|p| p.start <= i && i < p.end);

    // Search backward from `pos` for the innermost enclosing unmatched `open_ch`.
    let mut found_open = None;
    let mut depth = 0usize;
    let mut i = pos;
    loop {
        if !in_ph(i) {
            let ch = chars.get(i).copied();
            if ch == Some(close_ch) && i != pos {
                depth += 1;
            } else if ch == Some(open_ch) {
                if depth == 0 {
                    found_open = Some(i);
                    break;
                }
                depth -= 1;
            }
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }

    // If not enclosed, look forward on the current line for the next `open_ch` (standard Vim).
    let open = if let Some(o) = found_open {
        o
    } else {
        let (_, line_end) = line_bounds(chars, pos);
        (pos..line_end).find(|&k| !in_ph(k) && chars.get(k) == Some(&open_ch))?
    };

    // Match the closing delimiter forward from `open`.
    let mut depth = 0usize;
    let mut close = None;
    for k in open..chars.len() {
        if in_ph(k) {
            continue;
        }
        if chars.get(k) == Some(&open_ch) {
            depth += 1;
        } else if chars.get(k) == Some(&close_ch) {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                close = Some(k);
                break;
            }
        }
    }
    let close = close?;
    let (start, end) = if around {
        (open, close + 1)
    } else {
        (open + 1, close)
    };
    Some(TextRange {
        start,
        end,
        linewise: false,
    })
}

fn is_blank(chars: &[char], start: usize, end: usize) -> bool {
    chars
        .get(start..end)
        .unwrap_or_default()
        .iter()
        .all(|c| c.is_whitespace())
}

fn paragraph_object(chars: &[char], pos: usize, around: bool) -> TextRange {
    let (mut top_start, mut top_end) = line_bounds(chars, pos);
    let target_blank = is_blank(chars, top_start, top_end);
    while top_start > 0 {
        let (ps, pe) = line_bounds(chars, top_start - 1);
        if is_blank(chars, ps, pe) != target_blank {
            break;
        }
        top_start = ps;
        top_end = pe;
    }
    let (mut bot_start, mut bot_end) = line_bounds(chars, pos);
    while bot_end < chars.len() {
        let (ns, ne) = line_bounds(chars, bot_end + 1);
        if is_blank(chars, ns, ne) != target_blank {
            break;
        }
        bot_start = ns;
        bot_end = ne;
    }
    if around {
        let prev_bot_end = bot_end;
        while bot_end < chars.len() {
            let (ns, ne) = line_bounds(chars, bot_end + 1);
            if is_blank(chars, ns, ne) == target_blank {
                break;
            }
            bot_start = ns;
            bot_end = ne;
        }
        if bot_end == prev_bot_end {
            while top_start > 0 {
                let (ps, pe) = line_bounds(chars, top_start - 1);
                if is_blank(chars, ps, pe) == target_blank {
                    break;
                }
                top_start = ps;
                let _ = pe;
            }
        }
    }
    let _ = (top_end, bot_start);
    TextRange {
        start: top_start,
        end: bot_end,
        linewise: true,
    }
}
