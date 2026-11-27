//! Shell segment lexing, quoting (`'...'`, `$'...'`, `"..."`), comment stripping, line
//! continuation normalization, leading `KEY=VAL` peeling, and redirection/subshell scanning.

#[path = "command_policy_subshell.rs"]
mod subshell;
#[path = "command_policy_words.rs"]
mod words;

pub use subshell::extract_subshell_commands;
pub use words::{
    is_excluded_compound_builtin, normalize_command_word, normalize_segment_words, pattern_matches,
    shell_words, skip_command_wrappers, strip_bin_prefix, unwrap_segment_command,
};

/// Shell separator immediately preceding a [`CommandSegment`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentSep {
    /// `&&`
    And,
    /// `||`
    Or,
    /// `;`
    Semi,
    /// `|` or `|&`
    Pipe,
    /// `&`
    Bg,
    /// `\n`
    Newline,
}

/// One normalized command segment extracted from a shell command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSegment {
    /// Whitespace-collapsed segment text.
    pub text: String,
    /// Operator that preceded this segment (`None` for the first segment).
    pub sep_before: Option<SegmentSep>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum QuoteState {
    None,
    Single,
    AnsiSingle,
    Double,
}

fn line_continuation_len(chars: &[char], i: usize) -> usize {
    if chars.get(i) != Some(&'\\') {
        return 0;
    }
    match chars.get(i + 1) {
        Some(&'\n') => 2,
        Some(&'\r') if chars.get(i + 2) == Some(&'\n') => 3,
        _ => 0,
    }
}

/// Split `command` into segments on unquoted `&&`, `||`, `;`, `|`, `&`, and `\n` while preserving
/// `'...'`, `$'...'`, `"..."`, backslash escapes/continuations, `#` comments, and `>&`/`<&`/`&>`
/// redirections.
#[must_use]
pub fn split_segments(command: &str) -> Vec<CommandSegment> {
    let chars: Vec<char> = command.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = QuoteState::None;
    let mut next_sep: Option<SegmentSep> = None;
    let mut at_word_start = true;
    let mut i = 0usize;

    while let Some(&ch) = chars.get(i) {
        if quote != QuoteState::None {
            i = step_quoted_split(&chars, i, &mut cur, &mut quote);
            at_word_start = false;
            continue;
        }
        let cont = line_continuation_len(&chars, i);
        if cont > 0 {
            i += cont;
            continue;
        }
        if ch == '#' && at_word_start {
            while chars.get(i).is_some_and(|&c| c != '\n') {
                i += 1;
            }
            continue;
        }
        if ch == '\\' {
            cur.push(ch);
            if let Some(&esc) = chars.get(i + 1) {
                cur.push(esc);
                i += 2;
            } else {
                i += 1;
            }
            at_word_start = false;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'\'') {
            quote = QuoteState::AnsiSingle;
            cur.push_str("$'");
            at_word_start = false;
            i += 2;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = if ch == '\'' {
                QuoteState::Single
            } else {
                QuoteState::Double
            };
            cur.push(ch);
            at_word_start = false;
            i += 1;
            continue;
        }
        if let Some((sep, step)) = match_separator(&chars, i, &cur) {
            push_segment(&mut out, &mut cur, next_sep);
            next_sep = Some(sep);
            at_word_start = true;
            i += step;
            continue;
        }
        at_word_start = ch.is_ascii_whitespace();
        cur.push(ch);
        i += 1;
    }
    push_segment(&mut out, &mut cur, next_sep);
    out
}

fn step_quoted_split(chars: &[char], i: usize, cur: &mut String, quote: &mut QuoteState) -> usize {
    let Some(&ch) = chars.get(i) else {
        return i + 1;
    };
    match *quote {
        QuoteState::Single => {
            cur.push(ch);
            if ch == '\'' {
                *quote = QuoteState::None;
            }
            i + 1
        }
        QuoteState::AnsiSingle => {
            cur.push(ch);
            if ch == '\\'
                && let Some(&esc) = chars.get(i + 1)
            {
                cur.push(esc);
                return i + 2;
            }
            if ch == '\'' {
                *quote = QuoteState::None;
            }
            i + 1
        }
        QuoteState::Double => {
            let cont = line_continuation_len(chars, i);
            if cont > 0 {
                return i + cont;
            }
            cur.push(ch);
            if ch == '\\'
                && let Some(&esc) = chars.get(i + 1)
            {
                cur.push(esc);
                return i + 2;
            }
            if ch == '"' {
                *quote = QuoteState::None;
            }
            i + 1
        }
        QuoteState::None => i + 1,
    }
}

fn match_separator(chars: &[char], i: usize, cur: &str) -> Option<(SegmentSep, usize)> {
    let ch = *chars.get(i)?;
    let next = chars.get(i + 1).copied();
    match ch {
        '\n' => Some((SegmentSep::Newline, 1)),
        ';' => Some((SegmentSep::Semi, if next == Some(';') { 2 } else { 1 })),
        '|' => match next {
            Some('|') => Some((SegmentSep::Or, 2)),
            Some('&') => Some((SegmentSep::Pipe, 2)),
            _ => Some((SegmentSep::Pipe, 1)),
        },
        '&' => match next {
            Some('&') => Some((SegmentSep::And, 2)),
            Some('>') => None,
            _ if cur.ends_with('>') || cur.ends_with('<') => None,
            _ => Some((SegmentSep::Bg, 1)),
        },
        _ => None,
    }
}

fn push_segment(out: &mut Vec<CommandSegment>, cur: &mut String, sep_before: Option<SegmentSep>) {
    let collapsed = collapse_whitespace(cur);
    cur.clear();
    if !collapsed.is_empty() {
        out.push(CommandSegment {
            text: collapsed,
            sep_before,
        });
    }
}

/// Collapse runs of unquoted ASCII whitespace into single spaces, resolving `\<newline>`
/// continuations while preserving `'...'`, `$'...'`, and `"..."`.
#[must_use]
pub fn collapse_whitespace(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut quote = QuoteState::None;
    let mut pending_space = false;
    let mut i = 0usize;

    while let Some(&ch) = chars.get(i) {
        if quote != QuoteState::None {
            if pending_space && !out.is_empty() {
                out.push(' ');
                pending_space = false;
            }
            i = step_quoted_split(&chars, i, &mut out, &mut quote);
            continue;
        }
        let cont = line_continuation_len(&chars, i);
        if cont > 0 {
            i += cont;
            continue;
        }
        if ch.is_ascii_whitespace() {
            pending_space = true;
            i += 1;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        if ch == '\\' {
            out.push(ch);
            if let Some(&esc) = chars.get(i + 1) {
                out.push(esc);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'\'') {
            quote = QuoteState::AnsiSingle;
            out.push_str("$'");
            i += 2;
            continue;
        }
        if ch == '\'' {
            quote = QuoteState::Single;
        } else if ch == '"' {
            quote = QuoteState::Double;
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Peel leading `KEY=VAL` environment assignments from a normalized shell segment, returning
/// `(env_var_names, bare_command)`.
#[must_use]
pub fn peel_env_prefixes(segment: &str) -> (Vec<String>, String) {
    let bytes = segment.as_bytes();
    let mut pos = 0usize;
    let mut env_vars = Vec::new();

    while pos < bytes.len() {
        pos = skip_ws(bytes, pos);
        let start = pos;
        let Some((key, next_pos)) = scan_env_assignment(segment, pos) else {
            break;
        };
        let rest_start = skip_ws(bytes, next_pos);
        if rest_start >= bytes.len() {
            pos = start;
            break;
        }
        env_vars.push(key);
        pos = rest_start;
    }

    let bare = segment.get(pos..).unwrap_or(segment).trim().to_owned();
    (env_vars, bare)
}

pub(super) fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

fn scan_env_assignment(text: &str, start: usize) -> Option<(String, usize)> {
    let slice = text.get(start..)?;
    let bytes = slice.as_bytes();
    let first = *bytes.first()?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let mut idx = 1usize;
    while bytes
        .get(idx)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        idx += 1;
    }
    if bytes.get(idx) != Some(&b'=') {
        return None;
    }
    let key = slice.get(..idx)?.to_owned();
    idx += 1;
    let mut quote = QuoteState::None;
    while let Some(&b) = bytes.get(idx) {
        match quote {
            QuoteState::Single => {
                idx += 1;
                if b == b'\'' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::AnsiSingle | QuoteState::Double => {
                if b == b'\\' && idx + 1 < bytes.len() {
                    idx += 2;
                    continue;
                }
                idx += 1;
                if (quote == QuoteState::AnsiSingle && b == b'\'')
                    || (quote == QuoteState::Double && b == b'"')
                {
                    quote = QuoteState::None;
                }
            }
            QuoteState::None => {
                if b.is_ascii_whitespace() {
                    break;
                }
                if b == b'\\' && idx + 1 < bytes.len() {
                    idx += 2;
                    continue;
                }
                if b == b'$' && bytes.get(idx + 1) == Some(&b'\'') {
                    quote = QuoteState::AnsiSingle;
                    idx += 2;
                    continue;
                }
                if b == b'\'' {
                    quote = QuoteState::Single;
                } else if b == b'"' {
                    quote = QuoteState::Double;
                }
                idx += 1;
            }
        }
    }
    Some((key, start + idx))
}

/// Whether `segment` contains shell command substitution (`$(...)`, backticks), process
/// substitution (`<(...)`, `>(...)`), or unquoted subshell parentheses.
#[must_use]
pub fn has_unquoted_subshell(segment: &str) -> bool {
    let chars: Vec<char> = segment.chars().collect();
    let mut quote = QuoteState::None;
    let mut i = 0usize;
    while let Some(&ch) = chars.get(i) {
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
                    continue;
                }
                if ch == '`' || (ch == '$' && chars.get(i + 1) == Some(&'(')) {
                    return true;
                }
                i += 1;
            }
            QuoteState::None => {
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                if ch == '$' && chars.get(i + 1) == Some(&'\'') {
                    quote = QuoteState::AnsiSingle;
                    i += 2;
                    continue;
                }
                if ch == '\'' {
                    quote = QuoteState::Single;
                } else if ch == '"' {
                    quote = QuoteState::Double;
                } else if matches!(ch, '`' | '(' | ')')
                    || (ch == '{'
                        && chars.get(i + 1).is_some_and(char::is_ascii_whitespace)
                        && !i
                            .checked_sub(1)
                            .and_then(|j| chars.get(j))
                            .is_some_and(|prev| prev.is_ascii_alphanumeric() || *prev == '$'))
                {
                    return true;
                }
                i += 1;
            }
        }
    }
    false
}

/// Whether `segment` contains an unquoted output redirection (`>`, `>>`, `>|`, `&>`, `2>`, ...).
#[must_use]
pub fn has_unquoted_output_redirection(segment: &str) -> bool {
    let chars: Vec<char> = segment.chars().collect();
    let mut quote = QuoteState::None;
    let mut i = 0usize;
    while let Some(&ch) = chars.get(i) {
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
                if ch == '$' && chars.get(i + 1) == Some(&'\'') {
                    quote = QuoteState::AnsiSingle;
                    i += 2;
                    continue;
                }
                if ch == '\'' {
                    quote = QuoteState::Single;
                } else if ch == '"' {
                    quote = QuoteState::Double;
                } else if ch == '>' {
                    return true;
                }
                i += 1;
            }
        }
    }
    false
}
