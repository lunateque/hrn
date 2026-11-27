//! Token-efficient accessibility tree snapshot builder (`[role] "name" @e1`).
//!
//! Prunes ignored/empty layout wrappers and assigns short deterministic `@eN` refs mapped to
//! CDP `backendDOMNodeId`s so the agent can click/fill elements without dumping megabytes of HTML.

use hrn_kernel::serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Default maximum number of rendered accessibility lines per snapshot.
pub(crate) const DEFAULT_MAX_NODES: usize = 250;

/// Rendered accessibility tree plus ref-to-`backendDOMNodeId` lookup table.
#[derive(Clone, Debug, Default)]
pub(crate) struct AxSnapshot {
    /// Compact text representation for the LLM.
    pub(crate) text: String,
    /// Map from `"@e1"`, `"@e2"`, ... to CDP `backendDOMNodeId`.
    pub(crate) refs: BTreeMap<String, i64>,
}

struct AxNode<'a> {
    role: &'a str,
    name: &'a str,
    value: &'a str,
    ignored: bool,
    backend_id: Option<i64>,
    children: Vec<&'a str>,
    states: Vec<String>,
}

/// Convert the CDP `Accessibility.getFullAXTree` result into a compact [`AxSnapshot`].
pub(crate) fn build_snapshot(tree: &Value, max_nodes: usize) -> AxSnapshot {
    let Some(raw_nodes) = tree.get("nodes").and_then(Value::as_array) else {
        return AxSnapshot {
            text: "(empty accessibility tree)".to_owned(),
            refs: BTreeMap::new(),
        };
    };

    let mut by_id: BTreeMap<&str, AxNode<'_>> = BTreeMap::new();
    let mut order: Vec<&str> = Vec::with_capacity(raw_nodes.len());
    for raw in raw_nodes {
        let Some(node_id) = raw.get("nodeId").and_then(Value::as_str) else {
            continue;
        };
        let role = raw
            .pointer("/role/value")
            .and_then(Value::as_str)
            .unwrap_or("none");
        let name = raw
            .pointer("/name/value")
            .and_then(Value::as_str)
            .unwrap_or("");
        let value = raw
            .pointer("/value/value")
            .and_then(Value::as_str)
            .unwrap_or("");
        let ignored = raw.get("ignored").and_then(Value::as_bool).unwrap_or(false);
        let backend_id = raw.get("backendDOMNodeId").and_then(Value::as_i64);
        let children = raw
            .get("childIds")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let states = extract_states(raw);
        order.push(node_id);
        by_id.insert(
            node_id,
            AxNode {
                role,
                name,
                value,
                ignored,
                backend_id,
                children,
                states,
            },
        );
    }

    let Some(&root_id) = order.first() else {
        return AxSnapshot {
            text: "(empty accessibility tree)".to_owned(),
            refs: BTreeMap::new(),
        };
    };

    let mut out = String::new();
    let mut refs = BTreeMap::new();
    let mut visited = BTreeSet::new();
    let mut count = 0usize;
    let mut next_ref = 1usize;
    let limit = max_nodes.clamp(10, 1000);

    walk(
        root_id,
        0,
        &by_id,
        &mut visited,
        &mut out,
        &mut refs,
        &mut count,
        &mut next_ref,
        limit,
    );

    if count >= limit {
        let _ = writeln!(
            out,
            "... (truncated at {limit} nodes; pass higher `max_nodes` if needed)"
        );
    }
    let text = out.trim_end().to_owned();
    AxSnapshot {
        text: if text.is_empty() {
            "(no visible accessibility nodes)".to_owned()
        } else {
            text
        },
        refs,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "recursive tree walk passes mutable accumulators directly"
)]
fn walk<'a>(
    node_id: &'a str,
    depth: usize,
    by_id: &BTreeMap<&'a str, AxNode<'a>>,
    visited: &mut BTreeSet<&'a str>,
    out: &mut String,
    refs: &mut BTreeMap<String, i64>,
    count: &mut usize,
    next_ref: &mut usize,
    limit: usize,
) {
    if *count >= limit || !visited.insert(node_id) {
        return;
    }
    let Some(node) = by_id.get(node_id) else {
        return;
    };

    let interactive = is_interactive_role(node.role);
    let trimmed_name = clean_text(node.name, 120);
    let trimmed_val = clean_text(node.value, 80);

    let emit = !node.ignored
        && !is_noise_role(node.role)
        && (interactive || !trimmed_name.is_empty() || !trimmed_val.is_empty());

    let next_depth = if emit {
        *count += 1;
        let indent = "  ".repeat(depth.min(8));
        let _ = write!(out, "{indent}[{}]", node.role);
        if !trimmed_name.is_empty() {
            let _ = write!(out, " \"{trimmed_name}\"");
        }
        if !trimmed_val.is_empty() && trimmed_val != trimmed_name {
            let _ = write!(out, " value=\"{trimmed_val}\"");
        }
        for st in &node.states {
            let _ = write!(out, " ({st})");
        }
        if interactive && let Some(backend) = node.backend_id {
            let tag = format!("@e{next_ref}");
            *next_ref += 1;
            let _ = write!(out, " {tag}");
            refs.insert(tag, backend);
        }
        out.push('\n');
        depth + 1
    } else {
        depth
    };

    // Don't repeat raw StaticText child when its parent already displayed the exact same label.
    for child_id in &node.children {
        if let Some(child) = by_id.get(child_id)
            && child.role == "StaticText"
            && !trimmed_name.is_empty()
            && clean_text(child.name, 120) == trimmed_name
        {
            continue;
        }
        walk(
            child_id, next_depth, by_id, visited, out, refs, count, next_ref, limit,
        );
        if *count >= limit {
            break;
        }
    }
}

fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "checkbox"
            | "radio"
            | "switch"
            | "tab"
            | "menuitem"
            | "option"
            | "slider"
            | "spinbutton"
            | "listbox"
    )
}

fn is_noise_role(role: &str) -> bool {
    matches!(
        role,
        "none"
            | "generic"
            | "GenericContainer"
            | "InlineTextBox"
            | "LineBreak"
            | "LayoutTable"
            | "LayoutTableRow"
            | "LayoutTableCell"
            | "Ignored"
    )
}

fn extract_states(raw: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(props) = raw.get("properties").and_then(Value::as_array) else {
        return out;
    };
    for prop in props {
        let Some(name) = prop.get("name").and_then(Value::as_str) else {
            continue;
        };
        let val = prop.pointer("/value/value");
        match name {
            "disabled" | "focused" | "required" | "selected" if val == Some(&Value::Bool(true)) => {
                out.push(name.to_owned());
            }
            "checked" | "expanded" => {
                if val == Some(&Value::Bool(true)) || val.and_then(Value::as_str) == Some("true") {
                    out.push(name.to_owned());
                }
            }
            "level" => {
                if let Some(n) = val.and_then(Value::as_u64) {
                    out.push(format!("h{n}"));
                }
            }
            _ => {}
        }
    }
    out
}

fn clean_text(input: &str, max_chars: usize) -> String {
    let collapsed = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let escaped = collapsed.replace('"', "'");
    match escaped.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}...", escaped.get(..cut).unwrap_or_default()),
        None => escaped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::serde_json::json;

    #[test]
    fn formats_compact_tree_with_refs_and_prunes_wrappers() {
        let tree = json!({
            "nodes": [
                { "nodeId": "1", "role": {"value": "RootWebArea"}, "name": {"value": "Login"}, "childIds": ["2"] },
                { "nodeId": "2", "role": {"value": "generic"}, "childIds": ["3", "4", "5"] },
                { "nodeId": "3", "role": {"value": "textbox"}, "name": {"value": "Email"}, "backendDOMNodeId": 101, "properties": [{"name": "focused", "value": {"value": true}}] },
                { "nodeId": "4", "role": {"value": "button"}, "name": {"value": "Sign in"}, "backendDOMNodeId": 102, "childIds": ["6"] },
                { "nodeId": "5", "role": {"value": "generic"}, "ignored": true },
                { "nodeId": "6", "role": {"value": "StaticText"}, "name": {"value": "Sign in"} }
            ]
        });
        let snap = build_snapshot(&tree, 100);
        assert!(
            snap.text.contains("[textbox] \"Email\" (focused) @e1"),
            "{}",
            snap.text
        );
        assert!(
            snap.text.contains("[button] \"Sign in\" @e2"),
            "{}",
            snap.text
        );
        assert!(!snap.text.contains("[generic]"), "{}", snap.text);
        assert!(!snap.text.contains("StaticText"), "{}", snap.text);
        assert_eq!(snap.refs.get("@e1"), Some(&101));
        assert_eq!(snap.refs.get("@e2"), Some(&102));
    }
}
