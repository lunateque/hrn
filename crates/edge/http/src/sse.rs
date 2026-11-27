//! Incremental Server-Sent Events (SSE) frame parser (WHATWG `text/event-stream` rules).
//!
//! The parser buffers raw bytes and only decodes complete lines, so a multi-byte UTF-8 scalar
//! split across two network chunks is never corrupted. Lines end with `\n`, `\r\n` or a lone `\r`
//! (a trailing `\r` is held back until the next byte shows whether it is half of a `\r\n`).

/// One decoded SSE event frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    /// Optional `event:` field.
    pub event: Option<String>,
    /// Joined `data:` lines.
    pub data: String,
}

/// Stateful incremental byte-to-SSE parser.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    started: bool,
    event: Option<String>,
    data_lines: Vec<String>,
}

const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

impl SseParser {
    /// Push raw chunk bytes and drain all complete events.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        if !self.started {
            if self.buf.len() < BOM.len() && BOM.starts_with(&self.buf) {
                return Vec::new();
            }
            self.started = true;
            if self.buf.starts_with(BOM) {
                self.buf.drain(..BOM.len());
            }
        }
        let mut out = Vec::new();
        let mut consumed = 0usize;
        while let Some(rest) = self.buf.get(consumed..) {
            let Some(i) = rest.iter().position(|b| matches!(b, b'\n' | b'\r')) else {
                break;
            };
            let terminator_len = match (rest.get(i), rest.get(i + 1)) {
                (Some(b'\r'), Some(b'\n')) => 2,
                // A lone trailing CR may be the first half of CRLF: wait for more bytes.
                (Some(b'\r'), None) => break,
                _ => 1,
            };
            let line = String::from_utf8_lossy(rest.get(..i).unwrap_or_default()).into_owned();
            consumed += i + terminator_len;
            self.consume_line(&line, &mut out);
        }
        self.buf.drain(..consumed);
        out
    }

    /// Flush trailing buffered data at stream EOF (an unterminated last line and any event still
    /// pending without its blank-line terminator). Callers must treat unparsable payloads here as
    /// a truncated stream.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let tail = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&tail);
            self.consume_line(line.trim_end_matches('\r'), &mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.data_lines.is_empty() {
            self.event = None;
        } else {
            out.push(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data_lines).join("\n"),
            });
        }
    }

    fn consume_line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data_lines.push(value.to_owned()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut p = SseParser::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.push(c));
        }
        out.extend(p.finish());
        out
    }

    fn ev(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: event.map(str::to_owned),
            data: data.to_owned(),
        }
    }

    #[test]
    fn parses_chunked_crlf_and_multiline_events() {
        let mut p = SseParser::default();
        let a = p.push(b"event: message_start\r\ndata: {\"a\":");
        assert!(a.is_empty());
        let b = p.push(b"1}\r\n\r\ndata: line1\ndata: line2\n\n");
        assert_eq!(b.len(), 2);
        assert_eq!(b.first(), Some(&ev(Some("message_start"), "{\"a\":1}")));
        assert_eq!(b.get(1), Some(&ev(None, "line1\nline2")));
    }

    #[test]
    fn multibyte_char_split_across_chunks_is_preserved() {
        let full = "data: привет 😀\n\n".as_bytes();
        // Split inside the 4-byte emoji and inside a 2-byte Cyrillic letter.
        for cut in 1..full.len() {
            let (a, b) = full.split_at(cut);
            assert_eq!(
                collect(&[a, b]),
                vec![ev(None, "привет 😀")],
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn crlf_split_between_chunks_does_not_create_blank_line() {
        // "\r" | "\n" split must not be read as CR (line end) + LF (blank line = dispatch).
        let got = collect(&[b"data: a\r", b"\ndata: b\r\n\r\n"]);
        assert_eq!(got, vec![ev(None, "a\nb")]);
    }

    #[test]
    fn lone_cr_terminates_lines() {
        assert_eq!(
            collect(&[b"data: x\r\rdata: y\r\r"]),
            vec![ev(None, "x"), ev(None, "y")]
        );
    }

    #[test]
    fn bom_comments_and_fields_without_space_or_colon() {
        let got = collect(&[
            b"\xEF\xBB",
            b"\xBF: keep-alive\nevent:ping\ndata:{\"k\":1}\ndata\n\n",
        ]);
        assert_eq!(got, vec![ev(Some("ping"), "{\"k\":1}\n")]);
    }

    #[test]
    fn only_one_leading_space_is_stripped_from_values() {
        assert_eq!(
            collect(&[b"event:  two\ndata:  indented\n\n"]),
            vec![ev(Some(" two"), " indented")]
        );
    }

    #[test]
    fn blank_event_without_data_is_not_dispatched_and_resets_event_name() {
        let got = collect(&[b"event: orphan\n\ndata: z\n\n"]);
        assert_eq!(got, vec![ev(None, "z")]);
    }

    #[test]
    fn finish_flushes_unterminated_final_event() {
        assert_eq!(collect(&[b"data: [DONE]"]), vec![ev(None, "[DONE]")]);
    }
}
