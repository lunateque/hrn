//! JSON5 for `write` payloads on `SQLite` rows: JSON plus unquoted keys, single quotes, trailing
//! commas, `//` and `/* */` comments, hex numbers, leading `+`/`.`, `Infinity` / `NaN`.
//! Values become [`Value`]; a number that is not finite (or exceeds `i64`) becomes a string, so
//! nothing is silently dropped.

use hrn_kernel::{HrnError, Result};
use serde_json::{Map, Number, Value};

/// Parse one JSON5 value; trailing text after it is an error.
///
/// # Errors
/// Malformed JSON5, naming the position.
pub(super) fn parse(text: &str) -> Result<Value> {
    let bytes: Vec<char> = text.chars().collect();
    let mut at = 0usize;
    skip_ws(&bytes, &mut at);
    let value = value_of(&bytes, &mut at, 0)?;
    skip_ws(&bytes, &mut at);
    if at != bytes.len() {
        return Err(err(&bytes, at, "unexpected text after the value"));
    }
    Ok(value)
}

fn err(bytes: &[char], at: usize, what: &str) -> HrnError {
    HrnError::Tool(format!(
        "invalid JSON5 at character {what} (offset {at}): `{}`",
        bytes
            .get(at..(at + 24).min(bytes.len()))
            .unwrap_or_default()
            .iter()
            .collect::<String>()
    ))
}

const MAX_DEPTH: usize = 64;

fn skip_ws(bytes: &[char], at: &mut usize) {
    loop {
        match bytes.get(*at) {
            Some(c) if c.is_whitespace() => *at += 1,
            Some('/') if bytes.get(*at + 1) == Some(&'/') => {
                *at += 2;
                while !matches!(bytes.get(*at), None | Some('\n' | '\r')) {
                    *at += 1;
                }
            }
            Some('/') if bytes.get(*at + 1) == Some(&'*') => {
                *at += 2;
                while bytes.get(*at).is_some() {
                    if bytes.get(*at) == Some(&'*') && bytes.get(*at + 1) == Some(&'/') {
                        *at += 2;
                        break;
                    }
                    *at += 1;
                }
            }
            _ => return,
        }
    }
}

fn value_of(bytes: &[char], at: &mut usize, depth: usize) -> Result<Value> {
    if depth > MAX_DEPTH {
        return Err(err(bytes, *at, "is nested too deeply"));
    }
    let Some(&c) = bytes.get(*at) else {
        return Err(err(bytes, *at, "is missing a value"));
    };
    match c {
        '{' => object(bytes, at, depth),
        '[' => array(bytes, at, depth),
        '"' | '\'' => string(bytes, at).map(Value::String),
        _ if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' => number(bytes, at),
        _ => word(bytes, at),
    }
}

fn object(bytes: &[char], at: &mut usize, depth: usize) -> Result<Value> {
    *at += 1;
    let mut map = Map::new();
    loop {
        skip_ws(bytes, at);
        match bytes.get(*at) {
            Some('}') => {
                *at += 1;
                return Ok(Value::Object(map));
            }
            None => return Err(err(bytes, *at, "leaves the object unterminated")),
            _ => {}
        }
        let key = key_of(bytes, at)?;
        skip_ws(bytes, at);
        if bytes.get(*at) != Some(&':') {
            return Err(err(bytes, *at, "is not `:` after the key"));
        }
        *at += 1;
        skip_ws(bytes, at);
        map.insert(key, value_of(bytes, at, depth + 1)?);
        skip_ws(bytes, at);
        match bytes.get(*at) {
            Some(',') => *at += 1,
            Some('}') => {
                *at += 1;
                return Ok(Value::Object(map));
            }
            _ => return Err(err(bytes, *at, "is not `,` or `}` after the value")),
        }
    }
}

fn key_of(bytes: &[char], at: &mut usize) -> Result<String> {
    match bytes.get(*at) {
        Some('"' | '\'') => string(bytes, at),
        Some(&c) if c.is_alphabetic() || c == '_' || c == '$' => {
            let start = *at;
            while bytes
                .get(*at)
                .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
            {
                *at += 1;
            }
            Ok(bytes.get(start..*at).unwrap_or_default().iter().collect())
        }
        Some(_) => Err(err(bytes, *at, "does not start a key")),
        None => Err(err(bytes, *at, "leaves the object unterminated")),
    }
}

fn array(bytes: &[char], at: &mut usize, depth: usize) -> Result<Value> {
    *at += 1;
    let mut items = Vec::new();
    loop {
        skip_ws(bytes, at);
        match bytes.get(*at) {
            Some(']') => {
                *at += 1;
                return Ok(Value::Array(items));
            }
            None => return Err(err(bytes, *at, "leaves the array unterminated")),
            _ => {}
        }
        items.push(value_of(bytes, at, depth + 1)?);
        skip_ws(bytes, at);
        match bytes.get(*at) {
            Some(',') => *at += 1,
            Some(']') => {
                *at += 1;
                return Ok(Value::Array(items));
            }
            _ => return Err(err(bytes, *at, "is not `,` or `]` after the item")),
        }
    }
}

fn string(bytes: &[char], at: &mut usize) -> Result<String> {
    let quote = *bytes
        .get(*at)
        .ok_or_else(|| err(bytes, *at, "has no value"))?;
    *at += 1;
    let mut out = String::new();
    loop {
        let Some(&c) = bytes.get(*at) else {
            return Err(err(bytes, *at, "leaves the string unterminated"));
        };
        *at += 1;
        match c {
            c if c == quote => return Ok(out),
            '\\' => out.push(escape(bytes, at)?),
            _ => out.push(c),
        }
    }
}

fn escape(bytes: &[char], at: &mut usize) -> Result<char> {
    let Some(&c) = bytes.get(*at) else {
        return Err(err(bytes, *at, "ends in a backslash"));
    };
    *at += 1;
    let made = match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        'b' => '\u{8}',
        'f' => '\u{c}',
        'v' => '\u{b}',
        '0' => '\0',
        '\n' => return Ok('\n'),
        'u' => {
            let hex: String = bytes.get(*at..*at + 4).unwrap_or_default().iter().collect();
            let Ok(code) = u32::from_str_radix(&hex, 16) else {
                return Err(err(bytes, *at, "is not a \\u escape"));
            };
            *at += 4;
            return char::from_u32(code).ok_or_else(|| err(bytes, *at - 4, "is not a scalar"));
        }
        other => other,
    };
    Ok(made)
}

fn number(bytes: &[char], at: &mut usize) -> Result<Value> {
    let start = *at;
    if matches!(bytes.get(*at), Some('-' | '+')) {
        *at += 1;
    }
    let hex = bytes.get(*at) == Some(&'0') && matches!(bytes.get(*at + 1), Some('x' | 'X'));
    if hex {
        *at += 2;
        while bytes.get(*at).is_some_and(char::is_ascii_hexdigit) {
            *at += 1;
        }
        let digits: String = bytes
            .get(start..*at)
            .unwrap_or_default()
            .iter()
            .skip(2)
            .collect();
        return i64::from_str_radix(&digits, 16)
            .map(Value::from)
            .or_else(|_| digits.parse::<f64>().map(json_number))
            .map_err(|_| err(bytes, start, "is not a number"));
    }
    while bytes
        .get(*at)
        .is_some_and(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
    {
        *at += 1;
    }
    let text: String = bytes
        .get(start..*at)
        .unwrap_or_default()
        .iter()
        .filter(|c| **c != '+')
        .collect();
    if text.is_empty() || text == "-" {
        return Err(err(bytes, start, "is not a value"));
    }
    if let Ok(i) = text.parse::<i64>() {
        return Ok(Value::from(i));
    }
    text.parse::<f64>()
        .map(json_number)
        .map_err(|_| err(bytes, start, "is not a number"))
}

fn json_number(f: f64) -> Value {
    if f.is_finite() {
        Number::from_f64(f).map_or_else(|| Value::String(f.to_string()), Value::Number)
    } else {
        Value::String(f.to_string())
    }
}

fn word(bytes: &[char], at: &mut usize) -> Result<Value> {
    let start = *at;
    while bytes.get(*at).is_some_and(|c| c.is_alphabetic()) {
        *at += 1;
    }
    match bytes
        .get(start..*at)
        .unwrap_or_default()
        .iter()
        .collect::<String>()
        .as_str()
    {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        "null" => Ok(Value::Null),
        "Infinity" => Ok(Value::String("Infinity".into())),
        "NaN" => Ok(Value::String("NaN".into())),
        _ => Err(err(bytes, start, "is not a value")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json5_supersets_json_with_tokens_and_comments() {
        let v =
            parse("{ name: 'a\\nb', n: 0x10, f: -.5, tail: [1, 2,], /* c */ b: true, // line\n }")
                .unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj["name"], Value::String("a\nb".into()));
        assert_eq!(obj["n"], Value::from(16));
        assert_eq!(obj["f"], Value::from(-0.5));
        assert_eq!(obj["tail"], serde_json::json!([1, 2]));
        assert_eq!(obj["b"], Value::Bool(true));
        assert_eq!(parse("\"plain\"").unwrap(), Value::String("plain".into()));
        assert!(parse("{a: 1} trailing").is_err());
        assert!(parse("{a: }").is_err());
    }
}
