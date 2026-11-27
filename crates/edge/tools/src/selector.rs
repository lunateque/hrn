//! Trailing path selectors of `read`: `:N`, `:N-M`, `:N+K`, `:N-`, `:-K`, comma lists of those,
//! `:raw` (before or after the lines) and `:conflicts` (the merge-conflict index), e.g.
//! `src/a.rs:5-16,960-973:raw`.

use hrn_kernel::{HrnError, Result};
use std::fmt::Write as _;

/// One requested line span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Span {
    /// 1-indexed inclusive lines; `None` runs to the end of the text.
    Lines { start: usize, end: Option<usize> },
    /// The last `n` lines.
    Tail(usize),
}

/// A parsed selector: optional spans plus the raw and conflict-index flags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Selector {
    pub(crate) raw: bool,
    pub(crate) spans: Vec<Span>,
    /// `:conflicts` — list the merge-conflict hunks of the file instead of its lines.
    pub(crate) conflicts: bool,
    /// `:img` — force the raster-image reading.
    pub(crate) img: bool,
}

/// Split a trailing selector off `path`. Unrecognized `:suffix` text stays part of the path.
///
/// # Errors
/// A selector-shaped suffix with an invalid value (`:0`, `:9-3`, `:5+0`).
pub(crate) fn split(path: &str) -> Result<(&str, Selector)> {
    let mut rest = path;
    let mut sel = Selector::default();
    for _ in 0..3 {
        let Some((head, tail)) = rest.rsplit_once(':') else {
            break;
        };
        if tail == "raw" && !sel.raw {
            sel.raw = true;
        } else if tail == "conflicts" && !sel.conflicts {
            sel.conflicts = true;
        } else if tail == "img" && !sel.img {
            sel.img = true;
        } else if sel.spans.is_empty()
            && let Some(spans) = parse_spans(tail)?
        {
            sel.spans = spans;
        } else {
            break;
        }
        rest = head;
    }
    Ok((rest, sel))
}

/// Split a trailing selector off a URL. Line spans are only peeled while the remainder still
/// has a path after the host, so `https://host:8080` is a URL and not selector `:8080`.
///
/// # Errors
/// As [`split`].
pub(crate) fn split_url(raw: &str) -> Result<(&str, Selector)> {
    let mut rest = raw;
    let mut sel = Selector::default();
    for _ in 0..3 {
        let Some((head, tail)) = rest.rsplit_once(':') else {
            break;
        };
        let flag = matches!(
            (tail, sel.raw, sel.img, sel.conflicts),
            ("raw", false, _, _) | ("img", _, false, _) | ("conflicts", _, _, false)
        );
        let spans = sel.spans.is_empty() && path_after_host(head) && parse_spans(tail)?.is_some();
        if !flag && !spans {
            break;
        }
        match tail {
            "raw" => sel.raw = true,
            "img" => sel.img = true,
            "conflicts" => sel.conflicts = true,
            _ => sel.spans = parse_spans(tail)?.unwrap_or_default(),
        }
        rest = head;
    }
    Ok((rest, sel))
}

/// Whether `url` still has a path segment after `scheme://host`.
fn path_after_host(url: &str) -> bool {
    url.split_once("://")
        .is_some_and(|(_, rest)| rest.contains('/'))
}

/// `Ok(None)` when `text` is not selector-shaped at all.
fn parse_spans(text: &str) -> Result<Option<Vec<Span>>> {
    if text.is_empty()
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || b"-+.,".contains(&b))
        || !text.bytes().any(|b| b.is_ascii_digit())
    {
        return Ok(None);
    }
    let mut spans = Vec::new();
    for part in text.split(',') {
        match parse_part(&part.replace("..", "-"))? {
            Some(span) => spans.push(span),
            None => return Ok(None),
        }
    }
    Ok(Some(spans))
}

fn number(text: &str) -> Option<usize> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Saturate absurdly long digit strings instead of rejecting them.
    Some(text.parse().unwrap_or(usize::MAX))
}

fn line_number(n: usize) -> Result<usize> {
    if n == 0 {
        return Err(HrnError::Tool(
            "line selector 0 is invalid; lines are 1-indexed. Use :1".into(),
        ));
    }
    Ok(n)
}

fn parse_part(part: &str) -> Result<Option<Span>> {
    if let Some(tail) = part.strip_prefix('-') {
        return match number(tail) {
            Some(0) => Err(HrnError::Tool(
                "`:-0` selects nothing; use `:-K` with K >= 1".into(),
            )),
            Some(k) => Ok(Some(Span::Tail(k))),
            None => Ok(None),
        };
    }
    if let Some((a, k)) = part.split_once('+') {
        let (Some(start), Some(count)) = (number(a), number(k)) else {
            return Ok(None);
        };
        let start = line_number(start)?;
        if count == 0 {
            return Err(HrnError::Tool(format!(
                "`:{part}` counts 0 lines; use K >= 1"
            )));
        }
        return Ok(Some(Span::Lines {
            start,
            end: Some(start.saturating_add(count - 1)),
        }));
    }
    if let Some((a, b)) = part.split_once('-') {
        let Some(start) = number(a) else {
            return Ok(None);
        };
        let start = line_number(start)?;
        if b.is_empty() {
            return Ok(Some(Span::Lines { start, end: None }));
        }
        let Some(end) = number(b) else {
            return Ok(None);
        };
        if end < start {
            return Err(HrnError::Tool(format!(
                "`:{part}` ends before it starts; use :{start}-{start} or later"
            )));
        }
        return Ok(Some(Span::Lines {
            start,
            end: Some(end),
        }));
    }
    let Some(n) = number(part) else {
        return Ok(None);
    };
    let n = line_number(n)?;
    Ok(Some(Span::Lines {
        start: n,
        end: Some(n),
    }))
}

impl Selector {
    /// Inclusive 1-indexed windows over a text of `total` lines: tails resolved, open ends
    /// bounded by `default_limit`, sorted and merged. Windows past the end are dropped, so an
    /// empty result with non-empty `spans` means the selection is out of bounds.
    pub(crate) fn windows(&self, total: usize, default_limit: usize) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        if self.spans.is_empty() {
            if total > 0 {
                out.push((1, total.min(default_limit)));
            }
            return out;
        }
        for span in &self.spans {
            let window = match *span {
                Span::Tail(k) => (total.saturating_sub(k) + 1, total),
                Span::Lines { start, end } => (
                    start,
                    end.unwrap_or_else(|| start.saturating_add(default_limit - 1))
                        .min(total),
                ),
            };
            if window.0 <= window.1 {
                out.push(window);
            }
        }
        out.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(out.len());
        for (a, b) in out {
            match merged.last_mut() {
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        merged
    }

    /// A single bounded span (`:50-80`, `:7`) gets context lines around it in non-raw reads.
    pub(crate) fn is_single_bounded(&self) -> bool {
        matches!(self.spans.as_slice(), [Span::Lines { end: Some(_), .. }])
    }

    /// Canonical spelling (`5-16,960-973:raw`) for supersede keys; empty for a bare read.
    pub(crate) fn key(&self) -> String {
        let mut out = String::new();
        for (i, span) in self.spans.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = match *span {
                Span::Tail(k) => write!(out, "-{k}"),
                Span::Lines {
                    start,
                    end: Some(e),
                } => write!(out, "{start}-{e}"),
                Span::Lines { start, end: None } => write!(out, "{start}-"),
            };
        }
        if self.raw {
            out.push_str(":raw");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(a: usize, b: usize) -> Span {
        Span::Lines {
            start: a,
            end: Some(b),
        }
    }

    #[test]
    fn parses_every_selector_form() {
        let cases: [(&str, &str, Vec<Span>, bool); 9] = [
            ("a.rs:7", "a.rs", vec![lines(7, 7)], false),
            ("a.rs:5-9", "a.rs", vec![lines(5, 9)], false),
            ("a.rs:5..9", "a.rs", vec![lines(5, 9)], false),
            ("a.rs:5+3", "a.rs", vec![lines(5, 7)], false),
            (
                "a.rs:5-",
                "a.rs",
                vec![Span::Lines {
                    start: 5,
                    end: None,
                }],
                false,
            ),
            ("a.rs:-4", "a.rs", vec![Span::Tail(4)], false),
            (
                "a.rs:5-16,960-973",
                "a.rs",
                vec![lines(5, 16), lines(960, 973)],
                false,
            ),
            ("a.rs:raw:2-4", "a.rs", vec![lines(2, 4)], true),
            ("a.rs:2-4:raw", "a.rs", vec![lines(2, 4)], true),
        ];
        for (input, path, spans, raw) in cases {
            let (p, sel) = split(input).unwrap();
            assert_eq!((p, sel.spans, sel.raw), (path, spans, raw), "{input}");
        }
    }

    #[test]
    fn leaves_non_selectors_in_the_path() {
        for input in ["C:\\dir\\a.rs", "a.rs", "dir/a:b", "notes:v2", "a.rs:"] {
            let (p, sel) = split(input).unwrap();
            assert_eq!(p, input);
            assert_eq!(sel, Selector::default());
        }
        let (p, sel) = split("a.rs:raw").unwrap();
        assert_eq!((p, sel.raw, sel.spans.is_empty()), ("a.rs", true, true));
    }

    #[test]
    fn rejects_invalid_values() {
        for input in ["a.rs:0", "a.rs:9-3", "a.rs:5+0", "a.rs:-0"] {
            assert!(split(input).is_err(), "{input}");
        }
    }

    #[test]
    fn windows_merge_clip_and_resolve_tails() {
        let sel = split("f:10-20,15-30,5-6,-3").unwrap().1;
        assert_eq!(sel.windows(100, 300), vec![(5, 6), (10, 30), (98, 100)]);
        let past = split("f:500-600").unwrap().1;
        assert!(past.windows(100, 300).is_empty());
        let open = split("f:10-").unwrap().1;
        assert_eq!(open.windows(1000, 300), vec![(10, 309)]);
        assert_eq!(Selector::default().windows(1000, 300), vec![(1, 300)]);
        assert_eq!(Selector::default().windows(0, 300), vec![]);
    }
}
