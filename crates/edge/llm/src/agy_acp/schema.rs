//! Strict `OpenAPI`-3.0 / Gemini-compatible MCP tool schema sanitizer and wire-name clamping.
//!
//! Antigravity (`agy`) compiles MCP tool `inputSchema` payloads into Gemini/Vertex
//! `GenerateContentRequest.tools[].function_declarations[].parameters` (which follows the strict
//! `OpenAPI` 3.0 `Schema` subset) and prefixes every tool name with `mcp_hrn-bridge_`, enforcing
//! `^[a-zA-Z0-9_-]{1,64}$`. This module ensures arbitrary third-party MCP tool schemas and long
//! tool names never trigger HTTP 400 `INVALID_ARGUMENT` or get silently dropped by `agy`.

use super::bridge::BRIDGE_TOOL_PREFIX;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fmt::Write as _;

/// `agy` prepends `mcp_<serverName>_` (`mcp_hrn-bridge_` = 15 bytes) before validating
/// `^[a-zA-Z0-9_-]{1,64}$`, leaving 49 bytes for the bridge wire name.
pub const MAX_BRIDGE_WIRE_NAME_LEN: usize = 64 - "mcp_hrn-bridge_".len();

const MAX_SCHEMA_DEPTH: usize = 16;

/// Deterministically map an `hrn` tool name to an `agy`-valid `hrn__*` MCP wire name
/// satisfying `mcp_hrn-bridge_<wire>` in `^[a-zA-Z0-9_-]{1,64}$`.
#[must_use]
pub fn wire_tool_name(raw_name: &str) -> String {
    let prefixed = if raw_name.starts_with(BRIDGE_TOOL_PREFIX) {
        raw_name.to_owned()
    } else {
        format!("{BRIDGE_TOOL_PREFIX}{raw_name}")
    };
    let valid_chars = prefixed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid_chars && prefixed.len() <= MAX_BRIDGE_WIRE_NAME_LEN {
        return prefixed;
    }

    let sanitized: String = prefixed
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
                char::from(b)
            } else {
                '_'
            }
        })
        .collect();
    let hash = fnv1a_32(raw_name.as_bytes());
    let keep = MAX_BRIDGE_WIRE_NAME_LEN.saturating_sub(9); // `_` + 8 hex chars
    let prefix = sanitized
        .get(..keep.min(sanitized.len()))
        .unwrap_or(&sanitized)
        .trim_end_matches('_');
    let mut out = String::with_capacity(MAX_BRIDGE_WIRE_NAME_LEN);
    out.push_str(if prefix.starts_with(BRIDGE_TOOL_PREFIX) {
        prefix
    } else {
        BRIDGE_TOOL_PREFIX
    });
    let _ = write!(out, "_{hash:08x}");
    out
}

fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in bytes {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Recursively sanitize an MCP tool `inputSchema` so Gemini/Vertex `GenerateContentRequest`
/// accepts it without HTTP 400 (`missing items`, `anyOf`/`oneOf`/`const`, type arrays, dangling
/// `required`, unsupported JSON Schema keywords).
#[must_use]
pub fn sanitize_tool_schema(schema: &Value) -> Value {
    let mut root = match sanitize_node(schema, 0) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    root.insert("type".to_owned(), Value::String("object".to_owned()));
    if !matches!(root.get("properties"), Some(Value::Object(_))) {
        root.insert("properties".to_owned(), Value::Object(Map::new()));
    }
    prune_required(&mut root);
    Value::Object(root)
}

fn sanitize_node(node: &Value, depth: usize) -> Value {
    if depth >= MAX_SCHEMA_DEPTH {
        return json!({ "type": "string" });
    }
    let Some(map) = node.as_object() else {
        return json!({ "type": "string" });
    };

    let outer_desc = map
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);

    if let Some(const_val) = map.get("const") {
        return sanitize_const(const_val, outer_desc);
    }

    if let Some(variants) = map
        .get("anyOf")
        .or_else(|| map.get("oneOf"))
        .and_then(Value::as_array)
    {
        return sanitize_union(variants, outer_desc, depth + 1);
    }

    if let Some(all_of) = map.get("allOf").and_then(Value::as_array) {
        return sanitize_all_of(all_of, outer_desc, depth + 1);
    }

    let (resolved_type, mut nullable) = resolve_type(map);
    if map.get("nullable").and_then(Value::as_bool) == Some(true) {
        nullable = true;
    }

    let mut out = Map::new();
    out.insert("type".to_owned(), Value::String(resolved_type.to_owned()));
    if nullable {
        out.insert("nullable".to_owned(), Value::Bool(true));
    }
    if let Some(title) = map.get("title").and_then(Value::as_str)
        && !title.is_empty()
    {
        out.insert("title".to_owned(), Value::String(title.to_owned()));
    }
    let mut desc = outer_desc;

    for key in [
        "format",
        "pattern",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
    ] {
        if let Some(v) = map.get(key)
            && (v.is_string() || v.is_number())
        {
            out.insert(key.to_owned(), v.clone());
        }
    }

    if let Some(enum_arr) = map.get("enum").and_then(Value::as_array) {
        apply_enum(&mut out, &mut desc, resolved_type, enum_arr);
    }

    match resolved_type {
        "array" => {
            for key in ["minItems", "maxItems"] {
                if let Some(v) = map.get(key).filter(|v| v.is_u64()) {
                    out.insert(key.to_owned(), v.clone());
                }
            }
            let items = match map.get("items") {
                Some(Value::Object(_)) => {
                    sanitize_node(map.get("items").unwrap_or(&Value::Null), depth + 1)
                }
                Some(Value::Array(tuple)) => tuple.first().map_or_else(
                    || json!({ "type": "string" }),
                    |f| sanitize_node(f, depth + 1),
                ),
                _ => json!({ "type": "string" }),
            };
            out.insert("items".to_owned(), items);
        }
        "object" => {
            if let Some(props) = map.get("properties").and_then(Value::as_object) {
                let mut clean_props = Map::new();
                for (k, v) in props {
                    clean_props.insert(k.clone(), sanitize_node(v, depth + 1));
                }
                if !clean_props.is_empty() {
                    out.insert("properties".to_owned(), Value::Object(clean_props));
                }
            }
            if let Some(req) = map.get("required").and_then(Value::as_array) {
                out.insert("required".to_owned(), Value::Array(req.clone()));
                prune_required(&mut out);
            }
        }
        _ => {}
    }

    if let Some(d) = desc.filter(|s| !s.is_empty()) {
        out.insert("description".to_owned(), Value::String(d));
    }
    Value::Object(out)
}

fn sanitize_const(val: &Value, mut desc: Option<String>) -> Value {
    let mut out = Map::new();
    match val {
        Value::String(s) => {
            out.insert("type".to_owned(), Value::String("string".to_owned()));
            out.insert(
                "enum".to_owned(),
                Value::Array(vec![Value::String(s.clone())]),
            );
        }
        Value::Number(n) => {
            let ty = if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            };
            out.insert("type".to_owned(), Value::String(ty.to_owned()));
            append_hint(&mut desc, &format!("Must be {n}."));
        }
        Value::Bool(b) => {
            out.insert("type".to_owned(), Value::String("boolean".to_owned()));
            append_hint(&mut desc, &format!("Must be {b}."));
        }
        Value::Null => {
            out.insert("type".to_owned(), Value::String("string".to_owned()));
            out.insert("nullable".to_owned(), Value::Bool(true));
        }
        Value::Array(_) => {
            out.insert("type".to_owned(), Value::String("array".to_owned()));
            out.insert("items".to_owned(), json!({ "type": "string" }));
        }
        Value::Object(_) => {
            out.insert("type".to_owned(), Value::String("object".to_owned()));
        }
    }
    if let Some(d) = desc.filter(|s| !s.is_empty()) {
        out.insert("description".to_owned(), Value::String(d));
    }
    Value::Object(out)
}

fn sanitize_union(variants: &[Value], outer_desc: Option<String>, depth: usize) -> Value {
    let mut nullable = false;
    let mut non_null: Vec<&Value> = Vec::new();
    for v in variants {
        if is_null_schema(v) {
            nullable = true;
        } else {
            non_null.push(v);
        }
    }
    if non_null.is_empty() {
        let mut out = json!({ "type": "string", "nullable": true });
        attach_outer_desc(&mut out, outer_desc);
        return out;
    }

    if let Some(collapsed) = sanitize_const_union(&non_null, nullable, outer_desc.as_deref()) {
        return collapsed;
    }

    if let [only] = non_null.as_slice() {
        let mut single = sanitize_node(only, depth);
        if nullable && let Some(obj) = single.as_object_mut() {
            obj.insert("nullable".to_owned(), Value::Bool(true));
        }
        attach_outer_desc(&mut single, outer_desc);
        return single;
    }

    if non_null
        .iter()
        .all(|v| v.as_object().is_some_and(|m| m.get("properties").is_some()))
    {
        return merge_object_union(&non_null, nullable, outer_desc, depth);
    }

    let mut chosen = non_null.first().map_or_else(
        || json!({ "type": "string" }),
        |first| sanitize_node(first, depth),
    );
    if nullable && let Some(obj) = chosen.as_object_mut() {
        obj.insert("nullable".to_owned(), Value::Bool(true));
    }
    attach_outer_desc(&mut chosen, outer_desc);
    chosen
}

fn sanitize_const_union(
    non_null: &[&Value],
    nullable: bool,
    outer_desc: Option<&str>,
) -> Option<Value> {
    if !non_null
        .iter()
        .all(|v| v.as_object().is_some_and(|m| m.contains_key("const")))
    {
        return None;
    }
    let consts: Vec<&Value> = non_null.iter().filter_map(|v| v.get("const")).collect();
    let mut desc = outer_desc.map(str::to_owned);
    let mut out = Map::new();
    if consts.iter().all(|v| v.is_string()) {
        let mut seen = HashSet::new();
        let enums: Vec<Value> = consts
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| seen.insert((*s).to_owned()))
            .map(|s| Value::String(s.to_owned()))
            .collect();
        out.insert("type".to_owned(), Value::String("string".to_owned()));
        if !enums.is_empty() {
            out.insert("enum".to_owned(), Value::Array(enums));
        }
    } else if consts.iter().all(|v| v.is_number()) {
        let ty = if consts.iter().all(|v| v.is_i64() || v.is_u64()) {
            "integer"
        } else {
            "number"
        };
        let list: Vec<String> = consts.iter().map(ToString::to_string).collect();
        append_hint(&mut desc, &format!("Allowed values: {}.", list.join(", ")));
        out.insert("type".to_owned(), Value::String(ty.to_owned()));
    } else {
        return None;
    }
    if nullable {
        out.insert("nullable".to_owned(), Value::Bool(true));
    }
    if let Some(d) = desc.filter(|s| !s.is_empty()) {
        out.insert("description".to_owned(), Value::String(d));
    }
    Some(Value::Object(out))
}

fn merge_object_union(
    non_null: &[&Value],
    nullable: bool,
    outer_desc: Option<String>,
    depth: usize,
) -> Value {
    let mut merged_props = Map::new();
    let mut common_req: Option<HashSet<String>> = None;
    for v in non_null {
        let clean = sanitize_node(v, depth);
        if let Some(obj) = clean.as_object() {
            if let Some(props) = obj.get("properties").and_then(Value::as_object) {
                for (k, prop_val) in props {
                    merge_property_schema(&mut merged_props, k, prop_val);
                }
            }
            let req_set: HashSet<String> = obj
                .get("required")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            common_req = Some(match common_req {
                Some(prev) => prev.intersection(&req_set).cloned().collect(),
                None => req_set,
            });
        }
    }
    let mut out = Map::new();
    out.insert("type".to_owned(), Value::String("object".to_owned()));
    out.insert("properties".to_owned(), Value::Object(merged_props));
    if let Some(reqs) = common_req
        && !reqs.is_empty()
    {
        let mut sorted: Vec<String> = reqs.into_iter().collect();
        sorted.sort();
        out.insert(
            "required".to_owned(),
            Value::Array(sorted.into_iter().map(Value::String).collect()),
        );
        prune_required(&mut out);
    }
    if nullable {
        out.insert("nullable".to_owned(), Value::Bool(true));
    }
    if let Some(d) = outer_desc.filter(|s| !s.is_empty()) {
        out.insert("description".to_owned(), Value::String(d));
    }
    Value::Object(out)
}

fn sanitize_all_of(parts: &[Value], outer_desc: Option<String>, depth: usize) -> Value {
    let mut merged_props = Map::new();
    let mut merged_req: Vec<String> = Vec::new();
    let mut saw_object = false;
    for part in parts {
        let clean = sanitize_node(part, depth);
        if let Some(obj) = clean.as_object()
            && obj.get("type").and_then(Value::as_str) == Some("object")
        {
            saw_object = true;
            if let Some(props) = obj.get("properties").and_then(Value::as_object) {
                for (k, v) in props {
                    merged_props.insert(k.clone(), v.clone());
                }
            }
            if let Some(reqs) = obj.get("required").and_then(Value::as_array) {
                for r in reqs.iter().filter_map(Value::as_str) {
                    if !merged_req.iter().any(|existing| existing == r) {
                        merged_req.push(r.to_owned());
                    }
                }
            }
        }
    }
    if saw_object {
        let mut out = Map::new();
        out.insert("type".to_owned(), Value::String("object".to_owned()));
        if !merged_props.is_empty() {
            out.insert("properties".to_owned(), Value::Object(merged_props));
        }
        if !merged_req.is_empty() {
            out.insert(
                "required".to_owned(),
                Value::Array(merged_req.into_iter().map(Value::String).collect()),
            );
            prune_required(&mut out);
        }
        if let Some(d) = outer_desc.filter(|s| !s.is_empty()) {
            out.insert("description".to_owned(), Value::String(d));
        }
        return Value::Object(out);
    }
    let mut first = parts
        .first()
        .map_or_else(|| json!({ "type": "string" }), |p| sanitize_node(p, depth));
    attach_outer_desc(&mut first, outer_desc);
    first
}

fn resolve_type(map: &Map<String, Value>) -> (&'static str, bool) {
    match map.get("type") {
        Some(Value::String(s)) => (normalize_type_name(s), false),
        Some(Value::Array(arr)) => {
            let mut nullable = false;
            let mut picked = None;
            for item in arr {
                if let Some(s) = item.as_str() {
                    if s.eq_ignore_ascii_case("null") {
                        nullable = true;
                    } else if picked.is_none() {
                        picked = Some(normalize_type_name(s));
                    }
                }
            }
            (picked.unwrap_or("string"), nullable)
        }
        _ => {
            if map.contains_key("properties") {
                ("object", false)
            } else if map.contains_key("items") {
                ("array", false)
            } else {
                ("string", false)
            }
        }
    }
}

fn normalize_type_name(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "object" => "object",
        "array" => "array",
        "integer" => "integer",
        "number" => "number",
        "boolean" => "boolean",
        _ => "string",
    }
}

fn is_null_schema(v: &Value) -> bool {
    v.as_object().is_some_and(|m| {
        m.get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.eq_ignore_ascii_case("null"))
            || matches!(m.get("const"), Some(Value::Null))
    })
}

fn apply_enum(
    out: &mut Map<String, Value>,
    desc: &mut Option<String>,
    resolved_type: &str,
    enum_arr: &[Value],
) {
    if resolved_type == "string" {
        let mut seen = HashSet::new();
        let strings: Vec<Value> = enum_arr
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| seen.insert((*s).to_owned()))
            .map(|s| Value::String(s.to_owned()))
            .collect();
        if !strings.is_empty() {
            out.insert("enum".to_owned(), Value::Array(strings));
        }
    } else if !enum_arr.is_empty() && enum_arr.len() <= 16 {
        let vals: Vec<String> = enum_arr.iter().map(ToString::to_string).collect();
        append_hint(desc, &format!("Allowed values: {}.", vals.join(", ")));
    }
}

fn prune_required(obj: &mut Map<String, Value>) {
    let valid_keys: HashSet<String> = obj
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default();
    let Some(req_arr) = obj.get("required").and_then(Value::as_array) else {
        return;
    };
    let mut seen = HashSet::new();
    let filtered: Vec<Value> = req_arr
        .iter()
        .filter_map(Value::as_str)
        .filter(|k| valid_keys.contains(*k) && seen.insert((*k).to_owned()))
        .map(|k| Value::String(k.to_owned()))
        .collect();
    if filtered.is_empty() {
        obj.remove("required");
    } else {
        obj.insert("required".to_owned(), Value::Array(filtered));
    }
}

fn attach_outer_desc(target: &mut Value, outer_desc: Option<String>) {
    if let Some(d) = outer_desc.filter(|s| !s.is_empty())
        && let Some(obj) = target.as_object_mut()
        && !obj.contains_key("description")
    {
        obj.insert("description".to_owned(), Value::String(d));
    }
}

fn append_hint(desc: &mut Option<String>, hint: &str) {
    match desc {
        Some(existing) if !existing.trim().is_empty() => {
            if !existing.contains(hint) {
                existing.push(' ');
                existing.push_str(hint);
            }
        }
        _ => *desc = Some(hint.to_owned()),
    }
}

fn merge_property_schema(target: &mut Map<String, Value>, key: &str, incoming: &Value) {
    let Some(existing) = target.get_mut(key) else {
        target.insert(key.to_owned(), incoming.clone());
        return;
    };
    if let (Some(ex_enum), Some(in_enum)) = (
        existing.get_mut("enum").and_then(Value::as_array_mut),
        incoming.get("enum").and_then(Value::as_array),
    ) {
        for v in in_enum {
            if !ex_enum.contains(v) {
                ex_enum.push(v.clone());
            }
        }
    }
}
