//! Balanced subshell (`(...)`, `{ ... ; }`), command substitution (`$(...)`, backticks), and
//! process substitution (`<(...)`, `>(...)`) extraction for command policy and critical-rule
//! scanning.

use super::QuoteState;

/// Extract inner command strings from outer `( ... )` / `{ ... ; }` groups, `$(...)`,
/// `<(...)` / `>(...)`, unquoted `( ... )` / `{ ... ; }`, and backtick substitutions while
/// ignoring literal text inside single quotes (`'...'` and `$'...'`).
#[must_use]
pub fn extract_subshell_commands(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let trimmed = segment.trim();
    if let Some(inner) = strip_balanced_outer(trimmed, '(', ')') {
        push_nonempty(&mut out, inner.trim());
    } else if let Some(inner) = strip_balanced_outer(trimmed, '{', '}') {
        push_nonempty(&mut out, inner.trim().trim_end_matches(';').trim());
    }
    scan_embedded_substitutions(segment, &mut out);
    out
}

fn push_nonempty(out: &mut Vec<String>, text: &str) {
    if !text.is_empty() && !out.iter().any(|existing| existing == text) {
        out.push(text.to_owned());
    }
}

fn strip_balanced_outer(text: &str, open: char, close: char) -> Option<&str> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    if chars.first().map(|(_, c)| *c) != Some(open) || chars.last().map(|(_, c)| *c) != Some(close)
    {
        return None;
    }
    let end = find_matching_delim(&chars, 0, open, close)?;
    if end + 1 == chars.len() {
        let start_byte = open.len_utf8();
        let end_byte = chars.get(end)?.0;
        text.get(start_byte..end_byte)
    } else {
        None
    }
}

fn scan_embedded_substitutions(segment: &str, out: &mut Vec<String>) {
    let chars: Vec<(usize, char)> = segment.char_indices().collect();
    let mut quote = QuoteState::None;
    let mut i = 0usize;
    while let Some(&(byte_idx, ch)) = chars.get(i) {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
                i += 1;
            }
            QuoteState::AnsiSingle => {
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                if ch == '\'' {
                    quote = QuoteState::None;
                }
                i += 1;
            }
            QuoteState::Double => {
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                    i += 1;
                } else if ch == '$' && chars.get(i + 1).map(|(_, c)| *c) == Some('(') {
                    i = extract_paren_body(segment, &chars, i + 1, out);
                } else if ch == '`' {
                    i = extract_backtick_body(segment, &chars, i, byte_idx, out);
                } else {
                    i += 1;
                }
            }
            QuoteState::None => {
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                if ch == '$' && chars.get(i + 1).map(|(_, c)| *c) == Some('\'') {
                    quote = QuoteState::AnsiSingle;
                    i += 2;
                } else if ch == '\'' {
                    quote = QuoteState::Single;
                    i += 1;
                } else if ch == '"' {
                    quote = QuoteState::Double;
                    i += 1;
                } else if matches!(ch, '$' | '<' | '>')
                    && chars.get(i + 1).map(|(_, c)| *c) == Some('(')
                {
                    i = extract_paren_body(segment, &chars, i + 1, out);
                } else if ch == '('
                    && !i
                        .checked_sub(1)
                        .and_then(|j| chars.get(j))
                        .is_some_and(|(_, prev)| {
                            prev.is_ascii_alphanumeric() || matches!(prev, '_' | ':')
                        })
                {
                    i = extract_paren_body(segment, &chars, i, out);
                } else if ch == '{'
                    && chars
                        .get(i + 1)
                        .is_some_and(|(_, c)| c.is_ascii_whitespace())
                    && !i
                        .checked_sub(1)
                        .and_then(|j| chars.get(j))
                        .is_some_and(|(_, prev)| prev.is_ascii_alphanumeric() || *prev == '$')
                {
                    i = extract_brace_body(segment, &chars, i, out);
                } else if ch == '`' {
                    i = extract_backtick_body(segment, &chars, i, byte_idx, out);
                } else {
                    i += 1;
                }
            }
        }
    }
}

fn extract_paren_body(
    segment: &str,
    chars: &[(usize, char)],
    open_idx: usize,
    out: &mut Vec<String>,
) -> usize {
    let Some(close_idx) = find_matching_delim(chars, open_idx, '(', ')') else {
        return open_idx + 1;
    };
    if let (Some(&(open_byte, _)), Some(&(close_byte, _))) =
        (chars.get(open_idx), chars.get(close_idx))
        && let Some(inner) = segment.get(open_byte + 1..close_byte)
    {
        push_nonempty(out, inner.trim());
    }
    close_idx + 1
}

fn extract_brace_body(
    segment: &str,
    chars: &[(usize, char)],
    open_idx: usize,
    out: &mut Vec<String>,
) -> usize {
    let Some(close_idx) = find_matching_delim(chars, open_idx, '{', '}') else {
        return open_idx + 1;
    };
    if let (Some(&(open_byte, _)), Some(&(close_byte, _))) =
        (chars.get(open_idx), chars.get(close_idx))
        && let Some(inner) = segment.get(open_byte + 1..close_byte)
    {
        push_nonempty(out, inner.trim().trim_end_matches(';').trim());
    }
    close_idx + 1
}

fn extract_backtick_body(
    segment: &str,
    chars: &[(usize, char)],
    open_idx: usize,
    open_byte: usize,
    out: &mut Vec<String>,
) -> usize {
    let mut j = open_idx + 1;
    while let Some(&(b, c)) = chars.get(j) {
        if c == '\\' {
            j += 2;
            continue;
        }
        if c == '`' {
            if let Some(raw) = segment.get(open_byte + 1..b) {
                let unescaped = raw
                    .replace("\\`", "`")
                    .replace("\\$", "$")
                    .replace("\\\\", "\\");
                push_nonempty(out, unescaped.trim());
            }
            return j + 1;
        }
        j += 1;
    }
    open_idx + 1
}

fn find_matching_delim(
    chars: &[(usize, char)],
    open_idx: usize,
    open: char,
    close: char,
) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote = QuoteState::None;
    let mut i = open_idx;
    while let Some(&(_, ch)) = chars.get(i) {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
                i += 1;
            }
            QuoteState::AnsiSingle | QuoteState::Double => {
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                if (quote == QuoteState::AnsiSingle && ch == '\'')
                    || (quote == QuoteState::Double && ch == '"')
                {
                    quote = QuoteState::None;
                }
                i += 1;
            }
            QuoteState::None => {
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                if ch == '$' && chars.get(i + 1).map(|(_, c)| *c) == Some('\'') {
                    quote = QuoteState::AnsiSingle;
                    i += 2;
                    continue;
                }
                if ch == '\'' {
                    quote = QuoteState::Single;
                } else if ch == '"' {
                    quote = QuoteState::Double;
                } else if ch == open {
                    depth += 1;
                } else if ch == close {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(i);
                    }
                }
                i += 1;
            }
        }
    }
    None
}
