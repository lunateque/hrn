//! Terminal text primitives: sanitising untrusted text, display widths, truncation and word
//! wrapping of styled segments.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::borrow::Cow;
use std::iter::Peekable;
use std::str::Chars;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A run of text in one style.
pub(crate) type Seg = (String, Style);

pub(crate) fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

pub(crate) fn str_width(s: &str) -> usize {
    s.width()
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Make external text safe to paint: escape sequences, control and bidi-override characters are
/// dropped, tabs become four spaces, `\r\n` becomes `\n`, and a lone `\r` rewinds the line the way
/// a terminal would (progress bars keep only their last state).
pub(crate) fn sanitize(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|c| (c.is_control() && c != '\n') || is_bidi_control(c))
    {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => out.truncate(out.rfind('\n').map_or(0, |i| i + 1)),
            '\u{1b}' => skip_escape(&mut chars),
            c if c.is_control() || is_bidi_control(c) => {}
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

fn skip_escape(chars: &mut Peekable<Chars<'_>>) {
    match chars.next() {
        Some('[') => {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        Some(']' | 'P' | '_' | '^') => {
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    break;
                }
            }
        }
        _ => {}
    }
}

/// Cut `text` to at most `max` columns, ending with `…` when something was dropped.
pub(crate) fn truncate(text: &str, max: usize) -> Cow<'_, str> {
    if str_width(text) <= max {
        return Cow::Borrowed(text);
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = char_width(c);
        if used + w + 1 > max {
            break;
        }
        used += w;
        out.push(c);
    }
    if max > 0 {
        out.push('…');
    }
    Cow::Owned(out)
}

/// Append spaces in `style` until `line` is `width` columns wide.
pub(crate) fn pad(line: &mut Line<'static>, width: usize, style: Style) {
    let used = line.width();
    if used < width {
        line.spans
            .push(Span::styled(" ".repeat(width - used), style));
    }
}

/// Word-wrap styled segments to `width` columns. The first output line starts with `first`, the
/// others with `rest`; prefix widths count against `width`. `\n` forces a break, words longer than
/// a line are split, and leading spaces (indentation) survive. Never returns an empty vector.
pub(crate) fn wrap(
    segs: &[Seg],
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
) -> Vec<Line<'static>> {
    let first_width: usize = first.iter().map(Span::width).sum();
    let rest_width: usize = rest.iter().map(Span::width).sum();
    let mut w = Wrapper {
        width: width.max(first_width.max(rest_width) + 1),
        rest,
        rest_width,
        lines: Vec::new(),
        prefix: first.to_vec(),
        line: Vec::new(),
        used: first_width,
        word: Vec::new(),
        word_width: 0,
        space: Vec::new(),
        space_width: 0,
    };
    for (text, style) in segs {
        for c in text.chars() {
            w.push(c, *style);
        }
    }
    w.finish()
}

/// Plain-text convenience over [`wrap`].
pub(crate) fn wrap_text(
    text: &str,
    style: Style,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
) -> Vec<Line<'static>> {
    wrap(&[(text.to_owned(), style)], width, first, rest)
}

struct Wrapper<'p> {
    width: usize,
    rest: &'p [Span<'static>],
    rest_width: usize,
    lines: Vec<Line<'static>>,
    prefix: Vec<Span<'static>>,
    line: Vec<Seg>,
    used: usize,
    word: Vec<Seg>,
    word_width: usize,
    space: Vec<Seg>,
    space_width: usize,
}

fn push_merged(target: &mut Vec<Seg>, c: char, style: Style) {
    match target.last_mut() {
        Some((text, s)) if *s == style => text.push(c),
        _ => target.push((c.to_string(), style)),
    }
}

impl Wrapper<'_> {
    fn push(&mut self, c: char, style: Style) {
        match c {
            '\n' => {
                self.flush_word();
                self.space.clear();
                self.space_width = 0;
                self.end_line();
            }
            ' ' => {
                self.flush_word();
                push_merged(&mut self.space, c, style);
                self.space_width += 1;
            }
            c => {
                push_merged(&mut self.word, c, style);
                self.word_width += char_width(c);
            }
        }
    }

    fn has_content(&self) -> bool {
        !self.line.is_empty()
    }

    fn flush_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let space = std::mem::take(&mut self.space);
        let word = std::mem::take(&mut self.word);
        let (space_width, word_width) = (self.space_width, self.word_width);
        self.space_width = 0;
        self.word_width = 0;
        if self.used + space_width + word_width <= self.width {
            self.emit(space, space_width);
            self.emit(word, word_width);
        } else if self.has_content() && self.rest_width + word_width <= self.width {
            self.end_line();
            self.emit(word, word_width);
        } else if !self.has_content() && self.used + word_width <= self.width {
            self.emit(word, word_width);
        } else {
            if self.has_content() {
                if self.used + space_width < self.width {
                    self.emit(space, space_width);
                } else {
                    self.end_line();
                }
            }
            for (text, style) in word {
                for c in text.chars() {
                    let cw = char_width(c);
                    if self.used + cw > self.width && self.has_content() {
                        self.end_line();
                    }
                    push_merged(&mut self.line, c, style);
                    self.used += cw;
                }
            }
        }
    }

    fn emit(&mut self, segs: Vec<Seg>, width: usize) {
        for (text, style) in segs {
            match self.line.last_mut() {
                Some((t, s)) if *s == style => t.push_str(&text),
                _ => self.line.push((text, style)),
            }
        }
        self.used += width;
    }

    fn end_line(&mut self) {
        let mut spans = std::mem::replace(&mut self.prefix, self.rest.to_vec());
        spans.extend(self.line.drain(..).map(|(t, s)| Span::styled(t, s)));
        self.lines.push(Line::from(spans));
        self.used = self.rest_width;
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_word();
        if self.has_content() || self.lines.is_empty() {
            self.end_line();
        }
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn wraps_on_words_and_splits_overlong_ones() {
        let lines = wrap_text("the quick brown fox", Style::new(), 10, &[], &[]);
        assert_eq!(texts(&lines), ["the quick", "brown fox"]);
        let lines = wrap_text("abcdefghijkl", Style::new(), 5, &[], &[]);
        assert_eq!(texts(&lines), ["abcde", "fghij", "kl"]);
    }

    #[test]
    fn prefixes_count_against_the_width_and_indentation_survives() {
        let first = [Span::raw("- ")];
        let rest = [Span::raw("  ")];
        let lines = wrap_text("alpha beta gamma", Style::new(), 12, &first, &rest);
        assert_eq!(texts(&lines), ["- alpha beta", "  gamma"]);
        let lines = wrap_text("  indented\nnext", Style::new(), 20, &[], &[]);
        assert_eq!(texts(&lines), ["  indented", "next"]);
    }

    #[test]
    fn wide_characters_use_two_columns() {
        let lines = wrap_text("日本語のテキスト", Style::new(), 6, &[], &[]);
        assert_eq!(texts(&lines), ["日本語", "のテキ", "スト"]);
        assert_eq!(texts(&wrap_text("", Style::new(), 6, &[], &[])), [""]);
    }

    #[test]
    fn sanitize_strips_escapes_and_rewinds_carriage_returns() {
        assert_eq!(sanitize("\u{1b}[31mred\u{1b}[0m\tx"), "red    x");
        assert_eq!(sanitize("10%\r50%\r100%\ndone\r\n"), "100%\ndone\n");
        assert_eq!(sanitize("a\u{1b}]0;title\u{7}b\u{202e}c"), "abc");
        assert!(matches!(sanitize("clean\ntext"), Cow::Borrowed(_)));
    }

    #[test]
    fn truncate_marks_cut_text() {
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("short", 8), "short");
    }
}
