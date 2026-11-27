//! What the user answered, whether it is acceptable, and the JSON the model gets back.

use super::{Field, Form, Kind, Spec, pattern};
use serde_json::{Map, Number, Value};
use std::cmp::Ordering;

/// One field's current answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    /// Indexes into [`Field::options`] that are chosen.
    pub picked: Vec<usize>,
    /// The free-text "Other" answer of a choice field.
    pub other: String,
    /// The text of a text or hidden field.
    pub text: String,
}

/// A finished form.
#[derive(Clone, Debug, PartialEq)]
pub struct Submission {
    /// The answers keyed by field name.
    pub values: Value,
    /// The user did not answer in time and these are the form's current values.
    pub timed_out: bool,
}

impl Form {
    /// The answers the form starts with: everything `checked`, `selected` or given a `value`.
    pub fn defaults(&self) -> Vec<Answer> {
        self.fields().map(Field::default_answer).collect()
    }

    /// Per field, why its answer is not acceptable; `None` when it is.
    pub fn errors(&self, answers: &[Answer]) -> Vec<Option<String>> {
        self.fields()
            .zip(answers)
            .map(|(field, answer)| field.error(answer))
            .collect()
    }

    /// The answers as `{name: value}`: a radio group or single select gives its value, a checkbox
    /// group or multi-select an array, a lone checkbox its value when checked, a number input a
    /// number; an unanswered choice is left out.
    pub fn values(&self, answers: &[Answer]) -> Value {
        let mut map = Map::new();
        for (field, answer) in self.fields().zip(answers) {
            if let Some(value) = field.value(answer) {
                map.insert(field.name.clone(), value);
            }
        }
        Value::Object(map)
    }
}

impl Field {
    fn default_answer(&self) -> Answer {
        let checked = |(i, choice): (usize, &super::Choice)| choice.checked.then_some(i);
        let mut answer = Answer::default();
        match &self.kind {
            Kind::One => {
                answer.picked = self
                    .options
                    .iter()
                    .enumerate()
                    .filter_map(checked)
                    .next_back()
                    .into_iter()
                    .collect();
            }
            Kind::Many | Kind::Flag => {
                answer.picked = self
                    .options
                    .iter()
                    .enumerate()
                    .filter_map(checked)
                    .collect();
            }
            Kind::Text(spec) => answer.text.clone_from(&spec.value),
            Kind::Hidden(value) => answer.text.clone_from(value),
        }
        answer
    }

    fn value(&self, answer: &Answer) -> Option<Value> {
        let other = answer.other.trim();
        let chosen = |i: &usize| self.options.get(*i).map(|c| Value::String(c.value.clone()));
        match &self.kind {
            Kind::One if !other.is_empty() => Some(Value::String(other.to_owned())),
            Kind::One | Kind::Flag => answer.picked.first().and_then(chosen),
            Kind::Many => {
                let mut picked = answer.picked.clone();
                picked.sort_unstable();
                let mut values: Vec<Value> = picked.iter().filter_map(chosen).collect();
                if !other.is_empty() {
                    values.push(Value::String(other.to_owned()));
                }
                Some(Value::Array(values))
            }
            Kind::Text(spec) => Some(text_value(spec, &answer.text)),
            Kind::Hidden(_) => Some(Value::String(answer.text.clone())),
        }
    }

    fn error(&self, answer: &Answer) -> Option<String> {
        let blank = match &self.kind {
            Kind::One | Kind::Many => answer.picked.is_empty() && answer.other.trim().is_empty(),
            Kind::Flag => answer.picked.is_empty(),
            Kind::Text(_) => answer.text.trim().is_empty(),
            Kind::Hidden(_) => return None,
        };
        if blank {
            return self.required.then(|| "This is required".to_owned());
        }
        match &self.kind {
            Kind::Text(spec) => text_error(spec, &answer.text),
            _ => None,
        }
    }
}

fn text_value(spec: &Spec, text: &str) -> Value {
    if matches!(spec.input.as_str(), "number" | "range")
        && let Some(number) = parse_number(text)
    {
        return Value::Number(number);
    }
    Value::String(text.to_owned())
}

fn parse_number(text: &str) -> Option<Number> {
    let text = text.trim();
    text.parse::<i64>()
        .ok()
        .map(Number::from)
        .or_else(|| text.parse::<f64>().ok().and_then(Number::from_f64))
}

fn text_error(spec: &Spec, text: &str) -> Option<String> {
    let chars = text.chars().count();
    if let Some(min) = spec.min_len.filter(|min| chars < *min) {
        return Some(format!("Use at least {min} characters"));
    }
    if let Some(max) = spec.max_len.filter(|max| chars > *max) {
        return Some(format!("Use at most {max} characters"));
    }
    let value = text.trim();
    let format = match spec.input.as_str() {
        "number" | "range" => parse_number(value)
            .is_none()
            .then(|| "Enter a number".to_owned()),
        "date" => (!is_date(value)).then(|| "Enter a date as YYYY-MM-DD".to_owned()),
        "email" => (!is_email(value)).then(|| "Enter an email address".to_owned()),
        "url" => (!is_url(value)).then(|| "Enter a URL such as https://example.com".to_owned()),
        _ => None,
    };
    format
        .or_else(|| bound_error(spec, value))
        .or_else(|| pattern_error(spec, text))
}

fn bound_error(spec: &Spec, value: &str) -> Option<String> {
    let compare = |bound: &str| -> Option<Ordering> {
        match (value.parse::<f64>(), bound.trim().parse::<f64>()) {
            (Ok(a), Ok(b)) => a.partial_cmp(&b),
            _ if spec.input == "number" || spec.input == "range" => None,
            _ => Some(value.cmp(bound.trim())),
        }
    };
    if let Some(min) = spec
        .min
        .as_deref()
        .filter(|min| compare(min) == Some(Ordering::Less))
    {
        return Some(format!("Must be at least {min}"));
    }
    spec.max
        .as_deref()
        .filter(|max| compare(max) == Some(Ordering::Greater))
        .map(|max| format!("Must be at most {max}"))
}

fn pattern_error(spec: &Spec, text: &str) -> Option<String> {
    let pattern = spec.pattern.as_deref()?;
    (pattern::full_match(pattern, text) == Some(false)).then(|| {
        spec.title
            .clone()
            .unwrap_or_else(|| format!("Must match {pattern}"))
    })
}

fn is_date(text: &str) -> bool {
    let mut parts = text.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let number = |s: &str, len: usize| {
        (s.len() == len && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())
            .flatten()
    };
    number(y, 4).is_some()
        && number(m, 2).is_some_and(|m| (1..=12).contains(&m))
        && number(d, 2).is_some_and(|d| (1..=31).contains(&d))
}

fn is_email(text: &str) -> bool {
    text.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty()
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && domain.contains('.')
            && !text.contains(char::is_whitespace)
            && !domain.contains('@')
    })
}

fn is_url(text: &str) -> bool {
    text.split_once("://").is_some_and(|(scheme, rest)| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
            && !rest.is_empty()
            && !text.contains(char::is_whitespace)
    })
}
