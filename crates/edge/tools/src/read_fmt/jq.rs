//! A deliberately small jq subset for `?q=` on JSON / JSONL:
//! paths (`.a.b`, `.["k"]`, `.[n]`, `.[a:b]`, `.[]`, `?` to swallow errors), `|`, `keys`,
//! `length`, `type`, `first`, `last`, `not`, `map(f)` and `select(cond)` where `cond` combines
//! `path op value|path` (`== != < <= > >=`), bare paths (truthiness), `and`, `or`, parentheses.

use hrn_kernel::{HrnError, Result};
use serde_json::Value;
use std::cmp::Ordering;

enum Seg {
    Key(String),
    Index(i64),
    Slice(Option<i64>, Option<i64>),
    Iter,
}

enum Stage {
    /// Segments plus whether a failure is swallowed (`?`).
    Path(Vec<Seg>, bool),
    Keys,
    Length,
    Type,
    First,
    Last,
    Not,
    Select(Cond),
    Map(Vec<Stage>),
}

#[derive(Clone, Copy)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

enum Operand {
    Lit(Value),
    Pipe(Vec<Stage>),
}

enum Cond {
    Or(Vec<Cond>),
    And(Vec<Cond>),
    Cmp(Vec<Stage>, Op, Operand),
    Truthy(Vec<Stage>),
}

/// A parsed filter.
pub(super) struct Program(Vec<Stage>);

fn fail(msg: impl std::fmt::Display) -> HrnError {
    HrnError::Tool(format!("jq: {msg}"))
}

/// Parse `filter`; an empty filter is the identity.
pub(super) fn parse(filter: &str) -> Result<Program> {
    let mut p = Parser { s: filter, pos: 0 };
    let stages = p.pipeline()?;
    p.skip_ws();
    if p.pos < filter.len() {
        return Err(fail(format!(
            "unexpected `{}` at offset {}",
            filter.get(p.pos..).unwrap_or_default(),
            p.pos
        )));
    }
    Ok(Program(stages))
}

impl Program {
    /// Every output of the filter for `input`.
    pub(super) fn run(&self, input: Value) -> Result<Vec<Value>> {
        eval(&self.0, vec![input])
    }
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        self.skip_ws();
        let hit = self.peek() == Some(b);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn expect(&mut self, b: u8) -> Result {
        if self.eat(b) {
            Ok(())
        } else {
            Err(fail(format!(
                "expected `{}` at offset {}",
                char::from(b),
                self.pos
            )))
        }
    }

    fn keyword(&mut self, kw: &str) -> bool {
        self.skip_ws();
        let rest = self.s.get(self.pos..).unwrap_or_default();
        let hit =
            rest.starts_with(kw) && !rest.as_bytes().get(kw.len()).copied().is_some_and(is_ident);
        if hit {
            self.pos += kw.len();
        }
        hit
    }

    fn ident(&mut self) -> &str {
        let start = self.pos;
        while self.peek().is_some_and(is_ident) {
            self.pos += 1;
        }
        self.s.get(start..self.pos).unwrap_or_default()
    }

    /// One JSON value at the cursor: a string, or a bare number / `true` / `false` / `null`.
    fn literal(&mut self) -> Result<Value> {
        self.skip_ws();
        let rest = self.s.get(self.pos..).unwrap_or_default();
        let parsed = if rest.starts_with('"') {
            let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
            let v = stream.next().and_then(std::result::Result::ok);
            self.pos += stream.byte_offset();
            v
        } else {
            let len = rest
                .find(|c: char| c.is_whitespace() || ")|,]".contains(c))
                .unwrap_or(rest.len());
            self.pos += len;
            serde_json::from_str(rest.get(..len).unwrap_or_default()).ok()
        };
        parsed.ok_or_else(|| fail(format!("expected a JSON value at offset {}", self.pos)))
    }

    fn pipeline(&mut self) -> Result<Vec<Stage>> {
        let mut stages = Vec::new();
        loop {
            stages.push(self.stage()?);
            if !self.eat(b'|') {
                return Ok(stages);
            }
        }
    }

    fn stage(&mut self) -> Result<Stage> {
        self.skip_ws();
        match self.peek() {
            Some(b'.') => self.path(),
            Some(b) if b.is_ascii_alphabetic() => {
                let name = self.ident().to_owned();
                match name.as_str() {
                    "keys" => Ok(Stage::Keys),
                    "length" => Ok(Stage::Length),
                    "type" => Ok(Stage::Type),
                    "first" => Ok(Stage::First),
                    "last" => Ok(Stage::Last),
                    "not" => Ok(Stage::Not),
                    "select" => {
                        self.expect(b'(')?;
                        let cond = self.or()?;
                        self.expect(b')')?;
                        Ok(Stage::Select(cond))
                    }
                    "map" => {
                        self.expect(b'(')?;
                        let inner = self.pipeline()?;
                        self.expect(b')')?;
                        Ok(Stage::Map(inner))
                    }
                    other => Err(fail(format!(
                        "unsupported function `{other}` (supported: keys length type first last \
                         not map select)"
                    ))),
                }
            }
            _ => Err(fail(format!("unexpected input at offset {}", self.pos))),
        }
    }

    fn path(&mut self) -> Result<Stage> {
        self.pos += 1; // the leading `.`
        let mut segs = Vec::new();
        let mut optional = false;
        loop {
            match self.peek() {
                Some(b'"') => match self.literal()? {
                    Value::String(k) => segs.push(Seg::Key(k)),
                    _ => return Err(fail("object key must be a string")),
                },
                Some(b) if is_ident(b) => segs.push(Seg::Key(self.ident().to_owned())),
                Some(b'[') => segs.push(self.bracket()?),
                _ => break,
            }
            if self.peek() == Some(b'?') {
                optional = true;
                self.pos += 1;
            }
            if self.peek() == Some(b'.') {
                self.pos += 1;
            }
        }
        if self.peek() == Some(b'?') {
            optional = true;
            self.pos += 1;
        }
        Ok(Stage::Path(segs, optional))
    }

    fn int(&mut self) -> Result<Option<i64>> {
        self.skip_ws();
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        let text = self.s.get(start..self.pos).unwrap_or_default();
        if text.is_empty() {
            return Ok(None);
        }
        text.parse()
            .map(Some)
            .map_err(|_| fail(format!("bad index `{text}`")))
    }

    fn bracket(&mut self) -> Result<Seg> {
        self.pos += 1; // `[`
        self.skip_ws();
        let seg = match self.peek() {
            Some(b']') => Seg::Iter,
            Some(b'"') => match self.literal()? {
                Value::String(k) => Seg::Key(k),
                _ => return Err(fail("object key must be a string")),
            },
            _ => {
                let from = self.int()?;
                if self.eat(b':') {
                    Seg::Slice(from, self.int()?)
                } else {
                    Seg::Index(from.ok_or_else(|| fail("empty index"))?)
                }
            }
        };
        self.expect(b']')?;
        Ok(seg)
    }

    fn or(&mut self) -> Result<Cond> {
        let mut parts = vec![self.and()?];
        while self.keyword("or") {
            parts.push(self.and()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Cond::Or(parts)
        })
    }

    fn and(&mut self) -> Result<Cond> {
        let mut parts = vec![self.comparison()?];
        while self.keyword("and") {
            parts.push(self.comparison()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Cond::And(parts)
        })
    }

    fn operator(&mut self) -> Option<Op> {
        self.skip_ws();
        let rest = self.s.get(self.pos..).unwrap_or_default();
        let (op, len) = [
            ("==", Op::Eq),
            ("!=", Op::Ne),
            ("<=", Op::Le),
            (">=", Op::Ge),
            ("<", Op::Lt),
            (">", Op::Gt),
        ]
        .into_iter()
        .find_map(|(t, op)| rest.starts_with(t).then_some((op, t.len())))?;
        self.pos += len;
        Some(op)
    }

    fn comparison(&mut self) -> Result<Cond> {
        if self.eat(b'(') {
            let inner = self.or()?;
            self.expect(b')')?;
            return Ok(inner);
        }
        let lhs = self.pipeline()?;
        let Some(op) = self.operator() else {
            return Ok(Cond::Truthy(lhs));
        };
        self.skip_ws();
        let rhs = if self.peek() == Some(b'.') {
            Operand::Pipe(vec![self.stage()?])
        } else {
            Operand::Lit(self.literal()?)
        };
        Ok(Cond::Cmp(lhs, op, rhs))
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(false) => 1,
        Value::Bool(true) => 2,
        Value::Number(_) => 3,
        Value::String(_) => 4,
        Value::Array(_) => 5,
        Value::Object(_) => 6,
    }
}

/// jq's total order (objects compare equal only when identical).
fn order(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .partial_cmp(&y.as_f64())
            .unwrap_or(Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Array(x), Value::Array(y)) => x
            .iter()
            .zip(y)
            .map(|(p, q)| order(p, q))
            .find(|o| o.is_ne())
            .unwrap_or_else(|| x.len().cmp(&y.len())),
        (Value::Object(_), Value::Object(_)) if a != b => Ordering::Less,
        _ => rank(a).cmp(&rank(b)),
    }
}

fn truthy(v: &Value) -> bool {
    !matches!(v, Value::Null | Value::Bool(false))
}

fn holds(op: Op, a: &Value, b: &Value) -> bool {
    let o = order(a, b);
    match op {
        Op::Eq => o.is_eq(),
        Op::Ne => o.is_ne(),
        Op::Lt => o.is_lt(),
        Op::Le => o.is_le(),
        Op::Gt => o.is_gt(),
        Op::Ge => o.is_ge(),
    }
}

fn test(cond: &Cond, v: &Value) -> Result<bool> {
    Ok(match cond {
        Cond::Or(parts) => {
            for p in parts {
                if test(p, v)? {
                    return Ok(true);
                }
            }
            false
        }
        Cond::And(parts) => {
            for p in parts {
                if !test(p, v)? {
                    return Ok(false);
                }
            }
            true
        }
        Cond::Truthy(p) => eval(p, vec![v.clone()])?.iter().any(truthy),
        Cond::Cmp(lhs, op, rhs) => {
            let left = eval(lhs, vec![v.clone()])?;
            let right = match rhs {
                Operand::Lit(lit) => vec![lit.clone()],
                Operand::Pipe(p) => eval(p, vec![v.clone()])?,
            };
            left.iter().any(|a| right.iter().any(|b| holds(*op, a, b)))
        }
    })
}

fn eval(stages: &[Stage], input: Vec<Value>) -> Result<Vec<Value>> {
    let mut cur = input;
    for stage in stages {
        let mut next = Vec::new();
        for v in cur {
            apply(stage, v, &mut next)?;
        }
        cur = next;
    }
    Ok(cur)
}

fn apply(stage: &Stage, v: Value, out: &mut Vec<Value>) -> Result {
    match stage {
        Stage::Path(segs, optional) => {
            let mut cur = vec![v];
            for seg in segs {
                let mut next = Vec::new();
                for item in cur {
                    if let Err(e) = step(seg, item, &mut next) {
                        if *optional {
                            return Ok(());
                        }
                        return Err(e);
                    }
                }
                cur = next;
            }
            out.extend(cur);
        }
        Stage::Keys => out.push(match &v {
            Value::Object(m) => Value::Array(m.keys().cloned().map(Value::String).collect()),
            Value::Array(a) => Value::Array((0..a.len()).map(Value::from).collect()),
            other => return Err(fail(format!("{} has no keys", type_name(other)))),
        }),
        Stage::Length => out.push(match &v {
            Value::Null => Value::from(0),
            Value::Bool(_) => return Err(fail("boolean has no length")),
            Value::Number(n) => serde_json::Number::from_f64(n.as_f64().unwrap_or(0.0).abs())
                .map_or(Value::Null, Value::Number),
            Value::String(s) => Value::from(s.chars().count()),
            Value::Array(a) => Value::from(a.len()),
            Value::Object(m) => Value::from(m.len()),
        }),
        Stage::Type => out.push(Value::from(type_name(&v))),
        Stage::First => out.push(match v {
            Value::Array(a) => a.into_iter().next().unwrap_or(Value::Null),
            other => return Err(fail(format!("cannot take first of {}", type_name(&other)))),
        }),
        Stage::Last => out.push(match v {
            Value::Array(a) => a.into_iter().next_back().unwrap_or(Value::Null),
            other => return Err(fail(format!("cannot take last of {}", type_name(&other)))),
        }),
        Stage::Not => out.push(Value::Bool(!truthy(&v))),
        Stage::Select(cond) => {
            if test(cond, &v)? {
                out.push(v);
            }
        }
        Stage::Map(inner) => {
            let items: Vec<Value> = match v {
                Value::Array(a) => a,
                Value::Object(m) => m.into_iter().map(|(_, x)| x).collect(),
                other => return Err(fail(format!("cannot map over {}", type_name(&other)))),
            };
            out.push(Value::Array(eval(inner, items)?));
        }
    }
    Ok(())
}

fn index_of(len: usize, i: i64) -> Option<usize> {
    if i >= 0 {
        usize::try_from(i).ok()
    } else {
        len.checked_sub(usize::try_from(i.unsigned_abs()).ok()?)
    }
}

fn bound(len: usize, i: Option<i64>, default: usize) -> usize {
    match i {
        None => default,
        Some(i) if i >= 0 => usize::try_from(i).unwrap_or(len).min(len),
        Some(i) => len.saturating_sub(usize::try_from(i.unsigned_abs()).unwrap_or(len)),
    }
}

fn step(seg: &Seg, v: Value, out: &mut Vec<Value>) -> Result {
    match (seg, v) {
        (Seg::Key(k), Value::Object(m)) => out.push(m.get(k).cloned().unwrap_or(Value::Null)),
        (Seg::Key(_) | Seg::Index(_) | Seg::Slice(..), Value::Null) => out.push(Value::Null),
        (Seg::Key(k), other) => {
            return Err(fail(format!(
                "cannot index {} with \"{k}\"",
                type_name(&other)
            )));
        }
        (Seg::Index(i), Value::Array(a)) => out.push(
            index_of(a.len(), *i)
                .and_then(|i| a.get(i).cloned())
                .unwrap_or(Value::Null),
        ),
        (Seg::Slice(from, to), Value::Array(a)) => {
            let (lo, hi) = (bound(a.len(), *from, 0), bound(a.len(), *to, a.len()));
            out.push(Value::Array(
                a.get(lo..hi.max(lo)).unwrap_or_default().to_vec(),
            ));
        }
        (Seg::Slice(from, to), Value::String(s)) => {
            let chars: Vec<char> = s.chars().collect();
            let (lo, hi) = (
                bound(chars.len(), *from, 0),
                bound(chars.len(), *to, chars.len()),
            );
            out.push(Value::String(
                chars
                    .get(lo..hi.max(lo))
                    .unwrap_or_default()
                    .iter()
                    .collect(),
            ));
        }
        (Seg::Iter, Value::Array(a)) => out.extend(a),
        (Seg::Iter, Value::Object(m)) => out.extend(m.into_iter().map(|(_, x)| x)),
        (Seg::Index(_) | Seg::Slice(..) | Seg::Iter, other) => {
            return Err(fail(format!(
                "cannot {} {}",
                if matches!(seg, Seg::Iter) {
                    "iterate over"
                } else {
                    "index"
                },
                type_name(&other)
            )));
        }
    }
    Ok(())
}
