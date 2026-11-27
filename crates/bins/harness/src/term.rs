//! The only place that touches stdout/stderr (the workspace forbids `print!`).
//! Write failures (closed pipe) are deliberately ignored: there is nowhere left to report them.

use std::io::Write;

/// Write `text` to stdout and flush.
pub(crate) fn out(text: &str) {
    let mut handle = std::io::stdout().lock();
    let _ = handle.write_all(text.as_bytes());
    let _ = handle.flush();
}

/// Write `text` plus a newline to stderr.
pub(crate) fn err_line(text: &str) {
    let mut handle = std::io::stderr().lock();
    let _ = handle.write_all(text.as_bytes());
    let _ = handle.write_all(b"\n");
}

/// Collapse `text` to its first line, at most `max` characters, marking any cut with `…`.
pub(crate) fn short(text: &str, max: usize) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let cut = first.chars().count() > max || text.trim().lines().nth(1).is_some();
    let mut s: String = first.chars().take(max).collect();
    if cut {
        s.push('…');
    }
    s
}

/// Strip ANSI escape sequences (CSI/OSC/DCS/2-byte ESC), C0/C1 control characters, and Unicode
/// bidi override/isolate/mark characters for safe single-line CLI output.
pub(crate) fn sanitize_cli_inline(text: &str) -> String {
    sanitize_cli(text, false)
}

/// Strip ANSI escape sequences, C0/C1 control characters (preserving `\n`), and Unicode bidi
/// override/isolate/mark characters for safe multi-line CLI output (e.g. commit messages).
pub(crate) fn sanitize_cli_multiline(text: &str) -> String {
    sanitize_cli(text, true)
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

fn sanitize_cli(text: &str, preserve_newlines: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_esc_sequence(&mut chars),
            '\u{009B}' => skip_csi_body(&mut chars),
            '\u{009D}' => skip_osc_body(&mut chars),
            '\n' if preserve_newlines => out.push('\n'),
            c if c.is_control() || is_bidi_control(c) => {}
            other => out.push(other),
        }
    }
    out
}

fn skip_esc_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.peek().copied() {
        Some('[') => {
            let _ = chars.next();
            skip_csi_body(chars);
        }
        Some(']') => {
            let _ = chars.next();
            skip_osc_body(chars);
        }
        Some('P' | 'X' | '^' | '_') => {
            let _ = chars.next();
            skip_st_terminated(chars);
        }
        Some(ch) if (0x20..=0x7E).contains(&(ch as u32)) => {
            while let Some(&next) = chars.peek() {
                let _ = chars.next();
                if (0x30..=0x7E).contains(&(next as u32)) {
                    break;
                }
            }
        }
        _ => {}
    }
}

fn skip_csi_body(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for ch in chars.by_ref() {
        if (0x40..=0x7E).contains(&(ch as u32)) {
            break;
        }
    }
}

fn skip_osc_body(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(ch) = chars.next() {
        if ch == '\x07' || ch == '\u{009C}' {
            break;
        }
        if ch == '\x1b' && chars.peek() == Some(&'\\') {
            let _ = chars.next();
            break;
        }
    }
}

fn skip_st_terminated(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(ch) = chars.next() {
        if ch == '\u{009C}' {
            break;
        }
        if ch == '\x1b' && chars.peek() == Some(&'\\') {
            let _ = chars.next();
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_cli_inline, sanitize_cli_multiline, short};

    #[test]
    fn short_keeps_first_line_and_marks_cuts() {
        assert_eq!(short("abc", 10), "abc");
        assert_eq!(short("abcdef", 3), "abc…");
        assert_eq!(short("one\ntwo", 10), "one…");
        assert_eq!(short("one\n", 10), "one");
        assert_eq!(short("", 10), "");
    }

    #[test]
    fn sanitize_cli_strips_ansi_controls_and_bidi() {
        let hostile = "\x1b[31mfeat:\x1b[0m \x1b]0;pwn\x07safe\x1b]52;c;abc\x1b\\\u{009B}1m\r\n\
            body\x07\x7f\u{202E}exe\u{202C}\u{2066}iso\u{2069}\u{200E}\u{200F}\u{061C}";
        assert_eq!(sanitize_cli_inline(hostile), "feat: safebodyexeiso");
        assert_eq!(sanitize_cli_multiline(hostile), "feat: safe\nbodyexeiso");
    }
}
