//! Block detection for `N*` anchors: a small bracket scanner for brace languages, indentation
//! for Python/YAML-like text, and heading sections for Markdown. Not a parser: it only has to
//! find where the construct opening on a line ends.

/// Comment and literal conventions of the file being edited, chosen by extension.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Syntax {
    /// `#` starts a comment instead of `//` and `/* */`.
    hash_comment: bool,
    /// `'` is a char literal or a lifetime rather than a string quote.
    rust: bool,
    markdown: bool,
}

impl Syntax {
    pub(super) fn for_path(path: &str) -> Self {
        let ext = path
            .rsplit_once('.')
            .map_or("", |(_, e)| e)
            .to_ascii_lowercase();
        Self {
            hash_comment: matches!(
                ext.as_str(),
                "py" | "pyi" | "sh" | "bash" | "zsh" | "rb" | "yaml" | "yml" | "toml" | "pl" | "r"
            ),
            rust: ext == "rs",
            markdown: matches!(ext.as_str(), "md" | "markdown" | "mdx"),
        }
    }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_decorator(trimmed: &str) -> bool {
    trimmed.starts_with('@') || trimmed.starts_with("#[") || trimmed.starts_with("///")
}

/// Bracket scanner state carried across the lines of one scan.
#[derive(Default)]
struct Scan {
    depth: i32,
    opened: bool,
    /// Inside `/* ... */`.
    block_comment: bool,
}

impl Scan {
    /// Feed one line; brackets inside strings and comments are ignored.
    fn line(&mut self, text: &str, syn: Syntax) {
        let b = text.as_bytes();
        let at = |i: usize| b.get(i).copied().unwrap_or(0);
        let mut i = 0;
        while i < b.len() {
            let c = at(i);
            if self.block_comment {
                if c == b'*' && at(i + 1) == b'/' {
                    self.block_comment = false;
                    i += 1;
                }
            } else {
                match c {
                    b'#' if syn.hash_comment => return,
                    b'/' if !syn.hash_comment && at(i + 1) == b'/' => return,
                    b'/' if !syn.hash_comment && at(i + 1) == b'*' => {
                        self.block_comment = true;
                        i += 1;
                    }
                    b'\'' if syn.rust => {
                        // `'x'` / `'\n'` are char literals; `'a` is a lifetime.
                        if at(i + 1) == b'\\' {
                            i += 2;
                            while i < b.len() && at(i) != b'\'' {
                                i += 1;
                            }
                        } else if at(i + 2) == b'\'' {
                            i += 2;
                        }
                    }
                    b'"' | b'\'' | b'`' => {
                        i += 1;
                        while i < b.len() && at(i) != c {
                            if at(i) == b'\\' {
                                i += 1;
                            }
                            i += 1;
                        }
                    }
                    b'{' | b'[' | b'(' => {
                        self.depth += 1;
                        self.opened = true;
                    }
                    b'}' | b']' | b')' => self.depth -= 1,
                    _ => {}
                }
            }
            i += 1;
        }
    }
}

/// End line (1-based) of the bracket group opened at `start`, `None` when the line opens no
/// multi-line group. `Err` when brackets never balance.
fn brace_end(lines: &[String], start: usize, syn: Syntax) -> Result<Option<usize>, String> {
    let mut scan = Scan::default();
    // First line at which a balanced group may end the block (later than `start` for Allman).
    let mut gate = start;
    for (i, text) in lines.iter().enumerate().skip(start - 1) {
        let n = i + 1;
        scan.line(text, syn);
        if n == start && scan.depth <= 0 {
            // Allman style: the `{` sits alone on the next non-blank line.
            let brace = lines
                .iter()
                .enumerate()
                .skip(n)
                .find(|(_, l)| !l.trim().is_empty())
                .filter(|(_, l)| {
                    l.trim_start().starts_with('{') && !text.trim_end().ends_with(';')
                });
            match brace {
                Some((j, _)) => gate = j + 1,
                None => return Ok(None),
            }
        } else if scan.opened && scan.depth <= 0 && n >= gate {
            return Ok(Some(n));
        }
    }
    if scan.opened {
        Err(format!(
            "brackets opened on line {start} never close before the end of the file"
        ))
    } else {
        Ok(None)
    }
}

/// End line of the deeper-indented run that follows `start`, `None` when there is none.
fn indent_end(lines: &[String], start: usize) -> Option<usize> {
    let base = indent_of(lines.get(start - 1)?);
    let mut end = None;
    for (i, text) in lines.iter().enumerate().skip(start) {
        if text.trim().is_empty() {
            continue;
        }
        if indent_of(text) <= base {
            break;
        }
        end = Some(i + 1);
    }
    end
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    (1..=6)
        .contains(&hashes)
        .then(|| line.get(hashes..))
        .flatten()
        .filter(|rest| rest.starts_with(' '))
        .map(|_| hashes)
}

fn markdown_end(lines: &[String], start: usize, level: usize) -> usize {
    let mut fence = false;
    let mut end = start;
    for (i, text) in lines.iter().enumerate().skip(start) {
        if text.trim_start().starts_with("```") || text.trim_start().starts_with("~~~") {
            fence = !fence;
        } else if !fence && heading_level(text).is_some_and(|l| l <= level) {
            break;
        }
        if !text.trim().is_empty() {
            end = i + 1;
        }
    }
    end
}

/// The 1-based end line of the block opening on line `n` (always `> n`), or why there is none.
pub(super) fn resolve(lines: &[String], n: usize, syn: Syntax) -> Result<usize, String> {
    let single = |line: usize| {
        format!(
            "line {line} opens no multi-line block; use the explicit-line form (`N.=N` / `N.=M`) instead of `N*`"
        )
    };
    let first = lines.get(n - 1).ok_or_else(|| {
        format!(
            "line {n} is past the end of the file ({} lines)",
            lines.len()
        )
    })?;
    let trimmed = first.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "line {n} is blank; anchor the first line of the construct (its opener)"
        ));
    }
    if trimmed
        .chars()
        .all(|c| matches!(c, '}' | ')' | ']' | ';' | ','))
    {
        return Err(format!(
            "line {n} is a closing delimiter; anchor the line that opens the construct"
        ));
    }
    if syn.markdown
        && let Some(level) = heading_level(first)
    {
        let end = markdown_end(lines, n, level);
        return if end > n { Ok(end) } else { Err(single(n)) };
    }
    // Decorators, attributes and doc comments belong to the item that follows them.
    let mut opener = n;
    while lines
        .get(opener - 1)
        .is_some_and(|l| is_decorator(l.trim()))
    {
        opener += 1;
    }
    if lines.get(opener - 1).is_none_or(|l| l.trim().is_empty()) {
        return Err(format!(
            "no declaration follows the attributes starting on line {n}; use an explicit range"
        ));
    }
    if let Some(end) = brace_end(lines, opener, syn)? {
        return Ok(end);
    }
    if let Some(end) = indent_end(lines, opener) {
        return Ok(end);
    }
    Err(single(opener))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    fn end(text: &str, n: usize, path: &str) -> Result<usize, String> {
        resolve(&lines(text), n, Syntax::for_path(path))
    }

    #[test]
    fn brace_blocks_follow_nesting_else_and_strings() {
        let src = "fn a() {\n    let s = \"}\"; // }\n    if x {\n        y();\n    } else {\n        z();\n    }\n}\nfn b() {}\n";
        assert_eq!(end(src, 1, "a.rs"), Ok(8));
        assert_eq!(end(src, 3, "a.rs"), Ok(7));
        assert!(end(src, 9, "a.rs").unwrap_err().contains("explicit-line"));
        assert!(end(src, 8, "a.rs").unwrap_err().contains("closing"));
        let lifetimes = "fn f<'a>(x: &'a str) -> char {\n    '{'\n}\n";
        assert_eq!(end(lifetimes, 1, "a.rs"), Ok(3));
    }

    #[test]
    fn attributes_and_decorators_resolve_through_the_item() {
        let rs = "/// doc\n#[derive(Debug)]\nstruct S {\n    a: u8,\n}\n";
        assert_eq!(end(rs, 1, "a.rs"), Ok(5));
        let py = "@cache\ndef greet(name):\n    print(name)\n\ngreet(1)\n";
        assert_eq!(end(py, 1, "a.py"), Ok(3));
    }

    #[test]
    fn indentation_blocks_and_multiline_calls() {
        let py = "class A:\n    def f(self):\n        return {\n            1: 2,\n        }\n\n    x = 1\ny = 2\n";
        assert_eq!(end(py, 1, "a.py"), Ok(7));
        assert_eq!(end(py, 2, "a.py"), Ok(5));
    }

    #[test]
    fn allman_braces_and_markdown_sections() {
        let c = "int main()\n{\n    return 0;\n}\n";
        assert_eq!(end(c, 1, "a.c"), Ok(4));
        let md = "# A\ntext\n## B\nmore\n\n# C\n```\n# not a heading\n```\n";
        assert_eq!(end(md, 1, "a.md"), Ok(4));
        assert_eq!(end(md, 3, "a.md"), Ok(4));
        assert_eq!(end(md, 6, "a.md"), Ok(9));
    }

    #[test]
    fn unbalanced_brackets_are_reported() {
        assert!(
            end("fn a() {\n  x\n", 1, "a.rs")
                .unwrap_err()
                .contains("never close")
        );
    }
}
