//! Minimal HTML → readable text conversion for `web_fetch` and search-result scraping.

/// Elements that end a line of text.
fn is_block_tag(name: &str) -> bool {
    let name = name.trim_start_matches('/').trim_end_matches('/');
    matches!(
        name,
        "p" | "br"
            | "div"
            | "li"
            | "ul"
            | "ol"
            | "tr"
            | "table"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "nav"
            | "pre"
            | "blockquote"
            | "hr"
            | "title"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
    )
}

/// Drop every `open ... close` span (ASCII case-insensitive); an unterminated span swallows the
/// rest of the input.
fn strip_spans(input: &str, open: &str, close: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0usize;
    while let Some(rel) = lower.get(cursor..).and_then(|s| s.find(open)) {
        let start = cursor + rel;
        out.push_str(input.get(cursor..start).unwrap_or_default());
        match lower.get(start..).and_then(|s| s.find(close)) {
            Some(end) => cursor = start + end + close.len(),
            None => return out,
        }
    }
    out.push_str(input.get(cursor..).unwrap_or_default());
    out
}

/// Single-pass HTML entity decoder for common named entities and numeric (`&#NNN;` / `&#xHH;`)
/// character references. Never double-decodes (`&amp;lt;` -> `&lt;`, `&amp;#65;` -> `&#65;`).
pub(crate) fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or_default());
        rest = rest.get(at..).unwrap_or_default();
        let decoded = rest.find(';').filter(|&end| end <= 12).and_then(|end| {
            let body = rest.get(1..end)?;
            let ch = match body {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                "hellip" => '…',
                "ndash" => '–',
                "mdash" => '—',
                num => {
                    let raw = num.strip_prefix('#')?;
                    let code = if let Some(hex) = raw.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok()?
                    } else {
                        raw.parse::<u32>().ok()?
                    };
                    char::from_u32(code)?
                }
            };
            Some((ch, end))
        });
        if let Some((ch, end)) = decoded {
            out.push(ch);
            rest = rest.get(end + 1..).unwrap_or_default();
        } else {
            out.push('&');
            rest = rest.get(1..).unwrap_or_default();
        }
    }
    out.push_str(rest);
    out
}

/// Convert an HTML document to plain text: scripts, styles, SVGs, templates, and comments removed,
/// tags dropped (respecting quoted attribute values containing `>`), block elements turned into
/// line breaks, whitespace collapsed.
pub fn to_text(html: &str) -> String {
    let mut stripped = html.to_owned();
    for (open, close) in [
        ("<!--", "-->"),
        ("<script", "</script>"),
        ("<style", "</style>"),
        ("<noscript", "</noscript>"),
        ("<svg", "</svg>"),
        ("<template", "</template>"),
    ] {
        stripped = strip_spans(&stripped, open, close);
    }

    let mut text = String::with_capacity(stripped.len() / 2);
    let mut chars = stripped.chars();
    while let Some(ch) = chars.next() {
        // A lone `<` in prose ("a < b") is text, not a tag.
        let starts_tag = ch == '<'
            && chars
                .clone()
                .next()
                .is_some_and(|n| n.is_ascii_alphabetic() || n == '/' || n == '!');
        if !starts_tag {
            text.push(ch);
            continue;
        }
        let mut name = String::new();
        let mut in_name = true;
        let mut quote: Option<char> = None;
        for c in chars.by_ref() {
            match quote {
                Some(q) if c == q => quote = None,
                None if c == '>' => break,
                None if !in_name && matches!(c, '"' | '\'') => quote = Some(c),
                None if in_name => {
                    if c.is_whitespace() {
                        in_name = false;
                    } else {
                        name.push(c.to_ascii_lowercase());
                    }
                }
                _ => {}
            }
        }
        if is_block_tag(&name) {
            text.push('\n');
        }
    }

    let decoded = unescape(&text);
    let mut lines: Vec<String> = Vec::new();
    for line in decoded.lines() {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() && lines.last().is_none_or(String::is_empty) {
            continue;
        }
        lines.push(collapsed);
    }
    lines.join("\n").trim().to_owned()
}

/// Text of a small inline fragment as a single line.
pub(crate) fn fragment_text(fragment: &str) -> String {
    to_text(fragment).replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_scripts_keeps_blocks_and_decodes_entities_once() {
        let html = "<html><head><style>p{}</style><script>alert('</p>')</script></head>\
                    <body><!-- hi > there --><p class=\"x\">Hello &amp; world</p>\
                    <p>1 < 2 &amp;lt;</p></body></html>";
        assert_eq!(to_text(html), "Hello & world\n\n1 < 2 &lt;");
    }

    #[test]
    fn handles_quoted_gt_numeric_entities_svg_and_template() {
        let html = "<svg><text>ignored</text></svg><template><p>hidden</p></template>\
                    <p><a title=\"a > b\" data-x='1 > 0'>Click</a> &#65;&#x42;&apos;s &amp;#65;</p>";
        assert_eq!(to_text(html), "Click AB's &#65;");
    }
}
