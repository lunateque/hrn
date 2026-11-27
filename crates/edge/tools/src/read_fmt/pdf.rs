//! PDF text extraction without a PDF library: scan indirect objects (plain and in object
//! streams), walk the page tree, decode each page's content streams (see `pdf_text`).
//!
//! Covers the common producers (Word, LaTeX, Chrome, Pages). Scanned pages, encrypted files and
//! fonts that carry no Unicode information (Identity-H without `ToUnicode`) yield no text.

use super::pdf_obj::{Dict, Lex, Obj, decode};
use super::pdf_text::{self, Fonts};
use hrn_kernel::{HrnError, Result};
use std::collections::HashMap;
use std::fmt::Write as _;

static NULL: Obj = Obj::Null;
const MAX_TREE_DEPTH: usize = 32;

/// The parsed object table over the source bytes.
pub(super) struct Doc<'a> {
    src: &'a [u8],
    objs: HashMap<u32, Obj>,
}

/// Text of every page, each under a `## Page N` heading.
pub(super) fn convert(bytes: &[u8]) -> Result<String> {
    let doc = Doc::parse(bytes);
    let pages = doc.pages();
    if pages.is_empty() {
        return Err(HrnError::Tool(if contains(bytes, b"/Encrypt") {
            "encrypted PDFs are not supported".into()
        } else {
            "no pages found; not a readable PDF".into()
        }));
    }
    let mut out = String::new();
    let mut skipped = false;
    let mut fonts = Fonts::default();
    for (i, (page, resources)) in pages.iter().enumerate() {
        let _ = writeln!(out, "## Page {}", i + 1);
        let text = pdf_text::page_text(&doc, page, *resources, &mut fonts);
        out.push_str(text.0.trim_end());
        out.push_str("\n\n");
        skipped |= text.1;
    }
    if skipped {
        out.push_str(
            "[Some text was skipped: its fonts carry no Unicode mapping (or the page is a scan).]\n",
        );
    }
    Ok(out)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

impl<'a> Doc<'a> {
    fn parse(src: &'a [u8]) -> Self {
        let mut doc = Self {
            src,
            objs: HashMap::new(),
        };
        doc.scan();
        doc.unpack_object_streams();
        doc
    }

    /// Every `N G obj` in file order; later revisions replace earlier ones.
    fn scan(&mut self) {
        let mut pos = 0usize;
        while let Some(rel) = self
            .src
            .get(pos..)
            .and_then(|s| s.windows(3).position(|w| w == b"obj"))
        {
            let at = pos + rel;
            pos = at + 3;
            let Some(id) = object_id_before(self.src, at) else {
                continue;
            };
            let mut lex = Lex::new(self.src, pos);
            if let Some(obj) = lex.object() {
                self.objs.insert(id, obj);
                pos = lex.pos.max(pos);
            }
        }
    }

    fn unpack_object_streams(&mut self) {
        let mut found: Vec<(u32, Obj)> = Vec::new();
        for obj in self.objs.values() {
            let Obj::Stream(dict, range) = obj else {
                continue;
            };
            if dict.get("Type").and_then(Obj::name) != Some("ObjStm") {
                continue;
            }
            let (Some(n), Some(first)) = (
                dict.get("N").and_then(Obj::int),
                dict.get("First").and_then(Obj::int),
            ) else {
                continue;
            };
            let Some(data) = self
                .src
                .get(range.clone())
                .and_then(|raw| decode(dict, raw))
            else {
                continue;
            };
            let (Ok(n), Ok(first)) = (usize::try_from(n), usize::try_from(first)) else {
                continue;
            };
            let mut head = Lex::new(&data, 0);
            for _ in 0..n {
                let (Some(id), Some(off)) = (
                    head.value().and_then(|o| o.int()),
                    head.value().and_then(|o| o.int()),
                ) else {
                    break;
                };
                if let (Ok(id), Ok(off)) = (u32::try_from(id), usize::try_from(off))
                    && let Some(v) = Lex::new(&data, first + off).value()
                {
                    found.push((id, v));
                }
            }
        }
        for (id, obj) in found {
            self.objs.entry(id).or_insert(obj);
        }
    }

    /// Follow indirect references.
    pub(super) fn resolve<'s>(&'s self, mut o: &'s Obj) -> &'s Obj {
        for _ in 0..8 {
            match o {
                Obj::Ref(id) => o = self.objs.get(id).unwrap_or(&NULL),
                _ => break,
            }
        }
        o
    }

    /// The dictionary behind `o` (a stream's dictionary included).
    pub(super) fn dict<'s>(&'s self, o: &'s Obj) -> Option<&'s Dict> {
        match self.resolve(o) {
            Obj::Dict(d) | Obj::Stream(d, _) => Some(d),
            _ => None,
        }
    }

    /// `d[key]`, resolved.
    pub(super) fn get<'s>(&'s self, d: &'s Dict, key: &str) -> Option<&'s Obj> {
        d.get(key).map(|o| self.resolve(o))
    }

    /// Decoded bytes of the stream behind `o`.
    pub(super) fn stream(&self, o: &Obj) -> Option<Vec<u8>> {
        match self.resolve(o) {
            Obj::Stream(d, range) => decode(d, self.src.get(range.clone())?),
            _ => None,
        }
    }

    /// Pages in reading order with their (inherited) resources.
    fn pages(&self) -> Vec<(&Dict, Option<&Obj>)> {
        let root = self.objs.values().find_map(|o| match o {
            Obj::Dict(d) if d.get("Type").and_then(Obj::name) == Some("Catalog") => d.get("Pages"),
            _ => None,
        });
        let mut out = Vec::new();
        if let Some(root) = root {
            self.walk(root, None, 0, &mut out);
        }
        if out.is_empty() {
            let mut ids: Vec<&u32> = self.objs.keys().collect();
            ids.sort_unstable();
            for id in ids {
                if let Some(Obj::Dict(d)) = self.objs.get(id)
                    && d.get("Type").and_then(Obj::name) == Some("Page")
                {
                    out.push((d, d.get("Resources")));
                }
            }
        }
        out
    }

    fn walk<'s>(
        &'s self,
        node: &'s Obj,
        inherited: Option<&'s Obj>,
        depth: usize,
        out: &mut Vec<(&'s Dict, Option<&'s Obj>)>,
    ) {
        let Some(d) = self.dict(node) else { return };
        if depth > MAX_TREE_DEPTH {
            return;
        }
        let resources = d.get("Resources").or(inherited);
        match self.get(d, "Kids") {
            Some(Obj::Arr(kids)) => {
                for kid in kids {
                    self.walk(kid, resources, depth + 1, out);
                }
            }
            _ if d.get("Type").and_then(Obj::name) == Some("Pages") => {}
            _ => out.push((d, resources)),
        }
    }
}

/// The object number of the `N G obj` header whose `obj` keyword starts at `at`.
fn object_id_before(src: &[u8], at: usize) -> Option<u32> {
    let head = src.get(..at)?;
    if !head.last().is_some_and(u8::is_ascii_whitespace) {
        return None;
    }
    if src.get(at + 3).is_some_and(u8::is_ascii_alphanumeric) {
        return None;
    }
    let trimmed = head.trim_ascii_end();
    let gen_start = trimmed
        .iter()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |i| i + 1);
    let before_gen = trimmed.get(..gen_start)?;
    if gen_start == trimmed.len() || !before_gen.last().is_some_and(u8::is_ascii_whitespace) {
        return None;
    }
    let id_part = before_gen.trim_ascii_end();
    let id_start = id_part
        .iter()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |i| i + 1);
    std::str::from_utf8(id_part.get(id_start..)?)
        .ok()?
        .parse()
        .ok()
}
