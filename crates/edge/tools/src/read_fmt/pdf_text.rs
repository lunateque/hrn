//! Page content streams to text: fonts (`ToUnicode` `CMap`s, WinAnsi/MacRoman + Differences,
//! widths) and the text-showing operators, with line and word breaks inferred from positions.

use super::pdf::Doc;
use super::pdf_obj::{Dict, Lex, Obj};
use std::collections::HashMap;
use std::rc::Rc;

const WIN_1252: &str = "€\u{81}‚ƒ„…†‡ˆ‰Š‹Œ\u{8d}Ž\u{8f}\u{90}‘’“”•–—˜™š›œ\u{9d}žŸ";
const MAC_ROMAN: &str = "ÄÅÇÉÑÖÜáàâäãåçéèêëíìîïñóòôöõúùûü†°¢£§•¶ß®©™´¨≠ÆØ∞±≤≥¥µ∂∑∏π∫ªºΩæø¿¡¬√ƒ≈∆«»… \
                         ÀÃÕŒœ–—“”‘’÷◊ÿŸ⁄€‹›ﬁﬂ‡·‚„‰ÂÊÁËÈÍÎÏÌÓÔ\u{f8ff}ÒÚÛÙıˆ˜¯˘˙˚¸˝˛ˇ";

/// Decoded fonts, shared across pages by object number.
#[derive(Default)]
pub(super) struct Fonts {
    cache: HashMap<u32, Rc<Font>>,
}

struct Font {
    /// Character code to text, from `ToUnicode`.
    cmap: HashMap<u32, String>,
    /// Bytes per code (1 for simple fonts).
    width: usize,
    simple: bool,
    mac_roman: bool,
    /// `Differences` overrides of simple fonts.
    diff: HashMap<u32, String>,
    /// Glyph widths in 1/1000 em by code.
    widths: HashMap<u32, f64>,
    default_width: f64,
}

fn code_of(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b))
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks(2)
        .map(|p| {
            (u16::from(p.first().copied().unwrap_or(0)) << 8)
                | u16::from(p.get(1).copied().unwrap_or(0))
        })
        .collect()
}

impl Font {
    fn build(doc: &Doc<'_>, fd: &Dict) -> Self {
        let type0 = doc.get(fd, "Subtype").and_then(Obj::name) == Some("Type0");
        let mut font = Self {
            cmap: HashMap::new(),
            width: if type0 { 2 } else { 1 },
            simple: !type0,
            mac_roman: false,
            diff: HashMap::new(),
            widths: HashMap::new(),
            default_width: if type0 { 1000.0 } else { 500.0 },
        };
        if let Some(data) = fd.get("ToUnicode").and_then(|o| doc.stream(o)) {
            font.parse_cmap(&data);
        }
        if type0 {
            let desc = doc.get(fd, "DescendantFonts").and_then(|o| match o {
                Obj::Arr(a) => a.first(),
                _ => None,
            });
            if let Some(cid) = desc.and_then(|d| doc.dict(d)) {
                font.default_width = doc.get(cid, "DW").and_then(Obj::num).unwrap_or(1000.0);
                if let Some(Obj::Arr(w)) = doc.get(cid, "W") {
                    font.parse_cid_widths(doc, w);
                }
            }
        } else {
            font.parse_simple(doc, fd);
        }
        font
    }

    fn parse_simple(&mut self, doc: &Doc<'_>, fd: &Dict) {
        let first = doc.get(fd, "FirstChar").and_then(Obj::int).unwrap_or(0);
        if let Some(Obj::Arr(ws)) = doc.get(fd, "Widths") {
            for (i, w) in ws.iter().enumerate() {
                if let (Some(w), Ok(code)) = (
                    doc.resolve(w).num(),
                    u32::try_from(first + i64::try_from(i).unwrap_or(0)),
                ) {
                    self.widths.insert(code, w);
                }
            }
        }
        let enc = doc.get(fd, "Encoding");
        let base = match enc {
            Some(Obj::Name(n)) => Some(n.as_str()),
            Some(Obj::Dict(d)) => doc.get(d, "BaseEncoding").and_then(Obj::name),
            _ => None,
        };
        self.mac_roman = base == Some("MacRomanEncoding");
        if let Some(Obj::Dict(d)) = enc
            && let Some(Obj::Arr(items)) = doc.get(d, "Differences")
        {
            let mut code = 0u32;
            for item in items {
                match doc.resolve(item) {
                    Obj::Name(n) => {
                        if let Some(s) = glyph_text(n) {
                            self.diff.insert(code, s);
                        }
                        code += 1;
                    }
                    other => {
                        code = other
                            .int()
                            .and_then(|i| u32::try_from(i).ok())
                            .unwrap_or(code);
                    }
                }
            }
        }
    }

    /// `/W [c [w1 w2 ...] cfirst clast w ...]`.
    fn parse_cid_widths(&mut self, doc: &Doc<'_>, w: &[Obj]) {
        let mut i = 0usize;
        while let Some(start) = w.get(i).map(|o| doc.resolve(o)).and_then(Obj::int) {
            let Ok(start) = u32::try_from(start) else {
                return;
            };
            match w.get(i + 1).map(|o| doc.resolve(o)) {
                Some(Obj::Arr(list)) => {
                    for (k, width) in list.iter().enumerate() {
                        if let (Some(width), Ok(k)) = (doc.resolve(width).num(), u32::try_from(k)) {
                            self.widths.insert(start + k, width);
                        }
                    }
                    i += 2;
                }
                Some(last) => {
                    let (Some(last), Some(width)) = (
                        last.int(),
                        w.get(i + 2).map(|o| doc.resolve(o)).and_then(Obj::num),
                    ) else {
                        return;
                    };
                    for code in start..=u32::try_from(last).unwrap_or(start).min(start + 0xFFFF) {
                        self.widths.insert(code, width);
                    }
                    i += 3;
                }
                None => return,
            }
        }
    }

    fn parse_cmap(&mut self, data: &[u8]) {
        let mut lex = Lex::new(data, 0);
        let mut stack: Vec<Obj> = Vec::new();
        while let Some(v) = lex.value() {
            let Obj::Op(op) = &v else {
                stack.push(v);
                continue;
            };
            match op.as_str() {
                "endcodespacerange" => {
                    if let Some(Obj::Str(lo)) = stack.first()
                        && !lo.is_empty()
                    {
                        self.width = lo.len();
                    }
                }
                "endbfchar" => {
                    for pair in stack.chunks(2) {
                        if let [Obj::Str(src), Obj::Str(dst)] = pair {
                            self.cmap
                                .insert(code_of(src), String::from_utf16_lossy(&utf16(dst)));
                        }
                    }
                }
                "endbfrange" => self.bf_ranges(&stack),
                "begincodespacerange" | "beginbfchar" | "beginbfrange" => {}
                _ => continue,
            }
            stack.clear();
        }
    }

    fn bf_ranges(&mut self, stack: &[Obj]) {
        for triple in stack.chunks(3) {
            let [Obj::Str(lo), Obj::Str(hi), dst] = triple else {
                continue;
            };
            let (lo, hi) = (code_of(lo), code_of(hi));
            if hi < lo || hi - lo > 0xFFFF {
                continue;
            }
            match dst {
                Obj::Str(d) => {
                    let units = utf16(d);
                    for off in 0..=(hi - lo) {
                        let mut u = units.clone();
                        if let Some(last) = u.last_mut() {
                            *last = last.wrapping_add(u16::try_from(off).unwrap_or(0));
                        }
                        self.cmap.insert(lo + off, String::from_utf16_lossy(&u));
                    }
                }
                Obj::Arr(items) => {
                    for (off, item) in items.iter().enumerate() {
                        if let (Obj::Str(d), Ok(off)) = (item, u32::try_from(off)) {
                            self.cmap
                                .insert(lo + off, String::from_utf16_lossy(&utf16(d)));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Append the text of `bytes` to `out`; returns the advance in 1/1000 em. Sets `skipped`
    /// when a code has no known Unicode value.
    fn show(&self, bytes: &[u8], out: &mut String, skipped: &mut bool) -> f64 {
        let mut advance = 0.0;
        for chunk in bytes.chunks(self.width.max(1)) {
            let code = code_of(chunk);
            advance += self
                .widths
                .get(&code)
                .copied()
                .unwrap_or(self.default_width);
            if let Some(s) = self.cmap.get(&code) {
                out.push_str(s);
            } else if !self.simple {
                *skipped = true;
            } else if let Some(s) = self.diff.get(&code) {
                out.push_str(s);
            } else if let Some(c) = self.base_char(code) {
                out.push(c);
            }
        }
        advance
    }

    fn base_char(&self, code: u32) -> Option<char> {
        let high = usize::try_from(code.saturating_sub(0x80)).ok()?;
        match code {
            0x80..=0xFF if self.mac_roman => MAC_ROMAN.chars().nth(high),
            0x80..=0x9F => WIN_1252.chars().nth(high),
            0x20..=0x7E | 0xA0..=0xFF => char::from_u32(code),
            _ => None,
        }
    }
}

/// Text of a Type1-style glyph name (`A`, `uni00E9`, `eacute`, `period`, ...).
fn glyph_text(name: &str) -> Option<String> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(c.to_string());
    }
    if let Some(hex) = name.strip_prefix("uni").filter(|h| h.len() == 4)
        && let Some(c) = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
    {
        return Some(c.to_string());
    }
    let plain = match name {
        "space" => " ",
        "period" => ".",
        "comma" => ",",
        "colon" => ":",
        "semicolon" => ";",
        "hyphen" | "minus" => "-",
        "underscore" => "_",
        "slash" => "/",
        "backslash" => "\\",
        "parenleft" => "(",
        "parenright" => ")",
        "bracketleft" => "[",
        "bracketright" => "]",
        "braceleft" => "{",
        "braceright" => "}",
        "quotesingle" => "'",
        "quotedbl" => "\"",
        "quoteleft" => "‘",
        "quoteright" => "’",
        "quotedblleft" => "“",
        "quotedblright" => "”",
        "endash" => "–",
        "emdash" => "—",
        "bullet" => "•",
        "ellipsis" => "…",
        "ampersand" => "&",
        "asterisk" => "*",
        "at" => "@",
        "numbersign" => "#",
        "dollar" => "$",
        "percent" => "%",
        "plus" => "+",
        "equal" => "=",
        "less" => "<",
        "greater" => ">",
        "question" => "?",
        "exclam" => "!",
        "asciicircum" => "^",
        "asciitilde" => "~",
        "bar" => "|",
        "grave" => "`",
        "fi" => "ﬁ",
        "fl" => "ﬂ",
        "zero" => "0",
        "one" => "1",
        "two" => "2",
        "three" => "3",
        "four" => "4",
        "five" => "5",
        "six" => "6",
        "seven" => "7",
        "eight" => "8",
        "nine" => "9",
        "germandbls" => "ß",
        "copyright" => "©",
        "registered" => "®",
        "degree" => "°",
        _ => "",
    };
    if !plain.is_empty() {
        return Some(plain.to_owned());
    }
    // Letter plus accent, e.g. `eacute`: base letter and a combining mark.
    [
        ("acute", '\u{301}'),
        ("grave", '\u{300}'),
        ("circumflex", '\u{302}'),
        ("dieresis", '\u{308}'),
        ("tilde", '\u{303}'),
        ("ring", '\u{30A}'),
        ("cedilla", '\u{327}'),
        ("caron", '\u{30C}'),
    ]
    .into_iter()
    .find_map(|(suffix, mark)| {
        let base = name.strip_suffix(suffix)?;
        let mut it = base.chars();
        let (c, None) = (it.next()?, it.next()) else {
            return None;
        };
        c.is_ascii_alphabetic().then(|| format!("{c}{mark}"))
    })
}

impl Fonts {
    fn lookup(&mut self, doc: &Doc<'_>, fonts: &Dict, name: &str) -> Option<Rc<Font>> {
        let entry = fonts.get(name)?;
        if let Obj::Ref(id) = entry {
            if let Some(f) = self.cache.get(id) {
                return Some(Rc::clone(f));
            }
            let font = Rc::new(Font::build(doc, doc.dict(entry)?));
            self.cache.insert(*id, Rc::clone(&font));
            return Some(font);
        }
        Some(Rc::new(Font::build(doc, doc.dict(entry)?)))
    }
}

/// Pen state while walking a content stream.
struct Pen {
    size: f64,
    a: f64,
    d: f64,
    lx: f64,
    ly: f64,
    /// Pen position after the last glyph, in user space.
    cx: f64,
    cy: f64,
    leading: f64,
    pending: bool,
    started: bool,
}

impl Pen {
    fn em(&self) -> f64 {
        self.size.abs().max(1.0)
    }

    /// Insert the break implied by a repositioning before the next text.
    fn settle(&mut self, out: &mut String) {
        if self.pending && self.started {
            if (self.ly - self.cy).abs() > 0.4 * self.em() * self.d.abs().max(0.01) {
                end_line(out);
            } else if self.lx - self.cx > 0.2 * self.em() * self.a.abs().max(0.01) {
                space(out);
            }
        }
        if self.pending {
            self.cx = self.lx;
            self.cy = self.ly;
            self.pending = false;
        }
    }
}

fn space(out: &mut String) {
    if !out.is_empty() && !out.ends_with([' ', '\n']) {
        out.push(' ');
    }
}

fn end_line(out: &mut String) {
    let kept = out.trim_end_matches(' ').len();
    out.truncate(kept);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Text of one page and whether some of it could not be decoded.
pub(super) fn page_text(
    doc: &Doc<'_>,
    page: &Dict,
    resources: Option<&Obj>,
    fonts: &mut Fonts,
) -> (String, bool) {
    let mut content: Vec<u8> = Vec::new();
    match doc.get(page, "Contents") {
        Some(Obj::Arr(parts)) => {
            for part in parts {
                content.extend(doc.stream(part).unwrap_or_default());
                content.push(b'\n');
            }
        }
        Some(single) => content = doc.stream(single).unwrap_or_default(),
        None => {}
    }
    let font_dict = resources
        .and_then(|r| doc.dict(r))
        .and_then(|r| doc.get(r, "Font"))
        .and_then(|f| doc.dict(f));
    let mut out = String::new();
    let mut skipped = content.is_empty();
    let mut current: Option<Rc<Font>> = None;
    let mut pen = Pen {
        size: 10.0,
        a: 1.0,
        d: 1.0,
        lx: 0.0,
        ly: 0.0,
        cx: 0.0,
        cy: 0.0,
        leading: 0.0,
        pending: false,
        started: false,
    };
    let mut lex = Lex::new(&content, 0);
    let mut ops: Vec<Obj> = Vec::new();
    while let Some(v) = lex.value() {
        let Obj::Op(op) = v else {
            ops.push(v);
            continue;
        };
        let nums: Vec<f64> = ops.iter().filter_map(Obj::num).collect();
        let n = |i: usize| nums.get(i).copied().unwrap_or(0.0);
        match op.as_str() {
            "ID" => lex.skip_inline_image(),
            "BT" => {
                (pen.lx, pen.ly, pen.a, pen.d) = (0.0, 0.0, 1.0, 1.0);
                pen.pending = true;
            }
            "Tf" => {
                if let Some(size) = nums.last() {
                    pen.size = *size;
                }
                if let (Some(Obj::Name(name)), Some(fd)) = (ops.first(), font_dict) {
                    current = fonts.lookup(doc, fd, name);
                }
            }
            "Td" | "TD" => {
                pen.lx += n(0) * pen.a;
                pen.ly += n(1) * pen.d;
                if op == "TD" {
                    pen.leading = -n(1);
                }
                pen.pending = true;
            }
            "Tm" => {
                (pen.a, pen.d, pen.lx, pen.ly) = (n(0), n(3), n(4), n(5));
                pen.pending = true;
            }
            "TL" => pen.leading = n(0),
            "T*" => next_line(&mut pen),
            "Tj" | "'" | "\"" | "TJ" => {
                if op != "Tj" && op != "TJ" {
                    next_line(&mut pen);
                }
                let items: &[Obj] = match ops.last() {
                    Some(Obj::Arr(items)) => items,
                    Some(single) => std::slice::from_ref(single),
                    None => &[],
                };
                show_items(items, &mut pen, current.as_deref(), &mut out, &mut skipped);
            }
            _ => {}
        }
        ops.clear();
    }
    (out, skipped)
}

fn next_line(pen: &mut Pen) {
    pen.ly -= if pen.leading == 0.0 {
        pen.em()
    } else {
        pen.leading
    } * pen.d;
    pen.pending = true;
}
/// Show strings and apply `TJ` adjustments, tracking the pen across them.
fn show_items(
    items: &[Obj],
    pen: &mut Pen,
    font: Option<&Font>,
    out: &mut String,
    skipped: &mut bool,
) {
    for item in items {
        if let Obj::Str(bytes) = item {
            pen.settle(out);
            let adv = if let Some(f) = font {
                f.show(bytes, out, skipped)
            } else {
                out.push_str(&String::from_utf8_lossy(bytes));
                f64::from(u32::try_from(bytes.len()).unwrap_or(0)) * 500.0
            };
            pen.cx += adv / 1000.0 * pen.size * pen.a;
            pen.started = true;
        } else if let Some(adjust) = item.num() {
            pen.cx -= adjust / 1000.0 * pen.size * pen.a;
            if adjust <= -180.0 {
                space(out);
            }
        }
    }
}
