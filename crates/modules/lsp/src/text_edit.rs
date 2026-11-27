//! Applying LSP `TextEdit[]` responses (used by `[lsp] format_on_write = true`).

use hrn_kernel::serde_json::Value;

/// One LSP `TextEdit`: 0-based UTF-16 `(line, character)` start/end range and replacement text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextEdit {
    pub(crate) start_line: u32,
    pub(crate) start_char: u32,
    pub(crate) end_line: u32,
    pub(crate) end_char: u32,
    pub(crate) new_text: String,
}

/// Parse an LSP `textDocument/formatting` response (`TextEdit[] | null`).
pub(crate) fn parse_text_edits(val: &Value) -> Vec<TextEdit> {
    let Some(arr) = val.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let start_line = u32::try_from(item.pointer("/range/start/line")?.as_u64()?).ok()?;
            let start_char =
                u32::try_from(item.pointer("/range/start/character")?.as_u64()?).ok()?;
            let end_line = u32::try_from(item.pointer("/range/end/line")?.as_u64()?).ok()?;
            let end_char = u32::try_from(item.pointer("/range/end/character")?.as_u64()?).ok()?;
            let new_text = item.get("newText")?.as_str()?.to_owned();
            Some(TextEdit {
                start_line,
                start_char,
                end_line,
                end_char,
                new_text,
            })
        })
        .collect()
}

/// Apply LSP `TextEdit`s to `source`, returning the formatted text (or `None` if unchanged).
pub(crate) fn apply_text_edits(source: &str, edits: &[TextEdit]) -> Option<String> {
    if edits.is_empty() {
        return None;
    }
    let line_starts = compute_line_starts(source);
    let mut resolved: Vec<(usize, usize, &str)> = edits
        .iter()
        .filter_map(|e| {
            let start = pos_to_byte(source, &line_starts, e.start_line, e.start_char)?;
            let end = pos_to_byte(source, &line_starts, e.end_line, e.end_char)?;
            (start <= end).then_some((start, end, e.new_text.as_str()))
        })
        .collect();
    if resolved.is_empty() {
        return None;
    }
    // Apply from end to start so earlier byte offsets stay valid.
    resolved.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut out = source.to_owned();
    let mut last_start = usize::MAX;
    for (start, end, new_text) in resolved {
        if end > last_start || end > out.len() {
            continue;
        }
        out.replace_range(start..end, new_text);
        last_start = start;
    }
    (out != source).then_some(out)
}

fn compute_line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (idx, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

fn pos_to_byte(text: &str, line_starts: &[usize], line: u32, utf16_col: u32) -> Option<usize> {
    let line_idx = usize::try_from(line).ok()?;
    if line_idx >= line_starts.len() {
        return Some(text.len());
    }
    let start = *line_starts.get(line_idx)?;
    let next_start = line_starts.get(line_idx + 1).copied().unwrap_or(text.len());
    let slice = text.get(start..next_start)?;
    let line_body = slice.trim_end_matches(['\r', '\n']);
    let mut utf16_seen = 0u32;
    for (rel_byte, ch) in line_body.char_indices() {
        if utf16_seen >= utf16_col {
            return Some(start + rel_byte);
        }
        utf16_seen = utf16_seen.saturating_add(u32::try_from(ch.len_utf16()).unwrap_or(1));
    }
    Some(start + line_body.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::serde_json::json;

    #[test]
    fn applies_multiple_text_edits_in_reverse_order() {
        let src = "fn main(){\nlet x=1;\n}\n";
        let raw = json!([
            {
                "range": { "start": { "line": 0, "character": 9 }, "end": { "line": 0, "character": 10 } },
                "newText": " {"
            },
            {
                "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 8 } },
                "newText": "    let x = 1;"
            }
        ]);
        let edits = parse_text_edits(&raw);
        let got = apply_text_edits(src, &edits).expect("changed");
        assert_eq!(got, "fn main() {\n    let x = 1;\n}\n");
    }
}
