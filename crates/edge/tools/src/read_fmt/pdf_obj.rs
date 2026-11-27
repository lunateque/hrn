//! PDF object syntax: a lexer/parser shared by file bodies, object streams, `CMap`s and page
//! content streams, plus the stream filters text extraction needs (Flate, `ASCIIHex`, `ASCII85`).

use flate2::read::ZlibDecoder;
use std::collections::HashMap;
use std::io::Read as _;
use std::ops::Range;

const MAX_DECODED: u64 = 128 * 1024 * 1024;

/// A PDF dictionary.
pub(super) type Dict = HashMap<String, Obj>;

/// One PDF object; content-stream operators and stray delimiters come back as `Op`.
pub(super) enum Obj {
    Null,
    Int(i64),
    Real(f64),
    Name(String),
    Str(Vec<u8>),
    Arr(Vec<Obj>),
    Dict(Dict),
    Ref(u32),
    /// Dictionary plus the raw (still filtered) data range in the source buffer.
    Stream(Dict, Range<usize>),
    Op(String),
}

impl Obj {
    /// Numeric value of an integer or real.
    pub(super) fn num(&self) -> Option<f64> {
        match self {
            Self::Int(i) => i32::try_from(*i).ok().map(f64::from),
            Self::Real(f) => Some(*f),
            _ => None,
        }
    }

    pub(super) fn int(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub(super) fn name(&self) -> Option<&str> {
        match self {
            Self::Name(n) => Some(n),
            _ => None,
        }
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}

fn is_delim(b: u8) -> bool {
    is_ws(b) || b"()<>[]{}/%".contains(&b)
}

/// Cursor over PDF syntax.
pub(super) struct Lex<'a> {
    b: &'a [u8],
    pub(super) pos: usize,
}

impl<'a> Lex<'a> {
    pub(super) fn new(b: &'a [u8], pos: usize) -> Self {
        Self { b, pos }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b'%' {
                while self.peek().is_some_and(|c| c != b'\n' && c != b'\r') {
                    self.pos += 1;
                }
            } else if is_ws(c) {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn token(&mut self) -> &'a [u8] {
        let start = self.pos;
        while self.peek().is_some_and(|c| !is_delim(c)) {
            self.pos += 1;
        }
        self.b.get(start..self.pos).unwrap_or_default()
    }

    /// The next object or operator; `None` at end of input.
    pub(super) fn value(&mut self) -> Option<Obj> {
        self.skip_ws();
        let c = self.peek()?;
        match c {
            b'/' => {
                self.pos += 1;
                Some(Obj::Name(name_text(self.token())))
            }
            b'(' => {
                self.pos += 1;
                Some(Obj::Str(self.literal()))
            }
            b'<' if self.b.get(self.pos + 1) == Some(&b'<') => {
                self.pos += 2;
                Some(Obj::Dict(self.dict()))
            }
            b'<' => {
                self.pos += 1;
                Some(Obj::Str(self.hex()))
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    match self.value()? {
                        Obj::Op(s) if s == "]" => return Some(Obj::Arr(items)),
                        v => items.push(v),
                    }
                }
            }
            b'>' if self.b.get(self.pos + 1) == Some(&b'>') => {
                self.pos += 2;
                Some(Obj::Op(">>".into()))
            }
            b']' | b'>' | b')' | b'{' | b'}' => {
                self.pos += 1;
                Some(Obj::Op(char::from(c).to_string()))
            }
            _ => Some(self.atom()),
        }
    }

    fn dict(&mut self) -> Dict {
        let mut d = Dict::new();
        while let Some(k) = self.value() {
            match k {
                Obj::Name(key) => {
                    if let Some(v) = self.value() {
                        d.insert(key, v);
                    }
                }
                Obj::Op(s) if s == ">>" => break,
                _ => {}
            }
        }
        d
    }

    fn atom(&mut self) -> Obj {
        let tok = self.token();
        if tok.is_empty() {
            self.pos += 1;
            return Obj::Null;
        }
        let text = String::from_utf8_lossy(tok);
        let starts_num = tok
            .first()
            .is_some_and(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.'));
        if starts_num {
            if let Ok(i) = text.parse::<i64>() {
                return self.maybe_ref(i);
            }
            if let Ok(f) = text.parse::<f64>() {
                return Obj::Real(f);
            }
        }
        match text.as_ref() {
            "null" => Obj::Null,
            _ => Obj::Op(text.into_owned()),
        }
    }

    /// `N G R` after the integer `n`.
    fn maybe_ref(&mut self, n: i64) -> Obj {
        let save = self.pos;
        self.skip_ws();
        let generation = self.token();
        if !generation.is_empty() && generation.iter().all(u8::is_ascii_digit) {
            self.skip_ws();
            if self.token() == b"R"
                && let Ok(id) = u32::try_from(n)
            {
                return Obj::Ref(id);
            }
        }
        self.pos = save;
        Obj::Int(n)
    }

    fn literal(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut depth = 1usize;
        while let Some(c) = self.peek() {
            self.pos += 1;
            match c {
                b'(' => {
                    depth += 1;
                    out.push(c);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(c);
                }
                b'\\' => self.escape(&mut out),
                _ => out.push(c),
            }
        }
        out
    }

    fn escape(&mut self, out: &mut Vec<u8>) {
        let Some(c) = self.peek() else { return };
        self.pos += 1;
        match c {
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'\r' => {
                if self.peek() == Some(b'\n') {
                    self.pos += 1;
                }
            }
            b'\n' => {}
            b'0'..=b'7' => {
                let mut code = u32::from(c - b'0');
                for _ in 0..2 {
                    match self.peek() {
                        Some(d @ b'0'..=b'7') => {
                            code = code * 8 + u32::from(d - b'0');
                            self.pos += 1;
                        }
                        _ => break,
                    }
                }
                out.push(u8::try_from(code & 0xFF).unwrap_or(0));
            }
            other => out.push(other),
        }
    }

    fn hex(&mut self) -> Vec<u8> {
        let mut digits = Vec::new();
        while let Some(c) = self.peek() {
            self.pos += 1;
            match c {
                b'>' => break,
                _ => digits.extend(
                    (char::from(c))
                        .to_digit(16)
                        .and_then(|d| u8::try_from(d).ok()),
                ),
            }
        }
        digits
            .chunks(2)
            .map(|p| {
                let hi = p.first().copied().unwrap_or(0);
                let lo = p.get(1).copied().unwrap_or(0);
                (hi << 4) | lo
            })
            .collect()
    }

    /// An indirect-object body: a value, or a dictionary followed by `stream ... endstream`.
    pub(super) fn object(&mut self) -> Option<Obj> {
        let v = self.value()?;
        let Obj::Dict(dict) = v else { return Some(v) };
        self.skip_ws();
        if !self.b.get(self.pos..)?.starts_with(b"stream") {
            return Some(Obj::Dict(dict));
        }
        self.pos += 6;
        if self.peek() == Some(b'\r') {
            self.pos += 1;
        }
        if self.peek() == Some(b'\n') {
            self.pos += 1;
        }
        let start = self.pos;
        // A direct /Length that lands on `endstream` is exact (binary data may end in an EOL
        // byte); otherwise search for the keyword and trim one EOL.
        let declared = dict
            .get("Length")
            .and_then(Obj::int)
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|n| {
                let tail = self.b.get(start.checked_add(n)?..)?;
                tail.trim_ascii_start()
                    .starts_with(b"endstream")
                    .then_some(start + n)
            });
        let end = if let Some(end) = declared {
            end
        } else {
            let rest = self.b.get(start..)?;
            let rel = rest.windows(9).position(|w| w == b"endstream")?;
            let mut end = start + rel;
            for eol in *b"\n\r" {
                if end > start && self.b.get(end - 1) == Some(&eol) {
                    end -= 1;
                }
            }
            end
        };
        let tail = self.b.get(end..)?;
        self.pos = end + tail.windows(9).position(|w| w == b"endstream")? + 9;
        Some(Obj::Stream(dict, start..end))
    }

    /// Skip an inline image's data after its `ID` operator.
    pub(super) fn skip_inline_image(&mut self) {
        let rest = self.b.get(self.pos..).unwrap_or_default();
        let found = rest.windows(4).position(|w| {
            w.first().is_some_and(|c| is_ws(*c))
                && w.get(1..3) == Some(b"EI")
                && w.get(3).is_none_or(|c| is_ws(*c))
        });
        self.pos = found.map_or(self.b.len(), |i| self.pos + i + 3);
    }
}

/// Decode `#xx` escapes of a name.
fn name_text(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while let Some(&c) = raw.get(i) {
        let hex = (c == b'#')
            .then(|| raw.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        if let Some(byte) = hex {
            out.push(byte);
            i += 3;
        } else {
            out.push(c);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn filters(dict: &Dict) -> Vec<String> {
    match dict.get("Filter").or_else(|| dict.get("F")) {
        Some(Obj::Name(n)) => vec![n.clone()],
        Some(Obj::Arr(a)) => a
            .iter()
            .filter_map(|o| o.name().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Apply the stream's filter chain to `raw`; `None` for filters not supported here (images,
/// LZW, encryption...).
pub(super) fn decode(dict: &Dict, raw: &[u8]) -> Option<Vec<u8>> {
    let mut data = raw.to_vec();
    for filter in filters(dict) {
        data = match filter.as_str() {
            "FlateDecode" | "Fl" => {
                let mut out = Vec::new();
                let res = ZlibDecoder::new(data.as_slice())
                    .take(MAX_DECODED)
                    .read_to_end(&mut out);
                // A truncated stream still yields its readable prefix.
                if res.is_err() && out.is_empty() {
                    return None;
                }
                out
            }
            "ASCIIHexDecode" | "AHx" => Lex::new(&data, 0).hex(),
            "ASCII85Decode" | "A85" => ascii85(&data),
            _ => return None,
        };
    }
    Some(data)
}

fn ascii85(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut group: Vec<u32> = Vec::with_capacity(5);
    let flush = |group: &mut Vec<u32>, out: &mut Vec<u8>| {
        let n = group.len();
        if n < 2 {
            group.clear();
            return;
        }
        while group.len() < 5 {
            group.push(84);
        }
        let value = group
            .iter()
            .fold(0u32, |acc, d| acc.wrapping_mul(85).wrapping_add(*d));
        out.extend(value.to_be_bytes().iter().take(n - 1));
        group.clear();
    };
    let body = data
        .iter()
        .copied()
        .skip_while(|b| is_ws(*b))
        .collect::<Vec<u8>>();
    let body = body.strip_prefix(b"<~").unwrap_or(&body);
    for &c in body {
        match c {
            b'~' => break,
            b'z' if group.is_empty() => out.extend([0u8; 4]),
            b'!'..=b'u' => {
                group.push(u32::from(c - b'!'));
                if group.len() == 5 {
                    flush(&mut group, &mut out);
                }
            }
            _ => {}
        }
    }
    flush(&mut group, &mut out);
    out
}
