//! Shell word tokenization, wrapper skipping, command-word normalization, and glob matching.

use super::{QuoteState, collapse_whitespace, skip_ws};

/// Quote-aware shell word splitter supporting `'...'`, `$'...'`, `"..."`, and `\` escapes.
#[must_use]
pub fn shell_words(segment: &str) -> Vec<String> {
    let chars: Vec<char> = segment.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let mut quote = QuoteState::None;
    let mut i = 0usize;

    while let Some(&ch) = chars.get(i) {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                } else {
                    cur.push(ch);
                }
                has_token = true;
                i += 1;
            }
            QuoteState::AnsiSingle => {
                if ch == '\\' {
                    i = decode_ansi_escape(&chars, i, &mut cur);
                } else if ch == '\'' {
                    quote = QuoteState::None;
                    i += 1;
                } else {
                    cur.push(ch);
                    i += 1;
                }
                has_token = true;
            }
            QuoteState::Double => {
                if ch == '\\'
                    && let Some(&esc) = chars.get(i + 1)
                    && matches!(esc, '"' | '\\' | '$' | '`' | '\n')
                {
                    cur.push(esc);
                    i += 2;
                } else if ch == '"' {
                    quote = QuoteState::None;
                    i += 1;
                } else {
                    cur.push(ch);
                    i += 1;
                }
                has_token = true;
            }
            QuoteState::None => {
                if ch == '\\' {
                    if has_windows_drive_prefix(&cur) {
                        cur.push('\\');
                        has_token = true;
                        i += 1;
                    } else if let Some(&esc) = chars.get(i + 1) {
                        cur.push(esc);
                        has_token = true;
                        i += 2;
                    } else {
                        cur.push('\\');
                        has_token = true;
                        i += 1;
                    }
                } else if ch == '$' && chars.get(i + 1) == Some(&'\'') {
                    quote = QuoteState::AnsiSingle;
                    has_token = true;
                    i += 2;
                } else if ch == '\'' {
                    quote = QuoteState::Single;
                    has_token = true;
                    i += 1;
                } else if ch == '"' {
                    quote = QuoteState::Double;
                    has_token = true;
                    i += 1;
                } else if ch.is_ascii_whitespace() {
                    if has_token {
                        out.push(std::mem::take(&mut cur));
                        has_token = false;
                    }
                    i += 1;
                } else {
                    cur.push(ch);
                    has_token = true;
                    i += 1;
                }
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
}

fn has_windows_drive_prefix(cur: &str) -> bool {
    let b = cur.as_bytes();
    b.len() >= 2 && b.first().is_some_and(u8::is_ascii_alphabetic) && b.get(1) == Some(&b':')
}

/// Decode one ANSI-C (`$'...'`) backslash escape starting at `chars[i] == '\\'`.
fn decode_ansi_escape(chars: &[char], i: usize, cur: &mut String) -> usize {
    let Some(&esc) = chars.get(i + 1) else {
        cur.push('\\');
        return i + 1;
    };
    match esc {
        'a' => {
            cur.push('\x07');
            i + 2
        }
        'b' => {
            cur.push('\x08');
            i + 2
        }
        'e' | 'E' => {
            cur.push('\x1b');
            i + 2
        }
        'f' => {
            cur.push('\x0c');
            i + 2
        }
        'n' => {
            cur.push('\n');
            i + 2
        }
        'r' => {
            cur.push('\r');
            i + 2
        }
        't' => {
            cur.push('\t');
            i + 2
        }
        'v' => {
            cur.push('\x0b');
            i + 2
        }
        '\\' | '\'' | '"' | '?' => {
            cur.push(esc);
            i + 2
        }
        'x' => decode_hex_escape(chars, i, 2, true, cur),
        'u' => decode_hex_escape(chars, i, 4, false, cur),
        'U' => decode_hex_escape(chars, i, 8, false, cur),
        '0'..='7' => decode_octal_escape(chars, i + 1, cur),
        'c' => {
            if let Some(&ctrl) = chars.get(i + 2)
                && let Ok(byte) = u8::try_from(u32::from(ctrl) & 0x1f)
            {
                cur.push(char::from(byte));
                i + 3
            } else {
                cur.push('\\');
                cur.push('c');
                i + 2
            }
        }
        other => {
            cur.push('\\');
            cur.push(other);
            i + 2
        }
    }
}

fn decode_hex_escape(
    chars: &[char],
    slash_idx: usize,
    max_digits: usize,
    as_byte: bool,
    cur: &mut String,
) -> usize {
    let start = slash_idx + 2;
    let mut val = 0u32;
    let mut j = start;
    let limit = start + max_digits;
    while j < limit
        && let Some(&c) = chars.get(j)
        && let Some(d) = c.to_digit(16)
    {
        val = (val << 4) | d;
        j += 1;
    }
    if j == start {
        cur.push('\\');
        if let Some(&esc) = chars.get(slash_idx + 1) {
            cur.push(esc);
        }
        return start;
    }
    if as_byte {
        if let Ok(byte) = u8::try_from(val & 0xff) {
            cur.push(char::from(byte));
        }
    } else if let Some(ch) = char::from_u32(val) {
        cur.push(ch);
    }
    j
}

fn decode_octal_escape(chars: &[char], start: usize, cur: &mut String) -> usize {
    let mut val = 0u32;
    let mut j = start;
    let limit = start + 3;
    while j < limit
        && let Some(&c) = chars.get(j)
        && let Some(d) = c.to_digit(8)
    {
        val = (val << 3) | d;
        j += 1;
    }
    if let Ok(byte) = u8::try_from(val & 0xff) {
        cur.push(char::from(byte));
    }
    j
}

/// Strip standard Unix binary directory prefixes (`/bin/`, `/usr/bin/`, `/sbin/`, `/usr/sbin/`,
/// `/usr/local/bin/`) from a command word.
#[must_use]
pub fn strip_bin_prefix(word: &str) -> &str {
    for prefix in [
        "/usr/local/bin/",
        "/usr/local/sbin/",
        "/usr/bin/",
        "/usr/sbin/",
        "/bin/",
        "/sbin/",
    ] {
        if let Some(rest) = word.strip_prefix(prefix)
            && !rest.is_empty()
        {
            return rest;
        }
    }
    word
}

/// Normalize the leading command word of `bare` by unquoting simple shell quotes/escapes
/// (`'rm'`, `"rm"`, `r\m`, `""rm`) and stripping standard `/bin/` / `/usr/bin/` prefixes.
#[must_use]
pub fn normalize_command_word(bare: &str) -> String {
    let words = shell_words(bare);
    let Some(first) = words.first() else {
        return bare.to_owned();
    };
    let head = strip_bin_prefix(first);
    let rest = skip_first_shell_word(bare);
    if rest.is_empty() {
        head.to_owned()
    } else {
        format!("{head} {rest}")
    }
}

/// Fully normalize a shell segment by unquoting/unescaping ALL shell words via [`shell_words`]
/// and stripping standard `/bin/` / `/usr/bin/` prefixes from word 0 (so `rm '-rf' '/'`,
/// `git 'push' origin main`, `tee '/etc/passwd'`, and `del '/s' /q C:\` normalize cleanly).
#[must_use]
pub fn normalize_segment_words(bare: &str) -> String {
    let words = shell_words(bare);
    join_normalized_words(&words)
}

/// Strip flag-bearing wrapper commands (`sudo`, `env`, `command`, `builtin`, `nohup`, `nice`,
/// `time`, and `exec`) from `bare` and return the fully normalized inner command, or `None` when
/// no wrapper is present.
#[must_use]
pub fn unwrap_segment_command(bare: &str) -> Option<String> {
    let words = shell_words(bare);
    let mut idx = skip_command_wrappers(&words);
    while words
        .get(idx)
        .is_some_and(|w| strip_bin_prefix(w) == "exec")
    {
        idx = skip_flag_args(&words, idx + 1, &["-a"], false);
        idx += skip_command_wrappers(words.get(idx..).unwrap_or_default());
    }
    if idx == 0 || idx >= words.len() {
        return None;
    }
    let slice = words.get(idx..)?;
    Some(join_normalized_words(slice))
}

fn join_normalized_words(words: &[String]) -> String {
    let Some(first) = words.first() else {
        return String::new();
    };
    let mut out = strip_bin_prefix(first).to_owned();
    for word in words.iter().skip(1) {
        out.push(' ');
        out.push_str(word);
    }
    out
}

fn skip_first_shell_word(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut i = skip_ws(bytes, 0);
    let mut quote = QuoteState::None;
    while let Some(&b) = bytes.get(i) {
        match quote {
            QuoteState::Single => {
                i += 1;
                if b == b'\'' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::AnsiSingle | QuoteState::Double => {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                i += 1;
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
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == b'$' && bytes.get(i + 1) == Some(&b'\'') {
                    quote = QuoteState::AnsiSingle;
                    i += 2;
                    continue;
                }
                if b == b'\'' {
                    quote = QuoteState::Single;
                } else if b == b'"' {
                    quote = QuoteState::Double;
                }
                i += 1;
            }
        }
    }
    text.get(i..).unwrap_or("").trim_start()
}

/// Advance past wrapper commands (`command`, `builtin`, `env`, `sudo`, `nohup`, `nice`, `time`)
/// and their flags in a tokenized shell word slice.
#[must_use]
pub fn skip_command_wrappers(words: &[String]) -> usize {
    let mut idx = 0usize;
    while let Some(raw) = words.get(idx).map(String::as_str) {
        match strip_bin_prefix(raw) {
            "builtin" | "nohup" => {
                idx += 1;
                if words.get(idx).map(String::as_str) == Some("--") {
                    idx += 1;
                }
            }
            "command" | "time" => idx = skip_flag_args(words, idx + 1, &[], false),
            "nice" => idx = skip_flag_args(words, idx + 1, &["-n"], false),
            "env" => {
                idx = skip_flag_args(
                    words,
                    idx + 1,
                    &["-u", "-C", "-S", "--unset", "--chdir"],
                    true,
                );
            }
            "sudo" => {
                idx = skip_flag_args(
                    words,
                    idx + 1,
                    &["-u", "-g", "-C", "-h", "-p", "-T", "-r", "-t"],
                    true,
                );
            }
            _ => break,
        }
    }
    idx
}

fn skip_flag_args(words: &[String], mut idx: usize, value_flags: &[&str], skip_env: bool) -> usize {
    while let Some(tok) = words.get(idx).map(String::as_str) {
        if tok == "--" {
            return idx + 1;
        }
        if value_flags.contains(&tok) {
            idx += 2;
        } else if (tok.starts_with('-') && tok.len() > 1) || (skip_env && is_env_word(tok)) {
            idx += 1;
        } else {
            break;
        }
    }
    idx
}

fn is_env_word(tok: &str) -> bool {
    let Some((key, _)) = tok.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Whether `bare_command` invokes a shell state-mutating builtin excluded from compound `allow`
/// chains (`cd`, `source`, `.`, `eval`, `exec`), including quoted/escaped or wrapped invocations.
#[must_use]
pub fn is_excluded_compound_builtin(bare_command: &str) -> bool {
    let words = shell_words(bare_command);
    let idx = skip_command_wrappers(&words);
    let Some(first) = words.get(idx) else {
        return false;
    };
    matches!(
        strip_bin_prefix(first),
        "cd" | "source" | "." | "eval" | "exec"
    )
}

/// Glob match (`*` = any characters, `?` = single character) on whitespace-collapsed command text.
#[must_use]
pub fn pattern_matches(pattern: &str, text: &str) -> bool {
    let pat_norm = collapse_whitespace(pattern);
    let txt_norm = collapse_whitespace(text);
    if pat_norm.is_empty() {
        return false;
    }
    let p: Vec<char> = pat_norm.chars().collect();
    let t: Vec<char> = txt_norm.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);

    while ti < t.len() {
        let pc = p.get(pi).copied();
        let tc = t.get(ti).copied();
        if pc == Some('*') {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
        } else if pc == Some('?') || (pc.is_some() && pc == tc) {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}
