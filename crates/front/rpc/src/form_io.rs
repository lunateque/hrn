//! A parsed `ask` form on the wire: re-rendered as HTML for RPC hosts, described as a JSON schema
//! for ACP elicitation, and the host's answers read back into the form's own answers.

use crate::wire::put;
use hrn_kernel::{Answer, Field, Form, Item, Kind, Submission, TextKind};
use serde_json::{Map, Value, json};
use std::fmt::Write as _;

/// Escape `text` for an HTML text or attribute position.
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// The form as HTML a host can render with a browser-grade form widget.
pub(crate) fn to_html(form: &Form) -> String {
    let mut html = String::from("<form>");
    for item in &form.items {
        match item {
            Item::Text { kind, text } => {
                let tag = match kind {
                    TextKind::Heading => "h3",
                    TextKind::Legend => "legend",
                    TextKind::Para => "p",
                    TextKind::List => "li",
                    TextKind::Pre => "pre",
                };
                let _ = write!(html, "<{tag}>{}</{tag}>", esc(text));
            }
            Item::Table { header, rows } => {
                html.push_str("<table>");
                for (n, row) in rows.iter().enumerate() {
                    let cell = if *header && n == 0 { "th" } else { "td" };
                    html.push_str("<tr>");
                    for text in row {
                        let _ = write!(html, "<{cell}>{}</{cell}>", esc(text));
                    }
                    html.push_str("</tr>");
                }
                html.push_str("</table>");
            }
            Item::Image { alt } => {
                let _ = write!(html, "<p>{}</p>", esc(alt));
            }
            Item::Field(field) => field_html(field, &mut html),
        }
    }
    html.push_str("</form>");
    html
}

fn field_html(field: &Field, html: &mut String) {
    let name = esc(&field.name);
    let required = if field.required { " required" } else { "" };
    match &field.kind {
        Kind::One | Kind::Many => {
            let input = if matches!(field.kind, Kind::One) {
                "radio"
            } else {
                "checkbox"
            };
            let _ = write!(html, "<fieldset><legend>{}</legend>", esc(&field.question));
            for choice in &field.options {
                let checked = if choice.checked { " checked" } else { "" };
                let _ = write!(
                    html,
                    "<label><input type=\"{input}\" name=\"{name}\" value=\"{}\"{checked}{required}> {}</label>",
                    esc(&choice.value),
                    esc(&choice.label)
                );
            }
            html.push_str("</fieldset>");
        }
        Kind::Flag => {
            for choice in &field.options {
                let checked = if choice.checked { " checked" } else { "" };
                let _ = write!(
                    html,
                    "<label><input type=\"checkbox\" name=\"{name}\" value=\"{}\"{checked}{required}> {}</label>",
                    esc(&choice.value),
                    esc(&field.question)
                );
            }
        }
        Kind::Text(spec) => {
            let _ = write!(html, "<label>{}</label>", esc(&field.question));
            let mut attrs = format!(" name=\"{name}\"{required}");
            for (key, value) in [
                ("placeholder", Some(&spec.placeholder)),
                ("min", spec.min.as_ref()),
                ("max", spec.max.as_ref()),
                ("pattern", spec.pattern.as_ref()),
                ("title", spec.title.as_ref()),
            ] {
                if let Some(value) = value.filter(|v| !v.is_empty()) {
                    let _ = write!(attrs, " {key}=\"{}\"", esc(value));
                }
            }
            if let Some(n) = spec.min_len {
                let _ = write!(attrs, " minlength=\"{n}\"");
            }
            if let Some(n) = spec.max_len {
                let _ = write!(attrs, " maxlength=\"{n}\"");
            }
            if spec.multiline {
                let _ = write!(html, "<textarea{attrs}>{}</textarea>", esc(&spec.value));
            } else {
                let _ = write!(
                    html,
                    "<input type=\"{}\"{attrs} value=\"{}\">",
                    esc(&spec.input),
                    esc(&spec.value)
                );
            }
        }
        Kind::Hidden(value) => {
            let _ = write!(
                html,
                "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
                esc(value)
            );
        }
    }
}

/// The context text of the form (headings and paragraphs), for a prompt line.
pub(crate) fn context(form: &Form) -> String {
    form.items
        .iter()
        .filter_map(|item| match item {
            Item::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A JSON schema of the form's fields, for a client that renders forms from a schema.
pub(crate) fn to_schema(form: &Form) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for field in form.fields() {
        let titled = |field: &Field| -> Vec<Value> {
            field
                .options
                .iter()
                .map(|c| json!({ "const": c.value, "title": c.label }))
                .collect()
        };
        let default: Vec<&str> = field
            .options
            .iter()
            .filter(|c| c.checked)
            .map(|c| c.value.as_str())
            .collect();
        let property = match &field.kind {
            Kind::One => {
                let mut p =
                    json!({ "type": "string", "title": field.question, "oneOf": titled(field) });
                if let Some(first) = default.last() {
                    put(&mut p, "default", json!(first));
                }
                p
            }
            Kind::Many => json!({
                "type": "array",
                "title": field.question,
                "items": { "anyOf": titled(field) },
                "default": default,
            }),
            Kind::Flag => json!({
                "type": "boolean",
                "title": field.question,
                "default": !default.is_empty(),
            }),
            Kind::Text(spec) => {
                let numeric = matches!(spec.input.as_str(), "number" | "range");
                let mut p = json!({
                    "type": if numeric { "number" } else { "string" },
                    "title": field.question,
                });
                if numeric {
                    if let Some(min) = spec
                        .min
                        .as_deref()
                        .and_then(|v| v.trim().parse::<f64>().ok())
                    {
                        put(&mut p, "minimum", json!(min));
                    }
                    if let Some(max) = spec
                        .max
                        .as_deref()
                        .and_then(|v| v.trim().parse::<f64>().ok())
                    {
                        put(&mut p, "maximum", json!(max));
                    }
                } else {
                    if let Some(n) = spec.min_len {
                        put(&mut p, "minLength", json!(n));
                    }
                    if let Some(n) = spec.max_len {
                        put(&mut p, "maxLength", json!(n));
                    }
                    if let Some(pattern) = &spec.pattern {
                        put(&mut p, "pattern", json!(pattern));
                    }
                }
                if !spec.value.is_empty() {
                    put(&mut p, "default", json!(spec.value));
                }
                p
            }
            Kind::Hidden(_) => continue,
        };
        properties.insert(field.name.clone(), property);
        if field.required {
            required.push(field.name.clone());
        }
    }
    json!({ "type": "object", "properties": properties, "required": required })
}

/// The text a JSON answer stands for.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn pick(field: &Field, answer: &mut Answer, text: &str) -> Result<(), String> {
    if let Some(i) = field.options.iter().position(|c| c.value == text) {
        answer.picked.push(i);
        return Ok(());
    }
    if field.other && answer.other.is_empty() {
        text.clone_into(&mut answer.other);
        return Ok(());
    }
    Err(format!(
        "`{}`: `{text}` is not one of the options",
        field.name
    ))
}

/// Read the host's answers (`{name: string | number | boolean | [string]}`) into the form's answers,
/// start from the form's defaults for fields the host left out, and check them.
///
/// # Errors
/// An unknown field name, a value of the wrong shape, or an answer the form rejects; the message
/// lists every problem.
pub(crate) fn submission(form: &Form, data: &Value) -> Result<Submission, String> {
    let empty = Map::new();
    let data = match data {
        Value::Null => &empty,
        Value::Object(map) => map,
        _ => return Err("form data must be an object keyed by field name".to_owned()),
    };
    let mut problems: Vec<String> = data
        .keys()
        .filter(|key| form.fields().all(|f| &f.name != *key))
        .map(|key| format!("`{key}` is not a field of this form"))
        .collect();
    let mut answers = form.defaults();
    for (field, answer) in form.fields().zip(answers.iter_mut()) {
        let Some(value) = data.get(&field.name) else {
            continue;
        };
        let result = match &field.kind {
            Kind::One | Kind::Many => {
                answer.picked.clear();
                answer.other.clear();
                let items: Vec<&Value> = match value {
                    Value::Array(items) => items.iter().collect(),
                    other => vec![other],
                };
                items.into_iter().try_for_each(|item| match scalar(item) {
                    Some(text) => pick(field, answer, &text),
                    None => Err(format!("`{}`: expected text", field.name)),
                })
            }
            Kind::Flag => {
                let on = match value {
                    Value::Bool(b) => *b,
                    other => {
                        scalar(other).is_some_and(|t| field.options.iter().any(|c| c.value == t))
                    }
                };
                answer.picked = if on { vec![0] } else { Vec::new() };
                Ok(())
            }
            Kind::Text(_) => scalar(value).map_or_else(
                || Err(format!("`{}`: expected text or a number", field.name)),
                |text| {
                    answer.text = text;
                    Ok(())
                },
            ),
            Kind::Hidden(_) => Ok(()),
        };
        if let Err(problem) = result {
            problems.push(problem);
        }
    }
    for (field, error) in form.fields().zip(form.errors(&answers)) {
        if let Some(error) = error {
            problems.push(format!("`{}`: {error}", field.name));
        }
    }
    if problems.is_empty() {
        Ok(Submission {
            values: form.values(&answers),
            timed_out: false,
        })
    } else {
        Err(problems.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> Form {
        Form::parse(
            "<form><fieldset><legend>Pick</legend>\
             <label><input type=radio name=a value=x> X</label>\
             <label><input type=radio name=a value=y> Y</label></fieldset>\
             <label>Note <input type=text name=n required></label>\
             <label><input type=checkbox name=ok value=yes> Sure</label></form>",
        )
        .unwrap_or_default()
    }

    #[test]
    fn answers_become_the_values_the_model_sees() {
        let got = submission(&form(), &json!({"a": "y", "n": "hi", "ok": true}));
        assert_eq!(
            got.map(|s| s.values),
            Ok(json!({"a": "y", "n": "hi", "ok": "yes"}))
        );
    }

    #[test]
    fn a_bad_answer_names_every_problem() {
        let err = submission(&form(), &json!({"a": "z", "b": 1})).unwrap_err();
        assert!(err.contains("`b` is not a field"), "{err}");
        assert!(err.contains("`n`: This is required"), "{err}");
    }

    #[test]
    fn rendered_html_round_trips_through_the_form_parser() {
        let original = form();
        let again = Form::parse(&to_html(&original)).unwrap_or_default();
        let fields = |f: &Form| -> Vec<(String, bool, String)> {
            f.fields()
                .map(|field| {
                    (
                        field.name.clone(),
                        field.required,
                        field
                            .options
                            .iter()
                            .map(|c| c.value.clone())
                            .collect::<Vec<_>>()
                            .join(","),
                    )
                })
                .collect()
        };
        assert_eq!(fields(&original), fields(&again));
    }
}
