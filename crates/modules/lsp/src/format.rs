//! Compact, token-efficient formatting for LSP diagnostics, locations, hover, symbols, and edits.

use crate::rpc::DiagnosticItem;
use crate::uri::uri_to_path;
use hrn_kernel::serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;

/// Render diagnostics for one file in `path:line:col [level] message` format.
pub(crate) fn format_diagnostics(
    path: &Path,
    cwd: &Path,
    items: &[DiagnosticItem],
    cap: usize,
) -> String {
    if items.is_empty() {
        return String::new();
    }
    let rel = display_rel(path, cwd);
    let mut out = String::new();
    for item in items.iter().take(cap) {
        let sev = match item.severity {
            1 => "error",
            2 => "warn",
            3 => "info",
            _ => "hint",
        };
        let mut tag = String::from(sev);
        if let Some(src) = &item.source {
            let _ = write!(tag, "/{src}");
        }
        if let Some(code) = &item.code {
            let _ = write!(tag, "({code})");
        }
        let first_line = item.message.lines().next().unwrap_or(&item.message);
        let _ = writeln!(
            out,
            "{rel}:{}:{} [{tag}] {first_line}",
            item.line + 1,
            item.col + 1
        );
    }
    if items.len() > cap {
        let _ = writeln!(out, "... ({} more diagnostics omitted)", items.len() - cap);
    }
    out
}

/// Format LSP `Location | Location[] | LocationLink[] | null` responses with 1-line context snippets.
pub(crate) fn format_locations(val: &Value, cwd: &Path, cap: usize) -> String {
    let locs = extract_locations(val);
    if locs.is_empty() {
        return "No locations found.".to_owned();
    }
    let mut out = String::new();
    for (uri, line, col) in locs.iter().take(cap) {
        let path_buf = uri_to_path(uri);
        let label = path_buf
            .as_deref()
            .map_or_else(|| uri.clone(), |p| display_rel(p, cwd));
        let snippet = path_buf
            .as_deref()
            .and_then(|p| read_line_snippet(p, cwd, *line));
        match snippet {
            Some(snip) => {
                let _ = writeln!(out, "{label}:{}:{}  {snip}", line + 1, col + 1);
            }
            None => {
                let _ = writeln!(out, "{label}:{}:{}", line + 1, col + 1);
            }
        }
    }
    if locs.len() > cap {
        let _ = writeln!(out, "... ({} more locations omitted)", locs.len() - cap);
    }
    out.trim_end().to_owned()
}

/// Format an LSP `Hover` response cleanly without raw JSON noise.
pub(crate) fn format_hover(val: &Value) -> String {
    if val.is_null() {
        return "No hover information at this position.".to_owned();
    }
    let Some(contents) = val.get("contents") else {
        return "No hover information at this position.".to_owned();
    };
    let text = match contents {
        Value::String(s) => s.clone(),
        Value::Object(map) => map
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.as_str()),
                Value::Object(m) => m.get("value").and_then(Value::as_str),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "No hover information at this position.".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Format `DocumentSymbol[]` or `SymbolInformation[]` concisely.
pub(crate) fn format_symbols(val: &Value, cwd: &Path, cap: usize) -> String {
    let Some(arr) = val.as_array() else {
        return "No symbols found.".to_owned();
    };
    if arr.is_empty() {
        return "No symbols found.".to_owned();
    }
    let mut lines = Vec::new();
    for item in arr {
        collect_symbol(item, cwd, 0, &mut lines, cap);
        if lines.len() >= cap {
            break;
        }
    }
    if lines.is_empty() {
        "No symbols found.".to_owned()
    } else {
        lines.join("\n")
    }
}

/// Format an LSP `WorkspaceEdit` as a deterministic, readable edit preview.
pub(crate) fn format_workspace_edit(val: &Value, cwd: &Path) -> String {
    if val.is_null() {
        return "LSP returned no rename edits for this symbol.".to_owned();
    }
    let mut out = String::new();
    let mut total = 0usize;
    if let Some(changes) = val.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            total += append_file_edits(&mut out, uri, edits, cwd);
        }
    }
    if let Some(doc_changes) = val.get("documentChanges").and_then(Value::as_array) {
        for change in doc_changes {
            if let (Some(uri), Some(edits)) = (
                change.pointer("/textDocument/uri").and_then(Value::as_str),
                change.get("edits"),
            ) {
                total += append_file_edits(&mut out, uri, edits, cwd);
            }
        }
    }
    if total == 0 {
        "LSP returned no rename edits for this symbol.".to_owned()
    } else {
        format!("Rename plan ({total} edit(s)):\n{}", out.trim_end())
    }
}

/// Format LSP `CallHierarchyIncomingCall[]` or `CallHierarchyOutgoingCall[]`.
pub(crate) fn format_call_hierarchy(
    val: &Value,
    direction: &str,
    cwd: &Path,
    cap: usize,
) -> String {
    let Some(arr) = val.as_array() else {
        return format!("No {direction} calls found.");
    };
    if arr.is_empty() {
        return format!("No {direction} calls found.");
    }
    let item_key = if direction == "incoming" {
        "from"
    } else {
        "to"
    };
    let mut lines = Vec::new();
    for entry in arr.iter().take(cap) {
        let Some(item) = entry.get(item_key) else {
            continue;
        };
        let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = symbol_kind_name(item.get("kind").and_then(Value::as_u64).unwrap_or(0));
        let uri = item.get("uri").and_then(Value::as_str).unwrap_or("");
        let label = uri_to_path(uri).map_or_else(|| uri.to_owned(), |p| display_rel(&p, cwd));
        let line = item
            .pointer("/selectionRange/start/line")
            .or_else(|| item.pointer("/range/start/line"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let col = item
            .pointer("/selectionRange/start/character")
            .or_else(|| item.pointer("/range/start/character"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let sites = entry
            .get("fromRanges")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if sites > 1 {
            lines.push(format!(
                "[{kind}] {name}  {label}:{}:{} ({sites} call sites)",
                line + 1,
                col + 1
            ));
        } else {
            lines.push(format!("[{kind}] {name}  {label}:{}:{}", line + 1, col + 1));
        }
    }
    if arr.len() > cap {
        lines.push(format!("... ({} more calls omitted)", arr.len() - cap));
    }
    if lines.is_empty() {
        format!("No {direction} calls found.")
    } else {
        lines.join("\n")
    }
}

/// Format LSP `(Command | CodeAction)[]` responses concisely.
pub(crate) fn format_code_actions(val: &Value, cwd: &Path, cap: usize) -> String {
    let Some(arr) = val.as_array() else {
        return "No code actions available at this position.".to_owned();
    };
    if arr.is_empty() {
        return "No code actions available at this position.".to_owned();
    }
    let mut lines = Vec::new();
    for action in arr.iter().take(cap) {
        let Some(title) = action.get("title").and_then(Value::as_str) else {
            continue;
        };
        let kind = action.get("kind").and_then(Value::as_str);
        let preferred = action
            .get("isPreferred")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut line = match kind {
            Some(k) if !k.is_empty() => format!("- [{k}] {title}"),
            _ => format!("- {title}"),
        };
        if preferred {
            line.push_str(" (preferred)");
        }
        if let Some(edit) = action.get("edit") {
            let preview = format_workspace_edit(edit, cwd);
            if let Some(first) = preview.lines().next()
                && first.starts_with("Rename plan (")
            {
                let count = first
                    .trim_start_matches("Rename plan (")
                    .trim_end_matches("):");
                let _ = write!(line, "  [{count}]");
            }
        } else if let Some(cmd) = action
            .pointer("/command/command")
            .or_else(|| action.get("command"))
            .and_then(Value::as_str)
        {
            let _ = write!(line, "  [cmd: {cmd}]");
        }
        lines.push(line);
    }
    if arr.len() > cap {
        lines.push(format!("... ({} more actions omitted)", arr.len() - cap));
    }
    if lines.is_empty() {
        "No code actions available at this position.".to_owned()
    } else {
        lines.join("\n")
    }
}

fn append_file_edits(out: &mut String, uri: &str, edits: &Value, cwd: &Path) -> usize {
    let Some(arr) = edits.as_array() else {
        return 0;
    };
    let label = uri_to_path(uri).map_or_else(|| uri.to_owned(), |p| display_rel(&p, cwd));
    let mut count = 0usize;
    for edit in arr {
        let line = edit
            .pointer("/range/start/line")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let col = edit
            .pointer("/range/start/character")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let _ = writeln!(out, "  {label}:{}:{} -> `{new_text}`", line + 1, col + 1);
        count += 1;
    }
    count
}

fn collect_symbol(val: &Value, cwd: &Path, depth: usize, out: &mut Vec<String>, cap: usize) {
    if out.len() >= cap {
        return;
    }
    let Some(name) = val.get("name").and_then(Value::as_str) else {
        return;
    };
    let kind = symbol_kind_name(val.get("kind").and_then(Value::as_u64).unwrap_or(0));
    let indent = "  ".repeat(depth);
    if let Some(uri) = val.pointer("/location/uri").and_then(Value::as_str) {
        let line = val
            .pointer("/location/range/start/line")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let rel = uri_to_path(uri).map_or_else(|| uri.to_owned(), |p| display_rel(&p, cwd));
        out.push(format!("{indent}[{kind}] {name}  ({rel}:{})", line + 1));
    } else {
        let line = val
            .pointer("/selectionRange/start/line")
            .or_else(|| val.pointer("/range/start/line"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        out.push(format!("{indent}[{kind}] {name}  (:{})", line + 1));
    }
    if let Some(children) = val.get("children").and_then(Value::as_array) {
        for child in children {
            collect_symbol(child, cwd, depth + 1, out, cap);
            if out.len() >= cap {
                break;
            }
        }
    }
}

fn extract_locations(val: &Value) -> Vec<(String, u32, u32)> {
    let mut out = Vec::new();
    match val {
        Value::Array(arr) => {
            for item in arr {
                if let Some(loc) = parse_one_location(item) {
                    out.push(loc);
                }
            }
        }
        Value::Object(_) => {
            if let Some(loc) = parse_one_location(val) {
                out.push(loc);
            }
        }
        _ => {}
    }
    out
}

fn parse_one_location(v: &Value) -> Option<(String, u32, u32)> {
    if let Some(uri) = v.get("uri").and_then(Value::as_str) {
        let line = u32::try_from(v.pointer("/range/start/line")?.as_u64()?).ok()?;
        let col = u32::try_from(v.pointer("/range/start/character")?.as_u64()?).ok()?;
        return Some((uri.to_owned(), line, col));
    }
    if let Some(uri) = v.get("targetUri").and_then(Value::as_str) {
        let line = u32::try_from(
            v.pointer("/targetSelectionRange/start/line")
                .or_else(|| v.pointer("/targetRange/start/line"))?
                .as_u64()?,
        )
        .ok()?;
        let col = u32::try_from(
            v.pointer("/targetSelectionRange/start/character")
                .or_else(|| v.pointer("/targetRange/start/character"))?
                .as_u64()?,
        )
        .ok()?;
        return Some((uri.to_owned(), line, col));
    }
    None
}

fn read_line_snippet(path: &Path, cwd: &Path, zero_based_line: u32) -> Option<String> {
    if !crate::lifecycle::is_path_within_workspace(path, cwd) {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let idx = usize::try_from(zero_based_line).ok()?;
    let line = text.lines().nth(idx)?.trim();
    if line.is_empty() {
        return None;
    }
    if line.len() > 100 {
        let cut = line.floor_char_boundary(100);
        Some(format!("{}...", line.get(..cut).unwrap_or("")))
    } else {
        Some(line.to_owned())
    }
}

pub(crate) fn display_rel(path: &Path, cwd: &Path) -> String {
    let p = path.strip_prefix(cwd).unwrap_or(path);
    p.to_string_lossy().replace('\\', "/")
}

fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "mod",
        3 => "ns",
        4 => "pkg",
        5 => "class",
        6 => "method",
        7 => "prop",
        8 => "field",
        9 => "ctor",
        10 => "enum",
        11 => "iface",
        12 => "fn",
        13 => "var",
        14 => "const",
        22 => "enum_member",
        23 => "struct",
        25 => "op",
        26 => "type_param",
        _ => "sym",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uri::path_to_uri;
    use hrn_kernel::serde_json::json;

    #[test]
    fn format_locations_omits_snippets_outside_workspace() {
        let root = std::env::temp_dir().join(format!("hrn-lsp-fmt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).expect("ws");
        let inside = ws.join("in.rs");
        let outside = root.join("secret.rs");
        std::fs::write(&inside, "fn inside_fn() {}\n").expect("in");
        std::fs::write(&outside, "const TOP_SECRET: &str = \"leak\";\n").expect("out");

        let val = json!([
            {
                "uri": path_to_uri(&inside),
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }
            },
            {
                "uri": path_to_uri(&outside),
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }
            }
        ]);
        let rendered = format_locations(&val, &ws, 10);
        assert!(rendered.contains("fn inside_fn()"), "{rendered}");
        assert!(
            !rendered.contains("TOP_SECRET"),
            "external file content must never be read into snippets: {rendered}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
