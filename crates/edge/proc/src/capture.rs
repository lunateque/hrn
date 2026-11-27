//! Bounded output capture (head + tail) and an incremental UTF-8 decoder for pipe chunks.

use std::fmt::Write as _;

/// Decodes a byte stream into `String` chunks without ever splitting a multi-byte scalar that
/// straddles two `read` calls; genuinely invalid bytes become U+FFFD.
#[derive(Default)]
pub(crate) struct Utf8Chunker {
    pending: Vec<u8>,
}

impl Utf8Chunker {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        let mut out = String::with_capacity(buf.len());
        let mut pos = 0usize;
        while let Some(rest) = buf.get(pos..).filter(|r| !r.is_empty()) {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    out.push_str(valid);
                    break;
                }
                Err(err) => {
                    let good = err.valid_up_to();
                    out.push_str(&String::from_utf8_lossy(
                        rest.get(..good).unwrap_or_default(),
                    ));
                    let Some(bad) = err.error_len() else {
                        // Incomplete scalar at the very end: wait for the next chunk.
                        self.pending = rest.get(good..).unwrap_or_default().to_vec();
                        break;
                    };
                    out.push('\u{FFFD}');
                    pos += good + bad;
                }
            }
        }
        out
    }

    /// Flush a dangling partial scalar at EOF.
    pub(crate) fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            String::new()
        } else {
            let tail = std::mem::take(&mut self.pending);
            String::from_utf8_lossy(&tail).into_owned()
        }
    }
}

/// Keeps the first `head_limit` and the last `tail_limit` bytes of a stream; the middle is
/// dropped and counted. Build/test logs put the actionable error at the end, so a head-only cut
/// would hide exactly the part that matters.
pub(crate) struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    head_limit: usize,
    tail_limit: usize,
    omitted: usize,
}

impl Capture {
    pub(crate) fn new(head_limit: usize, tail_limit: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: Vec::new(),
            head_limit,
            tail_limit,
            omitted: 0,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        let room = self.head_limit.saturating_sub(self.head.len());
        let (to_head, rest) = bytes.split_at(room.min(bytes.len()));
        self.head.extend_from_slice(to_head);
        if rest.is_empty() {
            return;
        }
        self.tail.extend_from_slice(rest);
        if self.tail.len() > self.tail_limit {
            let excess = self.tail.len() - self.tail_limit;
            self.tail.drain(..excess);
            self.omitted += excess;
        }
    }

    pub(crate) fn truncated(&self) -> bool {
        self.omitted > 0
    }

    pub(crate) fn render(&self) -> String {
        let mut out = String::from_utf8_lossy(&self.head).into_owned();
        if self.omitted > 0 {
            // Writing to a `String` cannot fail.
            write!(out, "\n[... {} bytes omitted ...]\n", self.omitted).ok();
        }
        out.push_str(&String::from_utf8_lossy(&self.tail));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunker_reassembles_scalars_split_at_every_offset() {
        let text = "a€😀é\u{FFFD}z";
        let bytes = text.as_bytes();
        for cut in 0..=bytes.len() {
            let (a, b) = bytes.split_at(cut);
            let mut c = Utf8Chunker::default();
            let mut got = c.push(a);
            got.push_str(&c.push(b));
            got.push_str(&c.finish());
            assert_eq!(got, text, "cut at {cut}");
        }
    }

    #[test]
    fn chunker_replaces_invalid_bytes_and_keeps_going() {
        let mut c = Utf8Chunker::default();
        let mut got = c.push(b"ok\xFF\xFEok");
        got.push_str(&c.finish());
        assert_eq!(got, "ok\u{FFFD}\u{FFFD}ok");
        let mut c = Utf8Chunker::default();
        let mut got = c.push(b"x\xE2\x82");
        got.push_str(&c.finish());
        assert_eq!(got, "x\u{FFFD}");
    }

    #[test]
    fn capture_keeps_head_and_tail_and_counts_the_middle() {
        let mut cap = Capture::new(4, 4);
        for chunk in [&b"0123"[..], b"4567", b"89ab", b"cdef"] {
            cap.push(chunk);
        }
        assert!(cap.truncated());
        assert_eq!(cap.render(), "0123\n[... 8 bytes omitted ...]\ncdef");
        let mut small = Capture::new(8, 8);
        small.push(b"abc");
        assert!(!small.truncated());
        assert_eq!(small.render(), "abc");
    }
}
