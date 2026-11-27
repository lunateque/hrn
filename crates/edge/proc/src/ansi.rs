//! Terminal output as plain text: what a program prints to a pseudo-terminal, with the escape
//! sequences, bells and carriage returns of a screen taken out, for a model that reads a log.
//!
//! There is no screen model. A cursor jump to another line (`ConPTY` repaints rows with absolute
//! positions instead of line feeds) counts as a line break, a bare carriage return (a progress
//! bar) starts a new line, and everything else that moves the cursor or paints is dropped.
//!
//! A terminal also answers queries. `ConPTY` asks for the cursor position (`ESC [ 6 n`) and the
//! device attributes (`ESC [ c`) when it starts, and older hosts (Windows Server 2022) render
//! nothing until they get an answer. The stripper collects the replies a terminal owes
//! ([`Stripper::take_replies`]); the caller writes them back to the terminal's input.

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// After `ESC`.
    Escape,
    /// After `ESC` and an intermediate byte (`ESC ( B`): one final byte is left.
    Intermediate,
    /// Inside `ESC [ … <final>`.
    Csi,
    /// Inside a string (`OSC`, `DCS`, `APC`, `PM`, `SOS`) that ends at `BEL` or `ESC \`.
    Str,
    /// `ESC` seen inside a string: `\` ends it.
    StrEscape,
}

/// Longest CSI parameter string a recognised query has (`6` of `ESC [ 6 n`).
const QUERY_PARAMS: usize = 2;
/// Reply to a cursor position request: the top-left cell. There is no screen model, and the one
/// query that must be answered, `ConPTY`'s at startup, comes from a screen that is still empty.
const CURSOR_POSITION_REPLY: &[u8] = b"\x1b[1;1R";
/// Reply to a primary device attributes request: a VT100 with advanced video.
const DEVICE_ATTRIBUTES_REPLY: &[u8] = b"\x1b[?1;2c";

/// Incremental stripper; carries its state across chunk boundaries.
pub(crate) struct Stripper {
    state: State,
    /// A `\r` is waiting: `\r\n` is one line break, a lone `\r` is one too.
    pending_cr: bool,
    /// The last byte written was a line break (or nothing was written yet).
    at_line_start: bool,
    /// Parameter bytes of the current CSI sequence (only the first [`QUERY_PARAMS`] are kept).
    params: [u8; QUERY_PARAMS],
    /// How many parameter bytes the current CSI sequence has, kept or not.
    params_len: usize,
    /// Answers owed for the queries seen so far.
    replies: Vec<u8>,
}

impl Default for Stripper {
    fn default() -> Self {
        Self {
            state: State::Ground,
            pending_cr: false,
            at_line_start: true,
            params: [0; QUERY_PARAMS],
            params_len: 0,
            replies: Vec::new(),
        }
    }
}

/// Final bytes of CSI sequences that put the cursor on another line: `A`..`F` (up/down/next/
/// previous line), `H`/`f` (absolute position) and `d` (absolute row).
const fn moves_lines(final_byte: u8) -> bool {
    matches!(final_byte, b'A'..=b'F' | b'H' | b'f' | b'd')
}

impl Stripper {
    /// Append the plain text of `input` to `out`.
    pub(crate) fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            self.byte(b, out);
        }
    }

    /// The replies owed to the program for the queries fed so far, oldest first; each is
    /// returned once.
    pub(crate) fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// Owe a reply when the CSI sequence ending in `final_byte` is a query a terminal answers.
    fn answer(&mut self, final_byte: u8) {
        let params = self.params.get(..self.params_len);
        let reply = match (final_byte, params) {
            (b'n', Some(b"6")) => CURSOR_POSITION_REPLY,
            (b'c', Some(b"" | b"0")) => DEVICE_ATTRIBUTES_REPLY,
            _ => return,
        };
        self.replies.extend_from_slice(reply);
    }

    fn newline(&mut self, out: &mut Vec<u8>) {
        out.push(b'\n');
        self.at_line_start = true;
    }

    /// A line break only when the current line has text, so repaints do not stack blank lines.
    fn soft_newline(&mut self, out: &mut Vec<u8>) {
        if !self.at_line_start {
            self.newline(out);
        }
    }

    fn byte(&mut self, b: u8, out: &mut Vec<u8>) {
        match self.state {
            State::Ground => self.ground(b, out),
            State::Escape => match b {
                b'[' => {
                    self.params_len = 0;
                    self.state = State::Csi;
                }
                b']' | b'P' | b'X' | b'^' | b'_' => self.state = State::Str,
                0x20..=0x2F => self.state = State::Intermediate,
                _ => self.state = State::Ground,
            },
            State::Intermediate => {
                if !(0x20..=0x2F).contains(&b) {
                    self.state = State::Ground;
                }
            }
            State::Csi => match b {
                0x40..=0x7E => {
                    self.answer(b);
                    if moves_lines(b) {
                        self.soft_newline(out);
                    }
                    self.state = State::Ground;
                }
                0x1B => self.state = State::Escape,
                _ => {
                    if let Some(slot) = self.params.get_mut(self.params_len) {
                        *slot = b;
                    }
                    self.params_len = self.params_len.saturating_add(1);
                }
            },
            State::Str => match b {
                0x07 => self.state = State::Ground,
                0x1B => self.state = State::StrEscape,
                _ => {}
            },
            State::StrEscape => {
                self.state = if b == b'\\' {
                    State::Ground
                } else {
                    State::Str
                };
            }
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<u8>) {
        if self.pending_cr {
            self.pending_cr = false;
            self.newline(out);
            if b == b'\n' {
                return;
            }
        }
        match b {
            0x1B => self.state = State::Escape,
            b'\r' => self.pending_cr = true,
            b'\n' => self.newline(out),
            b'\t' => {
                out.push(b);
                self.at_line_start = false;
            }
            0x00..=0x1F | 0x7F => {}
            _ => {
                out.push(b);
                self.at_line_start = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(chunks: &[&[u8]]) -> String {
        let mut s = Stripper::default();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        String::from_utf8(out).expect("utf8")
    }

    #[test]
    fn colours_titles_and_modes_vanish_and_text_stays() {
        let raw = b"\x1b[?9001h\x1b]0;C:\\bash.exe\x07\x1b[32mgreen\x1b[0m ok\r\n>>> ";
        assert_eq!(strip(&[raw]), "green ok\n>>> ");
    }

    #[test]
    fn sequences_split_across_reads_are_still_removed() {
        assert_eq!(
            strip(&[b"a\x1b", b"[3", b"1mb\x1b]0;ti", b"tle\x1b", b"\\c"]),
            "abc"
        );
    }

    #[test]
    fn crlf_is_one_break_and_a_bare_cr_a_second_line() {
        assert_eq!(
            strip(&[b"one\r", b"\ntwo\r50%\r100%\r\n"]),
            "one\ntwo\n50%\n100%\n"
        );
    }

    #[test]
    fn a_cursor_jump_to_another_row_separates_lines() {
        assert_eq!(
            strip(&[b"first\x1b[3;1Hsecond\x1b[4;1H"]),
            "first\nsecond\n"
        );
    }

    #[test]
    fn utf8_passes_through() {
        assert_eq!(strip(&["héllo ✓".as_bytes()]), "héllo ✓");
    }

    #[test]
    fn terminal_queries_are_answered_once_even_when_split() {
        let mut s = Stripper::default();
        let mut out = Vec::new();
        s.feed(b"\x1b[1t\x1b[", &mut out);
        s.feed(b"6n\x1b[c\x1b[?1004hREADY", &mut out);
        assert_eq!(out, b"READY");
        assert_eq!(s.take_replies(), b"\x1b[1;1R\x1b[?1;2c");
        assert!(s.take_replies().is_empty());
        // Not queries: a colour, DECXCPR (`?6n`), a status report (`5n`), a secondary DA (`>c`).
        s.feed(b"\x1b[16n\x1b[?6n\x1b[5n\x1b[>c\x1b[0c", &mut out);
        assert_eq!(s.take_replies(), b"\x1b[?1;2c");
    }
}
