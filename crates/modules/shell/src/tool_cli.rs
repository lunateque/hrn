//! `hrn tool <name> …`: turning a shell command line into the JSON arguments of a session tool,
//! and its schema back into a `--help` page. The contract is the reference harness's tool
//! commands: required top-level scalar/enum fields are positionals in schema order, every field
//! also takes `--flag value` (underscores become dashes, a nested field is `--a.b`), a boolean is
//! `--flag`/`--no-flag`, an array flag repeats, `-j`/`--json <JSON>` merges a raw object, and
//! `@FILE`/`-` merges a JSON object or binds its text to the next positional. A usage error exits
//! `2`, the tool's own failure exits `1`.
//!
//! `@FILE` and `-` are read by the caller (only the shell knows the directory they are relative
//! to) and handed to [`parse`] keyed by their position in `argv`.

use hrn_kernel::serde_json::{self, Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// What the shell line asked for.
pub(crate) enum Invocation {
    /// Run the tool with these arguments.
    Call(Value),
    /// `--help`: print the usage page instead.
    Help,
}

/// How the value of one schema node is shaped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Scalar,
    Boolean,
    Array,
    Object,
    Other,
}

fn shape_of(schema: &Value) -> Shape {
    if schema.get("enum").is_some() {
        return Shape::Scalar;
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string" | "number" | "integer") => Shape::Scalar,
        Some("boolean") => Shape::Boolean,
        Some("array") => Shape::Array,
        Some("object") => Shape::Object,
        // A combinator (`oneOf`, …) with no top-level `type`: take the value as given.
        _ => Shape::Other,
    }
}

fn properties(schema: &Value) -> Option<&Map<String, Value>> {
    schema.get("properties").and_then(Value::as_object)
}

/// The schema node addressed by a dotted path (`a.b`), or `None`.
fn node_at<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut node = root;
    for segment in path.split('.') {
        node = properties(node)?.get(segment)?;
    }
    Some(node)
}

/// The top-level field a dotted path starts at.
fn head_of(path: &str) -> &str {
    path.split('.').next().unwrap_or(path)
}

/// Required top-level scalar/enum fields, in schema order: the positional slots.
fn positionals(schema: &Value) -> Vec<String> {
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return Vec::new();
    };
    required
        .iter()
        .filter_map(Value::as_str)
        .filter(|name| {
            properties(schema)
                .and_then(|p| p.get(*name))
                .is_some_and(|s| matches!(shape_of(s), Shape::Scalar))
        })
        .map(str::to_owned)
        .collect()
}

/// A `KEY=VALUE,KEY=VALUE` text as a JSON object (what an object flag wants).
fn split_pairs(text: &str) -> Value {
    let mut map = Map::new();
    for pair in text.split(',') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(
                k.trim().to_owned(),
                serde_json::from_str(v.trim()).unwrap_or_else(|_| json!(v.trim())),
            );
        }
    }
    Value::Object(map)
}

/// Turn flag or positional text into the value the schema node wants.
fn coerce(node: &Value, text: &str) -> Value {
    match shape_of(node) {
        Shape::Scalar => serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
        Shape::Boolean if matches!(text, "true" | "false") => json!(text == "true"),
        Shape::Object => split_pairs(text),
        _ => json!(text),
    }
}

/// Set `path` in `args`: arrays append, dotted paths descend, `k=v` text becomes an object.
fn set_field(args: &mut Map<String, Value>, schema: &Value, path: &str, value: Value) {
    let node = node_at(schema, path);
    if let Some(rest) = path.split_once('.').map(|(_, rest)| rest) {
        let mut inner = match args.remove(head_of(path)) {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        set_field(&mut inner, node.unwrap_or(&Value::Null), rest, value);
        args.insert(head_of(path).to_owned(), Value::Object(inner));
        return;
    }
    let value = match (node.map_or(Shape::Other, shape_of), value) {
        (Shape::Array, value) => {
            let mut list = match args.remove(path) {
                Some(Value::Array(list)) => list,
                _ => Vec::new(),
            };
            list.push(value);
            Value::Array(list)
        }
        (Shape::Object, Value::String(text)) => split_pairs(&text),
        (_, value) => value,
    };
    args.insert(path.to_owned(), value);
}

/// Merge a raw JSON object (from `-j`, `@FILE` or `-`) into `args`.
fn merge(args: &mut Map<String, Value>, raw: &Value) -> Result<(), String> {
    let map = raw
        .as_object()
        .ok_or_else(|| "expected a JSON object".to_owned())?;
    for (key, value) in map {
        set_field(args, &Value::Null, key, value.clone());
    }
    Ok(())
}

/// Parse `argv` against the tool's `schema`. `files[argv position]` carries the text read for a
/// `-` or `@FILE` token. `Err` is a usage error: the shell sees exit `2`.
pub(crate) fn parse(
    schema: &Value,
    words: &[String],
    files: &BTreeMap<usize, String>,
) -> Result<Invocation, String> {
    let mut args = Map::new();
    let mut filled: BTreeSet<String> = BTreeSet::new();
    let mut operands: Vec<(usize, String)> = Vec::new();
    let mut only_operands = false;
    let mut idx = 0usize;
    while let Some(token) = words.get(idx) {
        idx += 1;
        let word = token.as_str();
        if !only_operands {
            match word {
                "--" => {
                    only_operands = true;
                    continue;
                }
                "--help" | "-h" => return Ok(Invocation::Help),
                "-j" | "--json" => {
                    let raw = words
                        .get(idx)
                        .ok_or_else(|| format!("{word} needs a JSON object"))?;
                    idx += 1;
                    let value: Value = serde_json::from_str(raw)
                        .map_err(|e| format!("{word}: invalid JSON: {e}"))?;
                    merge(&mut args, &value).map_err(|e| format!("{word}: {e}"))?;
                    continue;
                }
                _ if word.starts_with("--no-") => {
                    let name = word[5..].replace('-', "_");
                    if node_at(schema, &name).is_none() {
                        return Err(format!("unknown flag `--{name}` (see `--help`)"));
                    }
                    set_field(&mut args, schema, &name, json!(false));
                    filled.insert(head_of(&name).to_owned());
                    continue;
                }
                _ if word.starts_with("--") => {
                    let body = &word[2..];
                    let (flag, inline) = body
                        .split_once('=')
                        .map_or((body, None), |(name, value)| (name, Some(value)));
                    let path = flag.replace('-', "_");
                    let Some(node) = node_at(schema, &path).cloned() else {
                        return Err(format!("unknown flag `--{flag}` (see `--help`)"));
                    };
                    let value = match inline {
                        Some(text) => coerce(&node, text),
                        None if shape_of(&node) == Shape::Boolean => json!(true),
                        None => match words.get(idx).map(String::as_str) {
                            Some(next) if !(next.starts_with("--") && next.len() > 2) => {
                                idx += 1;
                                coerce(&node, next)
                            }
                            _ => return Err(format!("--{flag} needs a value")),
                        },
                    };
                    set_field(&mut args, schema, &path, value);
                    filled.insert(head_of(&path).to_owned());
                    continue;
                }
                _ if word.starts_with('-') && word.len() > 1 && word != "-" => {
                    return Err(format!(
                        "unknown flag `{word}` (flags are `--<name>`; see `--help`)"
                    ));
                }
                _ => {}
            }
        }
        operands.push((idx - 1, token.clone()));
    }
    let mut slots: VecDeque<String> = positionals(schema)
        .into_iter()
        .filter(|name| !filled.contains(name))
        .collect();
    for (position, operand) in operands {
        let text = files.get(&position);
        if let Some(text) = text
            && let Ok(raw @ Value::Object(_)) = serde_json::from_str::<Value>(text)
        {
            merge(&mut args, &raw).map_err(|e| format!("{operand}: {e}"))?;
            continue;
        }
        let Some(name) = slots.pop_front() else {
            return Err(format!("unexpected argument `{operand}` (see `--help`)"));
        };
        let node = node_at(schema, &name).cloned().unwrap_or(Value::Null);
        let value = match text {
            Some(text) => json!(text.clone()),
            None => coerce(&node, &operand),
        };
        set_field(&mut args, schema, &name, value);
    }
    Ok(Invocation::Call(Value::Object(args)))
}

/// The `--help` page, written from the tool summary and the schema.
pub(crate) fn help(name: &str, summary: &str, schema: &Value) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if !summary.is_empty() {
        let _ = writeln!(out, "{name}: {summary}");
    }
    let _ = write!(out, "usage: {name}");
    for slot in positionals(schema) {
        let _ = write!(out, " <{slot}>");
    }
    let _ = writeln!(out, " [flags]");
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if let Some(fields) = properties(schema) {
        let _ = writeln!(out, "\nflags (a required field also fills its positional):");
        for (key, node) in fields {
            let kind = node.get("type").and_then(Value::as_str).unwrap_or("value");
            let mark = if required.contains(&key.as_str()) {
                "required"
            } else {
                kind
            };
            let text = node
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let _ = writeln!(out, "  --{:<24} [{mark}] {text}", key.replace('_', "-"));
        }
    }
    out.push_str(
        "\n  -j, --json <JSON>    merge a raw JSON object\n  \
         --no-<flag>          set a boolean flag to false\n  \
         @FILE / -            merge a JSON file/stdin, or bind its text to the next argument\n  \
         --                   treat the rest as arguments\n  \
         --help               this page\n\n\
         Exit codes: 0 ok · 1 the tool failed · 2 bad command line\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "description": "Read a file.",
            "properties": {
                "path": {"type": "string", "description": "File to read."},
                "offset": {"type": "integer"},
                "raw": {"type": "boolean"},
                "paths": {"type": "array", "items": {"type": "string"}},
                "selector": {
                    "type": "object",
                    "properties": {"line": {"type": "integer"}}
                }
            },
            "required": ["path"]
        })
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn call(words: &[&str]) -> Value {
        match parse(&schema(), &argv(words), &BTreeMap::new()) {
            Ok(Invocation::Call(args)) => args,
            Ok(Invocation::Help) => panic!("help was not asked for"),
            Err(err) => panic!("usage error: {err}"),
        }
    }

    #[test]
    fn a_required_scalar_is_a_positional_and_flags_fill_the_rest() {
        assert_eq!(
            call(&["notes.txt", "--offset", "12", "--raw"]),
            json!({"path": "notes.txt", "offset": 12, "raw": true})
        );
        assert_eq!(
            call(&["--path=x", "--no-raw"]),
            json!({"path": "x", "raw": false})
        );
        assert_eq!(call(&["--raw", "x"]), json!({"raw": true, "path": "x"}));
        assert_eq!(call(&["--raw=false"]), json!({"raw": false}));
    }

    #[test]
    fn a_flag_filled_slot_is_not_filled_twice_by_an_operand() {
        assert_eq!(
            call(&["--offset", "3", "notes.txt"]),
            json!({"offset": 3, "path": "notes.txt"})
        );
        assert!(parse(&schema(), &argv(&["--path", "a", "b"]), &BTreeMap::new()).is_err());
    }

    #[test]
    fn array_flags_repeat_and_nested_flags_use_dotted_paths() {
        assert_eq!(
            call(&[
                "a",
                "--paths",
                "one",
                "--paths",
                "two",
                "--selector.line",
                "3"
            ]),
            json!({"path": "a", "paths": ["one", "two"], "selector": {"line": 3}})
        );
        assert_eq!(
            call(&["a", "--selector", "line=4"]),
            json!({"path": "a", "selector": {"line": 4}})
        );
    }

    #[test]
    fn json_merges_and_unknown_flags_are_usage_errors() {
        assert_eq!(
            call(&["-j", r#"{"path":"j","offset":2}"#]),
            json!({"path": "j", "offset": 2})
        );
        assert!(parse(&schema(), &argv(&["a", "--zebra"]), &BTreeMap::new()).is_err());
        assert!(parse(&schema(), &argv(&["a", "-r"]), &BTreeMap::new()).is_err());
        assert!(parse(&schema(), &argv(&["a", "--offset"]), &BTreeMap::new()).is_err());
    }

    #[test]
    fn dash_text_merges_json_or_binds_the_next_slot() {
        let files = BTreeMap::from([(2, r#"{"path":"from-file"}"#.to_owned())]);
        match parse(&schema(), &argv(&["-j", "{}", "-"]), &files).expect("parse") {
            Invocation::Call(args) => assert_eq!(args, json!({"path": "from-file"})),
            Invocation::Help => panic!("help"),
        }
        let files = BTreeMap::from([(0, "plain text\n".to_owned())]);
        match parse(&schema(), &argv(&["-"]), &files).expect("parse") {
            Invocation::Call(args) => assert_eq!(args, json!({"path": "plain text\n"})),
            Invocation::Help => panic!("help"),
        }
    }

    #[test]
    fn after_double_dash_everything_is_an_operand() {
        assert_eq!(call(&["--", "--path"]), json!({"path": "--path"}));
    }

    #[test]
    fn lone_help_is_the_usage_page() {
        assert!(matches!(
            parse(&schema(), &argv(&["--help"]), &BTreeMap::new()),
            Ok(Invocation::Help)
        ));
        let page = help("read", "Read a file.", &schema());
        assert!(page.starts_with("read: Read a file.\n"), "{page}");
        assert!(page.contains("usage: read <path> [flags]"), "{page}");
        assert!(page.contains("[required] File to read."), "{page}");
    }
}
