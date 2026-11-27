//! Dependency-free recursive JSON Schema validator for structured subagent `yield` outputs.
//!
//! Supports `type` (scalar or array of types), `required`, `properties`, `additionalProperties`,
//! `items` (schema or tuple array), `enum`, `const`, `minLength`, `maxLength`, `minimum`,
//! `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `minItems`, `maxItems`, `anyOf`, `oneOf`,
//! `allOf`, and boolean schemas.

use serde_json::Value;

/// Maximum recursive validation depth to guard against pathological nested schemas.
const MAX_DEPTH: usize = 64;

/// Validate `value` against `schema`, returning a list of human-readable validation errors
/// (empty when `value` conforms to `schema`).
#[must_use]
pub fn validate_json(schema: &Value, value: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    validate_at(schema, value, "$", 0, &mut errors);
    errors
}

fn validate_at(schema: &Value, value: &Value, path: &str, depth: usize, errors: &mut Vec<String>) {
    if depth >= MAX_DEPTH {
        errors.push(format!(
            "{path}: exceeded maximum schema depth ({MAX_DEPTH})"
        ));
        return;
    }
    match schema {
        Value::Bool(true) => {}
        Value::Bool(false) => {
            errors.push(format!("{path}: schema forbids any value"));
        }
        Value::Object(obj) => {
            validate_type_and_const(obj, value, path, errors);
            validate_scalars(obj, value, path, errors);
            validate_containers(obj, value, path, depth, errors);
            validate_combinators(obj, value, path, depth, errors);
        }
        _ => {
            errors.push(format!(
                "{path}: invalid schema node (expected object or boolean)"
            ));
        }
    }
}

fn validate_type_and_const(
    obj: &serde_json::Map<String, Value>,
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    if let Some(ty) = obj.get("type") {
        match ty {
            Value::String(expected) => {
                if !matches_type(value, expected) {
                    errors.push(format!(
                        "{path}: expected type `{expected}`, got `{}`",
                        value_type_name(value)
                    ));
                }
            }
            Value::Array(list) => {
                let names: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
                if !names.is_empty() && !names.iter().any(|t| matches_type(value, t)) {
                    errors.push(format!(
                        "{path}: expected one of types [{}], got `{}`",
                        names.join(", "),
                        value_type_name(value)
                    ));
                }
            }
            _ => {}
        }
    }
    if let Some(expected) = obj.get("const")
        && !json_eq(value, expected)
    {
        errors.push(format!("{path}: must equal constant `{expected}`"));
    }
    if let Some(Value::Array(allowed)) = obj.get("enum")
        && !allowed.iter().any(|item| json_eq(value, item))
    {
        let preview: Vec<String> = allowed.iter().map(Value::to_string).collect();
        errors.push(format!(
            "{path}: value `{value}` is not in enum [{}]",
            preview.join(", ")
        ));
    }
}

fn validate_scalars(
    obj: &serde_json::Map<String, Value>,
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    if let Value::String(s) = value {
        let len = s.chars().count();
        if let Some(min) = obj.get("minLength").and_then(Value::as_u64)
            && usize::try_from(min).is_ok_and(|m| len < m)
        {
            errors.push(format!(
                "{path}: string length {len} is less than minLength {min}"
            ));
        }
        if let Some(max) = obj.get("maxLength").and_then(Value::as_u64)
            && usize::try_from(max).is_ok_and(|m| len > m)
        {
            errors.push(format!(
                "{path}: string length {len} exceeds maxLength {max}"
            ));
        }
    }
    if let Some(num) = value.as_f64() {
        if let Some(min) = obj.get("minimum").and_then(Value::as_f64)
            && num < min
        {
            errors.push(format!("{path}: number {num} is less than minimum {min}"));
        }
        if let Some(max) = obj.get("maximum").and_then(Value::as_f64)
            && num > max
        {
            errors.push(format!("{path}: number {num} exceeds maximum {max}"));
        }
        if let Some(ex_min) = obj.get("exclusiveMinimum").and_then(Value::as_f64)
            && num <= ex_min
        {
            errors.push(format!(
                "{path}: number {num} must be greater than exclusiveMinimum {ex_min}"
            ));
        }
        if let Some(ex_max) = obj.get("exclusiveMaximum").and_then(Value::as_f64)
            && num >= ex_max
        {
            errors.push(format!(
                "{path}: number {num} must be less than exclusiveMaximum {ex_max}"
            ));
        }
    }
}

fn validate_containers(
    obj: &serde_json::Map<String, Value>,
    value: &Value,
    path: &str,
    depth: usize,
    errors: &mut Vec<String>,
) {
    if let Value::Object(map) = value {
        if let Some(Value::Array(req)) = obj.get("required") {
            for key in req.iter().filter_map(Value::as_str) {
                if !map.contains_key(key) {
                    errors.push(format!("{path}: missing required property `{key}`"));
                }
            }
        }
        let props = obj.get("properties").and_then(Value::as_object);
        if let Some(props) = props {
            for (k, prop_schema) in props {
                if let Some(prop_val) = map.get(k) {
                    let child_path = format!("{path}.{k}");
                    validate_at(prop_schema, prop_val, &child_path, depth + 1, errors);
                }
            }
        }
        if let Some(additional) = obj.get("additionalProperties") {
            for (k, v) in map {
                let declared = props.is_some_and(|p| p.contains_key(k));
                if declared {
                    continue;
                }
                match additional {
                    Value::Bool(false) => {
                        errors.push(format!("{path}: unexpected additional property `{k}`"));
                    }
                    Value::Bool(true) => {}
                    sub_schema => {
                        let child_path = format!("{path}.{k}");
                        validate_at(sub_schema, v, &child_path, depth + 1, errors);
                    }
                }
            }
        }
    }
    if let Value::Array(arr) = value {
        if let Some(min) = obj.get("minItems").and_then(Value::as_u64)
            && usize::try_from(min).is_ok_and(|m| arr.len() < m)
        {
            errors.push(format!(
                "{path}: array length {} is less than minItems {min}",
                arr.len()
            ));
        }
        if let Some(max) = obj.get("maxItems").and_then(Value::as_u64)
            && usize::try_from(max).is_ok_and(|m| arr.len() > m)
        {
            errors.push(format!(
                "{path}: array length {} exceeds maxItems {max}",
                arr.len()
            ));
        }
        if let Some(items) = obj.get("items") {
            match items {
                Value::Array(tuple_schemas) => {
                    for (idx, item_schema) in tuple_schemas.iter().enumerate() {
                        if let Some(elem) = arr.get(idx) {
                            let child_path = format!("{path}[{idx}]");
                            validate_at(item_schema, elem, &child_path, depth + 1, errors);
                        }
                    }
                }
                item_schema => {
                    for (idx, elem) in arr.iter().enumerate() {
                        let child_path = format!("{path}[{idx}]");
                        validate_at(item_schema, elem, &child_path, depth + 1, errors);
                    }
                }
            }
        }
    }
}

fn validate_combinators(
    obj: &serde_json::Map<String, Value>,
    value: &Value,
    path: &str,
    depth: usize,
    errors: &mut Vec<String>,
) {
    if let Some(Value::Array(subs)) = obj.get("allOf") {
        for sub in subs {
            validate_at(sub, value, path, depth + 1, errors);
        }
    }
    if let Some(Value::Array(subs)) = obj.get("anyOf") {
        let matched = subs.iter().any(|sub| {
            let mut sub_errs = Vec::new();
            validate_at(sub, value, path, depth + 1, &mut sub_errs);
            sub_errs.is_empty()
        });
        if !matched {
            errors.push(format!("{path}: value did not match any branch of `anyOf`"));
        }
    }
    if let Some(Value::Array(subs)) = obj.get("oneOf") {
        let count = subs
            .iter()
            .filter(|sub| {
                let mut sub_errs = Vec::new();
                validate_at(sub, value, path, depth + 1, &mut sub_errs);
                sub_errs.is_empty()
            })
            .count();
        if count != 1 {
            errors.push(format!(
                "{path}: value matched {count} branches of `oneOf` (expected exactly 1)"
            ));
        }
    }
}

fn matches_type(value: &Value, expected: &str) -> bool {
    match expected {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "number" => value.is_number(),
        "integer" => match value {
            Value::Number(n) => {
                n.is_i64()
                    || n.is_u64()
                    || n.as_f64()
                        .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
            }
            _ => false,
        },
        _ => true,
    }
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// JSON equality treating mathematically equal numbers (`1` == `1.0`) as equal.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(na), Value::Number(nb)) => {
            na == nb || (na.as_f64().is_some() && na.as_f64() == nb.as_f64())
        }
        (Value::Array(aa), Value::Array(ab)) => {
            aa.len() == ab.len() && aa.iter().zip(ab.iter()).all(|(x, y)| json_eq(x, y))
        }
        (Value::Object(oa), Value::Object(ob)) => {
            oa.len() == ob.len()
                && oa
                    .iter()
                    .all(|(k, va)| ob.get(k).is_some_and(|vb| json_eq(va, vb)))
        }
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_objects_types_required_and_additional_properties() {
        let schema = json!({
            "type": "object",
            "required": ["ok", "count"],
            "properties": {
                "ok": { "type": "boolean" },
                "count": { "type": "integer", "minimum": 1, "maximum": 10 },
                "note": { "type": ["string", "null"], "minLength": 2, "maxLength": 5 }
            },
            "additionalProperties": false
        });
        assert!(validate_json(&schema, &json!({"ok": true, "count": 3})).is_empty());
        assert!(
            validate_json(&schema, &json!({"ok": false, "count": 10, "note": null})).is_empty()
        );
        assert!(validate_json(&schema, &json!({"ok": true, "count": 1, "note": "hi"})).is_empty());

        let errs = validate_json(
            &schema,
            &json!({"ok": "yes", "count": 1.5, "note": "x", "extra": 1}),
        );
        assert_eq!(errs.len(), 4, "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("$.ok")));
        assert!(errs.iter().any(|e| e.contains("$.count")));
        assert!(errs.iter().any(|e| e.contains("$.note")));
        assert!(
            errs.iter()
                .any(|e| e.contains("additional property `extra`"))
        );

        let missing = validate_json(&schema, &json!({"ok": true}));
        assert!(
            missing
                .iter()
                .any(|e| e.contains("required property `count`"))
        );
    }

    #[test]
    fn validates_arrays_enum_const_and_combinators() {
        let schema = json!({
            "type": "object",
            "properties": {
                "status": { "enum": ["pass", "fail"] },
                "version": { "const": 2 },
                "tags": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 3,
                    "items": { "type": "string" }
                },
                "choice": {
                    "oneOf": [
                        { "type": "integer", "maximum": 5 },
                        { "type": "integer", "minimum": 10 }
                    ]
                },
                "either": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "boolean" }
                    ]
                },
                "both": {
                    "allOf": [
                        { "type": "number", "minimum": 0 },
                        { "maximum": 100 }
                    ]
                }
            },
            "required": ["status", "version", "tags"]
        });

        let good = json!({
            "status": "pass",
            "version": 2.0,
            "tags": ["a", "b"],
            "choice": 12,
            "either": true,
            "both": 50
        });
        assert!(validate_json(&schema, &good).is_empty());

        let bad = json!({
            "status": "maybe",
            "version": 3,
            "tags": [],
            "choice": 7,
            "either": 42,
            "both": 150
        });
        let errs = validate_json(&schema, &bad);
        assert!(errs.len() >= 6, "{errs:?}");
    }
}
