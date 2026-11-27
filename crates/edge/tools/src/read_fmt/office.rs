//! DOCX, XLSX, PPTX and EPUB as plain text: unzip, then a minimal XML text walk.

use super::archive::MEMBER_CAP;
use super::xml::{self, Ev};
use super::zip::Zip;
use crate::html;
use hrn_kernel::{HrnError, Result};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Cursor;

type Doc<'a> = Zip<Cursor<&'a [u8]>>;

/// Text of an Office / EPUB document by extension (`docx`, `xlsx`, `pptx`, `epub`).
pub(super) fn convert(ext: &str, bytes: &[u8]) -> Result<String> {
    let mut zip = Zip::open(Cursor::new(bytes))?;
    match ext {
        "docx" => docx(&mut zip),
        "xlsx" => xlsx(&mut zip),
        "pptx" => pptx(&mut zip),
        _ => epub(&mut zip),
    }
}

/// Whether `ext` is one of the document extensions handled here.
pub(super) fn handles(ext: &str) -> bool {
    matches!(ext, "docx" | "xlsx" | "pptx" | "epub")
}

fn text_of(zip: &mut Doc<'_>, name: &str) -> Result<Option<String>> {
    let Some(idx) = zip.find(name) else {
        return Ok(None);
    };
    let bytes = zip.read(idx, MEMBER_CAP)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

fn required(zip: &mut Doc<'_>, name: &str) -> Result<String> {
    text_of(zip, name)?
        .ok_or_else(|| HrnError::Tool(format!("document has no `{name}`; not a valid file")))
}

fn docx(zip: &mut Doc<'_>) -> Result<String> {
    let xml_text = required(zip, "word/document.xml")?;
    let mut out = String::new();
    let mut para = String::new();
    let mut prefix = String::new();
    let mut in_t = false;
    let mut in_cell = 0usize;
    xml::walk(&xml_text, |ev| match ev {
        Ev::Open("w:t", _) => in_t = true,
        Ev::Close("w:t") => in_t = false,
        Ev::Open("w:tab", _) => para.push('\t'),
        Ev::Open("w:br" | "w:cr", _) => para.push('\n'),
        Ev::Open("w:tc", _) => in_cell += 1,
        Ev::Open("w:pStyle", attrs) => {
            let level = xml::attr(attrs, "w:val")
                .and_then(|v| v.strip_prefix("Heading").map(str::to_owned))
                .and_then(|n| n.parse::<usize>().ok());
            if let Some(level) = level {
                prefix = format!("{} ", "#".repeat(level.clamp(1, 6)));
            }
        }
        Ev::Text(t) if in_t => para.push_str(&t),
        Ev::Close("w:p") => {
            if in_cell > 0 {
                out.push_str(para.trim());
                out.push(' ');
            } else {
                let _ = writeln!(out, "{prefix}{para}");
            }
            para.clear();
            prefix.clear();
        }
        Ev::Close("w:tc") => {
            in_cell = in_cell.saturating_sub(1);
            let trimmed = out.trim_end().len();
            out.truncate(trimmed);
            out.push_str(" | ");
        }
        Ev::Close("w:tr") => {
            let trimmed = out.trim_end_matches([' ', '|']).len();
            out.truncate(trimmed);
            out.push('\n');
        }
        _ => {}
    });
    Ok(out)
}

/// Column index of a cell reference such as `AB12` (`A` = 0).
fn column(reference: &str) -> usize {
    reference
        .bytes()
        .take_while(u8::is_ascii_alphabetic)
        .fold(0usize, |acc, b| {
            acc * 26 + usize::from(b.to_ascii_uppercase() - b'A' + 1)
        })
        .saturating_sub(1)
}

fn shared_strings(zip: &mut Doc<'_>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let Some(xml_text) = text_of(zip, "xl/sharedStrings.xml")? else {
        return Ok(out);
    };
    let (mut cur, mut in_t, mut phonetic) = (String::new(), false, false);
    xml::walk(&xml_text, |ev| match ev {
        Ev::Open("rPh", _) => phonetic = true,
        Ev::Close("rPh") => phonetic = false,
        Ev::Open("t", _) => in_t = true,
        Ev::Close("t") => in_t = false,
        Ev::Text(t) if in_t && !phonetic => cur.push_str(&t),
        Ev::Close("si") => out.push(std::mem::take(&mut cur)),
        _ => {}
    });
    Ok(out)
}

/// `(sheet name, part path)` in workbook order.
fn sheet_parts(zip: &mut Doc<'_>) -> Result<Vec<(String, String)>> {
    let workbook = required(zip, "xl/workbook.xml")?;
    let mut targets: HashMap<String, String> = HashMap::new();
    if let Some(rels) = text_of(zip, "xl/_rels/workbook.xml.rels")? {
        xml::walk(&rels, |ev| {
            if let Ev::Open("Relationship", attrs) = ev
                && let (Some(id), Some(target)) =
                    (xml::attr(attrs, "Id"), xml::attr(attrs, "Target"))
            {
                targets.insert(id, target);
            }
        });
    }
    let mut sheets = Vec::new();
    xml::walk(&workbook, |ev| {
        if let Ev::Open("sheet", attrs) = ev
            && let Some(name) = xml::attr(attrs, "name")
        {
            let part = xml::attr(attrs, "r:id")
                .and_then(|id| targets.get(&id).cloned())
                .map_or_else(
                    || format!("xl/worksheets/sheet{}.xml", sheets.len() + 1),
                    |t| match t.strip_prefix('/') {
                        Some(abs) => abs.to_owned(),
                        None => format!("xl/{t}"),
                    },
                );
            sheets.push((name, part));
        }
    });
    Ok(sheets)
}

fn xlsx(zip: &mut Doc<'_>) -> Result<String> {
    let shared = shared_strings(zip)?;
    let mut out = String::new();
    for (name, part) in sheet_parts(zip)? {
        let Some(sheet) = text_of(zip, &part)? else {
            continue;
        };
        let _ = writeln!(out, "## Sheet: {name}");
        let (mut row_no, mut cells): (String, Vec<String>) = (String::new(), Vec::new());
        let (mut col, mut kind, mut value) = (0usize, String::new(), String::new());
        let (mut in_v, mut in_t) = (false, false);
        xml::walk(&sheet, |ev| match ev {
            Ev::Open("row", attrs) => {
                row_no = xml::attr(attrs, "r").unwrap_or_default();
                cells.clear();
            }
            Ev::Open("c", attrs) => {
                col = xml::attr(attrs, "r").map_or(cells.len(), |r| column(&r));
                kind = xml::attr(attrs, "t").unwrap_or_default();
                value.clear();
            }
            Ev::Open("v", _) => in_v = true,
            Ev::Close("v") => in_v = false,
            Ev::Open("t", _) => in_t = true,
            Ev::Close("t") => in_t = false,
            Ev::Text(t) if in_v || in_t => value.push_str(&t),
            Ev::Close("c") => {
                let text = match kind.as_str() {
                    "s" => value
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| shared.get(i).cloned())
                        .unwrap_or_default(),
                    "b" => (if value.trim() == "1" { "TRUE" } else { "FALSE" }).to_owned(),
                    _ => value.clone(),
                };
                if cells.len() <= col {
                    cells.resize(col + 1, String::new());
                }
                if let Some(slot) = cells.get_mut(col) {
                    *slot = text.replace('\n', " ");
                }
            }
            Ev::Close("row") if cells.iter().any(|c| !c.is_empty()) => {
                let _ = writeln!(out, "{row_no}: {}", cells.join(" | "));
            }
            _ => {}
        });
        out.push('\n');
    }
    Ok(out)
}

/// Paragraph text of a `DrawingML` part (`a:p` / `a:t`).
fn drawing_text(xml_text: &str) -> String {
    let (mut out, mut in_t) = (String::new(), false);
    xml::walk(xml_text, |ev| match ev {
        Ev::Open("a:t", _) => in_t = true,
        Ev::Close("a:t") => in_t = false,
        Ev::Open("a:br", _) | Ev::Close("a:p") => out.push('\n'),
        Ev::Text(t) if in_t => out.push_str(&t),
        _ => {}
    });
    out
}

fn numbered_parts(zip: &Doc<'_>, dir: &str, stem: &str) -> Vec<(u32, String)> {
    let mut parts: Vec<(u32, String)> = zip
        .entries()
        .iter()
        .filter_map(|e| {
            let n = e
                .name
                .strip_prefix(dir)?
                .strip_prefix(stem)?
                .strip_suffix(".xml")?
                .parse()
                .ok()?;
            Some((n, e.name.clone()))
        })
        .collect();
    parts.sort_unstable();
    parts
}

fn pptx(zip: &mut Doc<'_>) -> Result<String> {
    let mut out = String::new();
    for (n, part) in numbered_parts(zip, "ppt/slides/", "slide") {
        let _ = writeln!(out, "## Slide {n}");
        out.push_str(&drawing_text(&required(zip, &part)?));
        let notes = format!("ppt/notesSlides/notesSlide{n}.xml");
        if let Some(text) = text_of(zip, &notes)? {
            let text = drawing_text(&text);
            if !text.trim().is_empty() {
                let _ = writeln!(out, "Notes:\n{}", text.trim_end());
            }
        }
        out.push('\n');
    }
    Ok(out)
}

/// `href` resolved against the directory `base` (`..` honoured).
fn join(base: &str, href: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    for seg in href.split('#').next().unwrap_or_default().split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn epub(zip: &mut Doc<'_>) -> Result<String> {
    let container = required(zip, "META-INF/container.xml")?;
    let mut opf_path = None;
    xml::walk(&container, |ev| {
        if let Ev::Open("rootfile", attrs) = ev
            && opf_path.is_none()
        {
            opf_path = xml::attr(attrs, "full-path");
        }
    });
    let opf_path =
        opf_path.ok_or_else(|| HrnError::Tool("epub container names no package file".into()))?;
    let opf = required(zip, &opf_path)?;
    let dir = opf_path.rsplit_once('/').map_or("", |(d, _)| d);
    let mut manifest: HashMap<String, String> = HashMap::new();
    let mut spine: Vec<String> = Vec::new();
    xml::walk(&opf, |ev| match ev {
        Ev::Open("item", attrs) => {
            if let (Some(id), Some(href)) = (xml::attr(attrs, "id"), xml::attr(attrs, "href")) {
                manifest.insert(id, join(dir, &href));
            }
        }
        Ev::Open("itemref", attrs) => spine.extend(xml::attr(attrs, "idref")),
        _ => {}
    });
    let mut chapters = Vec::new();
    for id in &spine {
        let Some(path) = manifest.get(id) else {
            continue;
        };
        let Some(html_text) = text_of(zip, path)? else {
            continue;
        };
        let text = html::to_text(&html_text);
        if !text.is_empty() {
            chapters.push(text);
        }
    }
    Ok(chapters.join("\n\n---\n\n"))
}
