//! Tiny XML event walker for the Office and EPUB extractors: elements, attributes and decoded
//! text. Comments, processing instructions and DOCTYPEs are skipped; CDATA is text.

/// One XML event; an empty element yields `Open` then `Close`.
pub(super) enum Ev<'a> {
    /// Element name (with prefix) and the raw attribute text.
    Open(&'a str, &'a str),
    Close(&'a str),
    Text(String),
}

/// Feed every event of `xml` to `f`.
pub(super) fn walk<'a>(xml: &'a str, mut f: impl FnMut(Ev<'a>)) {
    let mut rest = xml;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            f(Ev::Text(decode(rest)));
            return;
        };
        if lt > 0 {
            f(Ev::Text(decode(rest.get(..lt).unwrap_or_default())));
        }
        rest = rest.get(lt..).unwrap_or_default();
        if let Some(body) = rest.strip_prefix("<![CDATA[") {
            let end = body.find("]]>").unwrap_or(body.len());
            f(Ev::Text(body.get(..end).unwrap_or_default().to_owned()));
            rest = body.get(end + 3..).unwrap_or_default();
            continue;
        }
        let skip_to = if rest.starts_with("<!--") {
            Some("-->")
        } else if rest.starts_with("<?") {
            Some("?>")
        } else if rest.starts_with("<!") {
            Some(">")
        } else {
            None
        };
        if let Some(end) = skip_to {
            rest = rest
                .find(end)
                .and_then(|i| rest.get(i + end.len()..))
                .unwrap_or_default();
            continue;
        }
        let Some(gt) = rest.find('>') else { return };
        let tag = rest.get(1..gt).unwrap_or_default();
        rest = rest.get(gt + 1..).unwrap_or_default();
        if let Some(name) = tag.strip_prefix('/') {
            f(Ev::Close(name.trim()));
            continue;
        }
        let empty = tag.ends_with('/');
        let tag = tag.strip_suffix('/').unwrap_or(tag);
        let (name, attrs) = tag
            .split_once(char::is_whitespace)
            .map_or((tag, ""), |(n, a)| (n, a));
        f(Ev::Open(name, attrs));
        if empty {
            f(Ev::Close(name));
        }
    }
}

/// Value of attribute `name` in the raw attribute text of a start tag.
pub(super) fn attr(attrs: &str, name: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(rel) = attrs.get(from..)?.find(name) {
        let at = from + rel;
        let end = at + name.len();
        let boundary = at == 0
            || attrs
                .get(..at)
                .and_then(|s| s.chars().next_back())
                .is_some_and(char::is_whitespace);
        let tail = attrs.get(end..)?.trim_start();
        if boundary && let Some(value) = tail.strip_prefix('=') {
            let value = value.trim_start();
            let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let body = value.get(1..)?;
            return body
                .find(quote)
                .map(|i| decode(body.get(..i).unwrap_or_default()));
        }
        from = end;
    }
    None
}

/// Decode the five predefined entities and numeric character references.
pub(super) fn decode(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(rest.get(..amp).unwrap_or_default());
        rest = rest.get(amp..).unwrap_or_default();
        let decoded = rest
            .find(';')
            .filter(|semi| *semi <= 10)
            .and_then(|semi| entity(rest.get(1..semi)?).map(|c| (c, semi)));
        if let Some((ch, semi)) = decoded {
            out.push(ch);
            rest = rest.get(semi + 1..).unwrap_or_default();
        } else {
            out.push('&');
            rest = rest.get(1..).unwrap_or_default();
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}
