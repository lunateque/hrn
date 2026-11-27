//! The YAML subset omp writes its `models.yml` and `config.yml` in: block maps and sequences,
//! flow `[a, b]` / `{ k: v }` collections, plain, single- and double-quoted scalars, comments.
//! Anchors, tags, multi-document files and multi-line plain scalars are not supported (a line
//! that is not `key: value` is skipped), which keeps this far smaller than a full YAML crate.

/// A parsed YAML value. Scalars stay text; the accessors convert on demand.
#[derive(Clone, Debug, PartialEq)]
pub enum Yaml {
    /// Key-value mapping preserving source order.
    Map(Vec<(String, Self)>),
    /// Ordered sequence of YAML nodes.
    Seq(Vec<Self>),
    /// Scalar value stored as raw text.
    Scalar(String),
}

impl Yaml {
    /// Return the value of `key` when this node is a map.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            Self::Seq(_) | Self::Scalar(_) => None,
        }
    }

    /// Return the text of a non-empty scalar.
    #[must_use]
    pub fn str(&self) -> Option<&str> {
        match self {
            Self::Scalar(s) if !s.is_empty() => Some(s),
            Self::Scalar(_) | Self::Map(_) | Self::Seq(_) => None,
        }
    }

    /// Parse this scalar as an `f64`.
    #[must_use]
    pub fn f64(&self) -> Option<f64> {
        self.str()?.parse().ok()
    }

    /// Parse this scalar as a `u64`.
    #[must_use]
    pub fn u64(&self) -> Option<u64> {
        self.str()?.parse().ok()
    }

    /// Parse this scalar as a case-insensitive boolean (`true` or `false`).
    #[must_use]
    pub fn bool(&self) -> Option<bool> {
        let s = self.str()?;
        if s.eq_ignore_ascii_case("true") {
            Some(true)
        } else if s.eq_ignore_ascii_case("false") {
            Some(false)
        } else {
            None
        }
    }

    /// Return the items of a sequence (empty slice for non-sequences).
    #[must_use]
    pub fn items(&self) -> &[Self] {
        match self {
            Self::Seq(items) => items,
            Self::Map(_) | Self::Scalar(_) => &[],
        }
    }

    /// Return the entries of a map (empty slice for non-maps).
    #[must_use]
    pub fn entries(&self) -> &[(String, Self)] {
        match self {
            Self::Map(entries) => entries,
            Self::Seq(_) | Self::Scalar(_) => &[],
        }
    }
}

struct Line {
    indent: usize,
    text: String,
}

/// Parse a YAML subset document; returns `None` when `text` holds no content.
#[must_use]
pub fn parse(text: &str) -> Option<Yaml> {
    let lines: Vec<Line> = text
        .lines()
        .filter_map(|raw| {
            let body = strip_comment(raw);
            let trimmed = body.trim();
            let indent = body.len() - body.trim_start().len();
            if trimmed.is_empty() || (indent == 0 && matches!(trimmed, "---" | "...")) {
                return None;
            }
            Some(Line {
                indent,
                text: trimmed.to_owned(),
            })
        })
        .collect();
    let indent = lines.first()?.indent;
    let mut parser = Parser { lines, pos: 0 };
    Some(parser.block(indent))
}

/// `raw` without a trailing ` # comment` (a `#` inside quotes is text; `''` inside single
/// quotes and `\"` / `\\` inside double quotes are escaped).
fn strip_comment(raw: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut prev_space = true;
    let mut chars = raw.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match quote {
            Some('\'') if c == '\'' && chars.peek().is_some_and(|&(_, n)| n == '\'') => {
                chars.next();
            }
            Some('"') if c == '\\' => {
                chars.next();
            }
            Some(q) if c == q => quote = None,
            None if c == '#' && prev_space => return raw.get(..at).unwrap_or(raw),
            None if (c == '\'' || c == '"') && prev_space => quote = Some(c),
            _ => {}
        }
        prev_space = c.is_whitespace() || (quote.is_none() && matches!(c, '[' | '{' | ',' | ':'));
    }
    raw
}

struct Parser {
    lines: Vec<Line>,
    pos: usize,
}

fn is_dash(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

impl Parser {
    fn peek(&self) -> Option<&Line> {
        self.lines.get(self.pos)
    }

    fn block(&mut self, indent: usize) -> Yaml {
        let Some(line) = self.peek() else {
            return Yaml::Scalar(String::new());
        };
        if is_dash(&line.text) {
            self.seq(indent)
        } else if split_key(&line.text).is_some() {
            self.map(indent)
        } else {
            let text = line.text.clone();
            self.pos += 1;
            inline(&text)
        }
    }

    fn seq(&mut self, indent: usize) -> Yaml {
        let mut items = Vec::new();
        while let Some(line) = self.peek()
            && line.indent == indent
            && is_dash(&line.text)
        {
            let rest = line
                .text
                .get(1..)
                .unwrap_or_default()
                .trim_start()
                .to_owned();
            if rest.is_empty() {
                self.pos += 1;
                items.push(match self.peek() {
                    Some(next) if next.indent > indent => {
                        let child = next.indent;
                        self.block(child)
                    }
                    _ => Yaml::Scalar(String::new()),
                });
                continue;
            }
            // `- key: v` opens a map whose later keys sit under the first one.
            let child = indent + (line.text.len() - rest.len());
            if let Some(line) = self.lines.get_mut(self.pos) {
                line.indent = child;
                line.text = rest;
            }
            items.push(self.block(child));
        }
        Yaml::Seq(items)
    }

    fn map(&mut self, indent: usize) -> Yaml {
        let mut entries = Vec::new();
        while let Some(line) = self.peek()
            && line.indent == indent
            && !is_dash(&line.text)
        {
            let Some((key, rest)) = split_key(&line.text) else {
                self.pos += 1;
                continue;
            };
            let rest = rest.to_owned();
            self.pos += 1;
            let value = if matches!(rest.as_str(), "|" | ">" | "|-" | ">-" | "|+" | ">+") {
                self.literal(indent, rest.starts_with('|'))
            } else if rest.is_empty() {
                match self.peek() {
                    Some(next) if next.indent > indent => {
                        let child = next.indent;
                        self.block(child)
                    }
                    Some(next) if next.indent == indent && is_dash(&next.text) => self.seq(indent),
                    _ => Yaml::Scalar(String::new()),
                }
            } else {
                inline(&rest)
            };
            entries.push((key, value));
        }
        Yaml::Map(entries)
    }

    /// The more-indented lines of a `|` / `>` block scalar, preserving relative indentation.
    fn literal(&mut self, indent: usize, keep_newlines: bool) -> Yaml {
        let mut parts = Vec::new();
        let mut base_indent: Option<usize> = None;
        while let Some(line) = self.peek()
            && line.indent > indent
        {
            let base = *base_indent.get_or_insert(line.indent);
            let extra = line.indent.saturating_sub(base);
            if keep_newlines && extra > 0 {
                parts.push(format!("{}{}", " ".repeat(extra), line.text));
            } else {
                parts.push(line.text.clone());
            }
            self.pos += 1;
        }
        Yaml::Scalar(parts.join(if keep_newlines { "\n" } else { " " }))
    }
}

/// `key: rest` of a block line; `None` for a scalar or flow collection.
fn split_key(text: &str) -> Option<(String, &str)> {
    if text.starts_with(['[', '{']) {
        return None;
    }
    let mut quote: Option<char> = None;
    let bytes = text.as_bytes();
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match quote {
            Some('\'') if c == '\'' && chars.peek().is_some_and(|&(_, n)| n == '\'') => {
                chars.next();
            }
            Some('"') if c == '\\' => {
                chars.next();
            }
            Some(q) if c == q => quote = None,
            None if (c == '\'' || c == '"') && at == 0 => quote = Some(c),
            None if c == ':' && bytes.get(at + 1).is_none_or(u8::is_ascii_whitespace) => {
                let raw_key = text.get(..at)?.trim();
                let key = if raw_key.starts_with(['\'', '"']) {
                    let mut cur = Cursor { rest: raw_key };
                    cur.scalar(false, false)
                } else {
                    raw_key.to_owned()
                };
                return Some((key, text.get(at + 1..)?.trim()));
            }
            _ => {}
        }
    }
    None
}

/// The value written after `key:` on the same line.
fn inline(text: &str) -> Yaml {
    let mut cursor = Cursor { rest: text };
    cursor.value(false)
}

struct Cursor<'a> {
    rest: &'a str,
}

impl Cursor<'_> {
    fn skip_space(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn eat(&mut self, c: char) -> bool {
        self.skip_space();
        match self.rest.strip_prefix(c) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    /// One value; `nested` scalars end at a flow delimiter, top-level ones run to the line end.
    fn value(&mut self, nested: bool) -> Yaml {
        self.skip_space();
        if self.eat('[') {
            let mut items = Vec::new();
            while !self.rest.is_empty() && !self.eat(']') {
                items.push(self.value(true));
                self.eat(',');
            }
            Yaml::Seq(items)
        } else if self.eat('{') {
            let mut entries = Vec::new();
            while !self.rest.is_empty() && !self.eat('}') {
                let key = self.scalar(true, true);
                let value = if self.eat(':') {
                    self.value(true)
                } else {
                    Yaml::Scalar(String::new())
                };
                entries.push((key, value));
                self.eat(',');
            }
            Yaml::Map(entries)
        } else {
            Yaml::Scalar(self.scalar(nested, false))
        }
    }

    fn scalar(&mut self, nested: bool, is_key: bool) -> String {
        self.skip_space();
        let first = self.rest.chars().next();
        if let Some(q @ ('\'' | '"')) = first {
            return self.quoted(q);
        }
        let end = self
            .rest
            .char_indices()
            .find(|&(at, c)| {
                nested && matches!(c, ',' | ']' | '}')
                    || is_key
                        && c == ':'
                        && self.rest.get(at + 1..).is_some_and(|r| r.starts_with(' '))
            })
            .map_or(self.rest.len(), |(at, _)| at);
        let (token, rest) = self.rest.split_at(end);
        self.rest = rest;
        token.trim().to_owned()
    }

    fn quoted(&mut self, quote: char) -> String {
        let mut out = String::new();
        let mut chars = self.rest.get(1..).unwrap_or_default().char_indices();
        let mut end = self.rest.len();
        while let Some((at, c)) = chars.next() {
            if c == quote {
                if quote == '\'' && self.rest.get(at + 2..).is_some_and(|r| r.starts_with('\'')) {
                    out.push('\'');
                    chars.next();
                    continue;
                }
                end = at + 2;
                break;
            }
            if quote == '"' && c == '\\' {
                match chars.next() {
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 'r')) => out.push('\r'),
                    Some((_, 't')) => out.push('\t'),
                    Some((_, '0')) => out.push('\0'),
                    Some((_, 'u')) => {
                        let mut hex = String::with_capacity(4);
                        for _ in 0..4 {
                            if let Some((_, h)) = chars.next() {
                                hex.push(h);
                            }
                        }
                        if let Some(ch) =
                            u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            out.push(ch);
                        }
                    }
                    Some((_, other)) => out.push(other),
                    None => {}
                }
                continue;
            }
            out.push(c);
        }
        self.rest = self.rest.get(end..).unwrap_or_default();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
providers:
  local:
    # a comment
    baseUrl: https://x.example/v1
    apiKey: '!cmd ''C:/a b/t.token'' # not a comment'
    models:
      - id: example/model-pro
        name: Example Model Pro (Cloud), big # trailing
        input: [text, image]
        cost: { input: 0.5, output: 2, cacheRead: 0 }
      - id: other
        contextWindow: 1048576
  empty:
    auth: none
roles:
  - a
  - b: 1
";

    #[test]
    fn parses_nested_maps_sequences_flow_collections_and_quotes() {
        let doc = parse(SAMPLE).expect("document");
        let local = doc
            .get("providers")
            .and_then(|p| p.get("local"))
            .expect("local");
        assert_eq!(
            local.get("baseUrl").and_then(Yaml::str),
            Some("https://x.example/v1")
        );
        assert_eq!(
            local.get("apiKey").and_then(Yaml::str),
            Some("!cmd 'C:/a b/t.token' # not a comment")
        );
        let models = local.get("models").expect("models").items();
        let [first, second] = models else {
            unreachable!("two models, got {models:?}");
        };
        assert_eq!(
            first.get("id").and_then(Yaml::str),
            Some("example/model-pro")
        );
        assert_eq!(
            first.get("name").and_then(Yaml::str),
            Some("Example Model Pro (Cloud), big")
        );
        assert_eq!(first.get("input").expect("input").items().len(), 2);
        let cost = first.get("cost").expect("cost");
        assert_eq!(cost.get("input").and_then(Yaml::f64), Some(0.5));
        assert_eq!(cost.get("cacheRead").and_then(Yaml::f64), Some(0.0));
        assert_eq!(
            second.get("contextWindow").and_then(Yaml::u64),
            Some(1_048_576)
        );
        let empty = doc
            .get("providers")
            .and_then(|p| p.get("empty"))
            .expect("empty");
        assert_eq!(empty.get("auth").and_then(Yaml::str), Some("none"));
        assert_eq!(doc.get("roles").expect("roles").items().len(), 2);
    }

    #[test]
    fn a_sequence_may_sit_at_the_indent_of_its_key() {
        let doc = parse("models:\n- id: a\n- id: b\nafter: 1\n").expect("document");
        assert_eq!(doc.get("models").expect("models").items().len(), 2);
        assert_eq!(doc.get("after").and_then(Yaml::u64), Some(1));
    }

    #[test]
    fn empty_input_has_no_document() {
        assert_eq!(parse("# nothing\n\n"), None);
    }

    #[test]
    fn hardened_escapes_block_scalars_doc_markers_bools_and_quoted_keys() {
        let doc = parse(concat!(
            "---\n",
            "\"a:b\": \"say \\\"hi\\\" # keep\\r\\n\\u0041\"\n",
            "flag_t: True\n",
            "flag_f: FALSE\n",
            "nested: { arr: [1, { k: \"x,y # z\" }] }\n",
            "script: |+\n",
            "  line1\n",
            "    indented\n",
            "...\n",
        ))
        .expect("document");
        assert_eq!(
            doc.get("a:b").and_then(Yaml::str),
            Some("say \"hi\" # keep\r\nA")
        );
        assert_eq!(doc.get("flag_t").and_then(Yaml::bool), Some(true));
        assert_eq!(doc.get("flag_f").and_then(Yaml::bool), Some(false));
        assert_eq!(
            doc.get("nested")
                .and_then(|n| n.get("arr"))
                .and_then(|a| a.items().get(1))
                .and_then(|m| m.get("k"))
                .and_then(Yaml::str),
            Some("x,y # z")
        );
        assert_eq!(
            doc.get("script").and_then(Yaml::str),
            Some("line1\n  indented")
        );
    }
}
