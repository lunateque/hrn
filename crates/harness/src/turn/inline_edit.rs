//! Recover accidental inline edit blocks when a model emits raw patch markup in assistant prose
//! instead of invoking the `edit` / `apply_patch` tool (`[edit] recover_inline_edits = true`).

use hrn_kernel::{ToolCall, ToolSpec};
use serde_json::{Value, json};

/// Scan assistant `text` for accidental inline edit markup matching the active edit tool schema.
///
/// Only emits calls for tools actually present in `available_tools` (respecting Plan mode and
/// `[edit] mode = "none"`).
pub(super) fn recover_calls(
    text: &str,
    available_tools: &[ToolSpec],
    mut next_id: impl FnMut() -> String,
) -> Vec<ToolCall> {
    let edit_spec = available_tools.iter().find(|t| t.name == "edit");
    let has_apply_patch = available_tools.iter().any(|t| t.name == "apply_patch");
    let edit_accepts_input = edit_spec.is_some_and(|s| has_prop(s, "input"));
    let edit_accepts_replace =
        edit_spec.is_some_and(|s| has_prop(s, "path") && has_prop(s, "old") && has_prop(s, "new"));
    let edit_accepts_patch = edit_spec.is_some_and(|s| has_prop(s, "patch"));

    if !edit_accepts_input && !edit_accepts_replace && !edit_accepts_patch && !has_apply_patch {
        return Vec::new();
    }

    let blocks = extract_candidate_blocks(text);
    let mut calls = Vec::new();
    for (lang, body) in blocks {
        if edit_accepts_input && looks_like_hashline(&body) {
            calls.push(ToolCall {
                id: next_id(),
                name: "edit".to_owned(),
                arguments: json!({ "input": body }).to_string(),
                signature: None,
            });
            continue;
        }
        if (edit_accepts_patch || has_apply_patch) && looks_like_codex_patch(&body) {
            let name = if edit_accepts_patch {
                "edit"
            } else {
                "apply_patch"
            };
            calls.push(ToolCall {
                id: next_id(),
                name: name.to_owned(),
                arguments: json!({ "patch": body }).to_string(),
                signature: None,
            });
            continue;
        }
        if edit_accepts_replace
            && (lang.is_empty()
                || lang.eq_ignore_ascii_case("json")
                || lang.eq_ignore_ascii_case("edit"))
            && let Some(args) = parse_replace_json(&body)
        {
            calls.push(ToolCall {
                id: next_id(),
                name: "edit".to_owned(),
                arguments: args.to_string(),
                signature: None,
            });
        }
    }
    calls
}

fn has_prop(spec: &ToolSpec, prop: &str) -> bool {
    spec.parameters
        .pointer(&format!("/properties/{prop}"))
        .is_some_and(Value::is_object)
}

/// Extract fenced code blocks (` ```lang ... ``` `) and standalone `*** Begin Patch ... *** End Patch`
/// regions from assistant prose.
fn extract_candidate_blocks(text: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0usize;
    while let Some(&line) = lines.get(i) {
        let trimmed = line.trim();
        if let Some(after_ticks) = trimmed.strip_prefix("```") {
            let lang = after_ticks.trim().to_owned();
            let mut body = Vec::new();
            i += 1;
            let mut closed = false;
            while let Some(&inner) = lines.get(i) {
                if inner.trim().starts_with("```") {
                    closed = true;
                    i += 1;
                    break;
                }
                body.push(inner);
                i += 1;
            }
            if closed && !body.is_empty() {
                blocks.push((lang, body.join("\n")));
            }
            continue;
        }
        if trimmed == "*** Begin Patch" {
            let mut body = vec![trimmed];
            i += 1;
            let mut closed = false;
            while let Some(&inner) = lines.get(i) {
                body.push(inner);
                i += 1;
                if inner.trim() == "*** End Patch" {
                    closed = true;
                    break;
                }
            }
            if closed {
                blocks.push((String::new(), body.join("\n")));
            }
            continue;
        }
        i += 1;
    }
    blocks
}

fn looks_like_hashline(body: &str) -> bool {
    let mut saw_header = false;
    let mut saw_op = false;
    for line in body.lines().map(str::trim) {
        if let Some(inner) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']'))
            && let Some((path, tag)) = inner.rsplit_once('#')
            && !path.trim().is_empty()
            && tag.len() == 4
            && tag.bytes().all(|b| b.is_ascii_hexdigit())
        {
            saw_header = true;
            continue;
        }
        if saw_header
            && (line.starts_with("PUT ")
                || line.starts_with("CUT ")
                || line == "REM"
                || line.starts_with("MV "))
        {
            saw_op = true;
        }
    }
    saw_header && saw_op
}

fn looks_like_codex_patch(body: &str) -> bool {
    let has_begin = body.lines().any(|l| l.trim() == "*** Begin Patch");
    let has_end = body.lines().any(|l| l.trim() == "*** End Patch");
    let has_directive = body.lines().map(str::trim).any(|l| {
        l.starts_with("*** Update File: ")
            || l.starts_with("*** Add File: ")
            || l.starts_with("*** Delete File: ")
    });
    has_begin && has_end && has_directive
}

fn parse_replace_json(body: &str) -> Option<Value> {
    let val: Value = serde_json::from_str(body.trim()).ok()?;
    let obj = val.as_object()?;
    let path = obj.get("path")?.as_str()?;
    let old = obj.get("old")?.as_str()?;
    let new = obj.get("new")?.as_str()?;
    if path.trim().is_empty() || old.is_empty() {
        return None;
    }
    let mut out = json!({
        "path": path,
        "old": old,
        "new": new,
    });
    if let Some(all) = obj.get("replace_all").and_then(Value::as_bool)
        && let Some(map) = out.as_object_mut()
    {
        map.insert("replace_all".to_owned(), Value::Bool(all));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashline_spec() -> ToolSpec {
        ToolSpec {
            name: "edit".to_owned(),
            description: String::new(),
            parameters: json!({
                "type": "object",
                "properties": { "input": { "type": "string" } }
            }),
        }
    }

    fn replace_spec() -> ToolSpec {
        ToolSpec {
            name: "edit".to_owned(),
            description: String::new(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old": { "type": "string" },
                    "new": { "type": "string" }
                }
            }),
        }
    }

    #[test]
    fn recovers_hashline_and_replace_blocks_and_ignores_plain_prose() {
        let mut n = 0usize;
        let mut id = || {
            n += 1;
            format!("c{n}")
        };
        let prose = "I will update `[src/lib.rs#1A2B]` in a moment, no patch yet.";
        assert!(recover_calls(prose, &[hashline_spec()], &mut id).is_empty());

        let hashline_msg =
            "Applying change:\n```\n[src/lib.rs#1A2B]\nPUT 2.=2:\n+let x = 2;\n```\n";
        let recovered = recover_calls(hashline_msg, &[hashline_spec()], &mut id);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].name, "edit");
        let parsed: Value = serde_json::from_str(&recovered[0].arguments).unwrap();
        assert!(
            parsed["input"]
                .as_str()
                .is_some_and(|s| s.contains("PUT 2.=2:"))
        );

        let replace_msg = "```json\n{\"path\":\"a.rs\",\"old\":\"a\",\"new\":\"b\"}\n```";
        let rec_rep = recover_calls(replace_msg, &[replace_spec()], &mut id);
        assert_eq!(rec_rep.len(), 1);
        let parsed_rep: Value = serde_json::from_str(&rec_rep[0].arguments).unwrap();
        assert_eq!(parsed_rep["path"], "a.rs");

        // When edit tool is absent (e.g. Plan mode or mode = none), nothing is recovered.
        assert!(recover_calls(hashline_msg, &[], &mut id).is_empty());
    }
}
