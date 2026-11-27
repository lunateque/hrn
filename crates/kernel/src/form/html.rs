//! A tolerant tokenizer for the HTML subset `ask` forms use: tags with attributes, text with
//! entities, comments. `<script>` and `<style>` are swallowed whole; `<textarea>` content is raw.

/// One lexical piece of the document.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Tok {
    /// `<name attr=…>`; names and attribute names are lower-cased.
    Open(String, Attrs),
    /// `</name>`.
    Close(String),
    /// Text between tags, entities decoded, whitespace untouched.
    Text(String),
}

/// The attributes of one start tag, in source order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Attrs(Vec<(String, String)>);

impl Attrs {
    pub(super) fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The attribute is present (a boolean attribute such as `checked`).
    pub(super) fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// The attribute's value when it is present and not blank.
    pub(super) fn text(&self, name: &str) -> Option<String> {
        self.get(name)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }
}

pub(super) fn tokenize(src: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut rest = src;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            out.push(Tok::Text(decode(rest)));
            break;
        };
        if lt > 0 {
            out.push(Tok::Text(decode(&rest[..lt])));
        }
        rest = &rest[lt..];
        let bytes = rest.as_bytes();
        let next = bytes.get(1).copied().unwrap_or(b' ');
        if rest.starts_with("<!--") {
            rest = after(rest, "-->", 4);
        } else if next == b'!' || next == b'?' {
            rest = after(rest, ">", 2);
        } else if next == b'/' && bytes.get(2).is_some_and(u8::is_ascii_alphabetic) {
            let end = rest.find('>').unwrap_or(rest.len());
            out.push(Tok::Close(rest[2..end].trim().to_ascii_lowercase()));
            rest = rest.get(end + 1..).unwrap_or("");
        } else if next.is_ascii_alphabetic() {
            let (name, attrs, tail) = open_tag(&rest[1..]);
            rest = tail;
            match name.as_str() {
                "script" | "style" => rest = after_close(rest, &name),
                "textarea" => {
                    let end = find_ci(rest, "</textarea").unwrap_or(rest.len());
                    out.push(Tok::Open(name, attrs));
                    out.push(Tok::Text(decode(&rest[..end])));
                    rest = &rest[end..];
                }
                _ => out.push(Tok::Open(name, attrs)),
            }
        } else {
            out.push(Tok::Text("<".to_owned()));
            rest = &rest[1..];
        }
    }
    out
}

/// `text` after the first `needle`, searching from `skip` (everything when it never appears).
fn after<'a>(text: &'a str, needle: &str, skip: usize) -> &'a str {
    text.get(skip..)
        .and_then(|tail| tail.find(needle).map(|at| &tail[at + needle.len()..]))
        .unwrap_or("")
}

/// `text` after the `</name>` that closes a swallowed element.
fn after_close<'a>(text: &'a str, name: &str) -> &'a str {
    match find_ci(text, &format!("</{name}")) {
        Some(at) => after(&text[at..], ">", 1),
        None => "",
    }
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let (hay, needle) = (hay.as_bytes(), needle.as_bytes());
    hay.windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

/// Parse `name attr…>` (the text after `<`), returning the unread tail past the `>`.
fn open_tag(src: &str) -> (String, Attrs, &str) {
    let name_end = src
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(src.len());
    let name = src[..name_end].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut rest = &src[name_end..];
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        let Some(first) = rest.chars().next() else {
            return (name, Attrs(attrs), "");
        };
        if first == '>' {
            return (name, Attrs(attrs), &rest[1..]);
        }
        let key_end = rest
            .find(|c: char| c.is_whitespace() || c == '=' || c == '>' || c == '/')
            .unwrap_or(rest.len());
        let key = rest[..key_end].to_ascii_lowercase();
        rest = rest[key_end..].trim_start();
        let mut value = String::new();
        if let Some(after_eq) = rest.strip_prefix('=') {
            let after_eq = after_eq.trim_start();
            let (raw, tail) =
                if let Some(quote) = after_eq.chars().next().filter(|c| matches!(c, '"' | '\'')) {
                    let body = &after_eq[1..];
                    let end = body.find(quote).unwrap_or(body.len());
                    (&body[..end], body.get(end + 1..).unwrap_or(""))
                } else {
                    let end = after_eq
                        .find(|c: char| c.is_whitespace() || c == '>')
                        .unwrap_or(after_eq.len());
                    (&after_eq[..end], &after_eq[end..])
                };
            value = decode(raw);
            rest = tail;
        }
        if !key.is_empty() {
            attrs.push((key, value));
        }
    }
}

/// Decode the entities forms use: the named basics and `&#N;` / `&#xH;`. Unknown ones stay.
fn decode(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest.find(';').filter(|end| *end <= 10).and_then(|end| {
            let ch = match &rest[1..end] {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => '\u{a0}',
                "hellip" => '…',
                "ndash" => '–',
                "mdash" => '—',
                num => {
                    let code = match num.strip_prefix('#')? {
                        hex if hex.starts_with(['x', 'X']) => u32::from_str_radix(&hex[1..], 16),
                        dec => dec.parse(),
                    };
                    char::from_u32(code.ok()?)?
                }
            };
            Some((ch, end))
        });
        if let Some((ch, end)) = decoded {
            out.push(ch);
            rest = &rest[end + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_attributes_and_entities() {
        let toks = tokenize(
            "<!-- c --><P Class=a>x &amp; &#65;&#x42; &bogus;</p><input name='n' checked value=\"a b\"/>",
        );
        assert_eq!(
            toks[0],
            Tok::Open("p".into(), Attrs(vec![("class".into(), "a".into())]))
        );
        assert_eq!(toks[1], Tok::Text("x & AB &bogus;".into()));
        assert_eq!(toks[2], Tok::Close("p".into()));
        let Tok::Open(name, attrs) = &toks[3] else {
            panic!("input tag");
        };
        assert_eq!(name, "input");
        assert_eq!(attrs.get("value"), Some("a b"));
        assert!(attrs.has("checked"));
        assert_eq!(attrs.get("name"), Some("n"));
    }

    #[test]
    fn script_is_swallowed_and_textarea_is_raw() {
        let toks = tokenize("a<script>if (a<b) {}</script>b<textarea><b>x</b></textarea>");
        assert_eq!(toks[0], Tok::Text("a".into()));
        assert_eq!(toks[1], Tok::Text("b".into()));
        assert_eq!(toks[3], Tok::Text("<b>x</b>".into()));
        assert_eq!(toks[4], Tok::Close("textarea".into()));
    }

    #[test]
    fn stray_angle_bracket_is_text() {
        let toks = tokenize("1 < 2 <b>");
        assert_eq!(toks[1], Tok::Text("<".into()));
    }

    #[test]
    fn unclosed_comment_and_quoted_gt_in_attr_do_not_panic() {
        let toks = tokenize("<input title=\"a > b\" value='&amp;#65;'>ok<!-- unclosed");
        let Tok::Open(name, attrs) = &toks[0] else {
            panic!("input");
        };
        assert_eq!(name, "input");
        assert_eq!(attrs.get("title"), Some("a > b"));
        assert_eq!(attrs.get("value"), Some("&#65;"));
        assert_eq!(toks[1], Tok::Text("ok".into()));
    }
}
