//! Syntax highlighting for code blocks, file excerpts and diffs: a one-pass lexer per language
//! family with keyword tables instead of grammars, painted in the titanium syntax palette.
//! Identifiers take the bright text color; whitespace and unknown characters keep the caller's
//! base style. Unknown languages stay plain.

use crate::theme;
use crate::wrap::Seg;
use ratatui::style::{Color, Style};

mod words;
use words::{
    C_KEYWORDS, C_TYPES, GO_KEYWORDS, GO_TYPES, JS_KEYWORDS, JS_TYPES, PYTHON_KEYWORDS,
    PYTHON_TYPES, RUST_KEYWORDS, RUST_TYPES, SHELL_KEYWORDS,
};

const COMMENT: Color = theme::SYN_COMMENT;
const KEYWORD: Color = theme::SYN_KEYWORD;
const FUNCTION: Color = theme::SYN_FUNCTION;
const VARIABLE: Color = theme::SYN_VARIABLE;
const STRING: Color = theme::SYN_STRING;
const NUMBER: Color = theme::SYN_NUMBER;
const TYPE: Color = theme::SYN_TYPE;
const OPERATOR: Color = theme::SYN_OPERATOR;
const PUNCTUATION: Color = theme::SYN_PUNCTUATION;

const OPERATORS: &str = "+-*/%=&|<>!^~?:";
const PUNCTUATIONS: &str = "()[]{},.;";

/// A language family the lexer knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lang {
    Rust,
    /// C, C++, Java, C#, Kotlin, Swift and their kin.
    C,
    Go,
    /// JavaScript and TypeScript.
    Js,
    Python,
    /// POSIX shells and PowerShell.
    Shell,
    Json,
    /// TOML and INI.
    Toml,
    Yaml,
    Diff,
}

/// The language of a fence tag or file extension (`rs`, `typescript`, `sh`…).
pub(crate) fn lang(tag: &str) -> Option<Lang> {
    Some(match tag.trim().to_ascii_lowercase().as_str() {
        "rs" | "rust" => Lang::Rust,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "c++" | "java" | "cs" | "csharp"
        | "kt" | "kts" | "kotlin" | "swift" | "scala" | "dart" | "zig" | "proto" => Lang::C,
        "go" | "golang" => Lang::Go,
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "javascript"
        | "typescript" => Lang::Js,
        "py" | "pyi" | "python" => Lang::Python,
        "sh" | "bash" | "zsh" | "fish" | "shell" | "console" | "ps1" | "psm1" | "powershell"
        | "pwsh" | "bat" | "cmd" => Lang::Shell,
        "json" | "jsonc" | "json5" | "jsonl" => Lang::Json,
        "toml" | "ini" | "cfg" | "conf" | "lock" => Lang::Toml,
        "yaml" | "yml" => Lang::Yaml,
        "diff" | "patch" => Lang::Diff,
        _ => return None,
    })
}

/// The language of a file, from its extension.
pub(crate) fn lang_for_path(path: &str) -> Option<Lang> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name {
        ".bashrc" | ".zshrc" | ".profile" | "Dockerfile" => Some(Lang::Shell),
        _ => lang(name.rsplit_once('.')?.1),
    }
}

/// What a line leaves open for the next one.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct State {
    block_comment: bool,
    string: Option<&'static str>,
}

/// Highlight `text` line by line; one segment list per line (a trailing newline adds none).
pub(crate) fn lines(text: &str, lang: Option<Lang>, base: Style) -> Vec<Vec<Seg>> {
    let mut state = State::default();
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.split('\n')
        .map(|line| self::line(line, lang, &mut state, base))
        .collect()
}

/// Highlight one line, continuing whatever `state` left open.
pub(crate) fn line(line: &str, lang: Option<Lang>, state: &mut State, base: Style) -> Vec<Seg> {
    let mut out = Out {
        segs: Vec::new(),
        base,
    };
    match lang {
        None => out.push(line, None),
        Some(Lang::Shell) => shell(line, state, &mut out),
        Some(Lang::Json | Lang::Toml | Lang::Yaml) => data(line, lang, &mut out),
        Some(Lang::Diff) => diff(line, &mut out),
        Some(code_lang) => code(line, &spec(code_lang), state, &mut out),
    }
    out.segs
}

struct Out {
    segs: Vec<Seg>,
    base: Style,
}

impl Out {
    fn push(&mut self, text: &str, color: Option<Color>) {
        if text.is_empty() {
            return;
        }
        let style = color.map_or(self.base, |c| self.base.fg(c));
        match self.segs.last_mut() {
            Some((last, last_style)) if *last_style == style => last.push_str(text),
            _ => self.segs.push((text.to_owned(), style)),
        }
    }
}

fn split(text: &str, at: usize) -> (&str, &str) {
    text.split_at_checked(at).unwrap_or((text, ""))
}

/// The byte length of the leading run of `text` whose chars satisfy `keep`.
fn run(text: &str, keep: impl Fn(char) -> bool) -> usize {
    text.find(|c| !keep(c)).unwrap_or(text.len())
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The end (exclusive) of a quoted run that starts after its opening `delim`, honoring
/// backslash escapes unless `raw`; `None` when the line ends first.
fn close_quote(text: &str, delim: &str, raw: bool) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' && !raw {
            escaped = true;
        } else if text.get(i..).is_some_and(|rest| rest.starts_with(delim)) {
            return Some(i + delim.len());
        }
    }
    None
}

struct Spec {
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    line_comment: &'static str,
    block_comment: Option<(&'static str, &'static str)>,
    /// Quotes whose strings may continue on the next line.
    multiline: &'static [&'static str],
    lifetimes: bool,
}

fn spec(lang: Lang) -> Spec {
    let c_like = |keywords, types| Spec {
        keywords,
        types,
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
        multiline: &[],
        lifetimes: false,
    };
    match lang {
        Lang::Rust => Spec {
            multiline: &["\""],
            lifetimes: true,
            ..c_like(RUST_KEYWORDS, RUST_TYPES)
        },
        Lang::Go => Spec {
            multiline: &["`"],
            ..c_like(GO_KEYWORDS, GO_TYPES)
        },
        Lang::Js => Spec {
            multiline: &["`"],
            ..c_like(JS_KEYWORDS, JS_TYPES)
        },
        Lang::Python => Spec {
            line_comment: "#",
            block_comment: None,
            multiline: &["\"\"\"", "'''"],
            ..c_like(PYTHON_KEYWORDS, PYTHON_TYPES)
        },
        _ => c_like(C_KEYWORDS, C_TYPES),
    }
}

fn code(line: &str, spec: &Spec, state: &mut State, out: &mut Out) {
    let mut rest = line;
    while !rest.is_empty() {
        if state.block_comment {
            let close = spec.block_comment.map_or("*/", |(_, close)| close);
            let Some(end) = rest.find(close) else {
                out.push(rest, Some(COMMENT));
                return;
            };
            let (comment, after) = split(rest, end + close.len());
            out.push(comment, Some(COMMENT));
            state.block_comment = false;
            rest = after;
            continue;
        }
        if let Some(delim) = state.string {
            let Some(end) = close_quote(rest, delim, delim != "\"") else {
                out.push(rest, Some(STRING));
                return;
            };
            let (string, after) = split(rest, end);
            out.push(string, Some(STRING));
            state.string = None;
            rest = after;
            continue;
        }
        rest = code_token(rest, spec, state, out);
    }
}

/// Paint the token at the start of `rest`; returns what follows it.
fn code_token<'a>(rest: &'a str, spec: &Spec, state: &mut State, out: &mut Out) -> &'a str {
    let Some(c) = rest.chars().next() else {
        return rest;
    };
    if c.is_whitespace() {
        let (space, after) = split(rest, run(rest, char::is_whitespace));
        out.push(space, None);
        return after;
    }
    if !spec.line_comment.is_empty() && rest.starts_with(spec.line_comment) {
        out.push(rest, Some(COMMENT));
        return "";
    }
    if let Some((open, _)) = spec.block_comment
        && rest.starts_with(open)
    {
        let (comment, after) = split(rest, open.len());
        out.push(comment, Some(COMMENT));
        state.block_comment = true;
        return after;
    }
    if let Some(&delim) = spec.multiline.iter().find(|d| rest.starts_with(**d)) {
        let (open, after) = split(rest, delim.len());
        out.push(open, Some(STRING));
        state.string = Some(delim);
        return after;
    }
    if c == '\'' && spec.lifetimes {
        let name = run(rest.get(1..).unwrap_or(""), is_ident);
        if name > 0
            && rest
                .get(1 + name..)
                .is_none_or(|after| !after.starts_with('\''))
        {
            let (lifetime, after) = split(rest, 1 + name);
            out.push(lifetime, Some(TYPE));
            return after;
        }
    }
    if matches!(c, '"' | '\'' | '`') {
        let (quote, body) = split(rest, 1);
        let end = close_quote(body, quote, false).map_or(rest.len(), |end| end + 1);
        let (string, after) = split(rest, end);
        out.push(string, Some(STRING));
        return after;
    }
    if c.is_ascii_digit() {
        let (number, after) = split(rest, number_len(rest));
        out.push(number, Some(NUMBER));
        return after;
    }
    if is_ident(c) || c == '$' || c == '@' {
        let len = 1 + run(rest.get(1..).unwrap_or(""), is_ident);
        let (word, after) = split(rest, len);
        out.push(word, Some(word_color(word, after, spec)));
        return after;
    }
    let len = c.len_utf8();
    let (symbol, after) = split(rest, len);
    let color = if OPERATORS.contains(c) {
        Some(OPERATOR)
    } else if PUNCTUATIONS.contains(c) {
        Some(PUNCTUATION)
    } else {
        None
    };
    out.push(symbol, color);
    after
}

/// The byte length of the number literal at the start of `rest` (`0x1f`, `1_000`, `2.5e3`,
/// `10u32`); a `.` belongs to it only when a digit follows.
fn number_len(rest: &str) -> usize {
    let mut len = 0;
    for (i, ch) in rest.char_indices() {
        let fraction = ch == '.'
            && rest
                .get(i + 1..)
                .and_then(|r| r.chars().next())
                .is_some_and(|n| n.is_ascii_digit());
        if !(is_ident(ch) || fraction) {
            break;
        }
        len = i + ch.len_utf8();
    }
    len
}

/// Keywords and known types by table, calls (and Rust macros) by the `(`/`!` after them,
/// decorators by their `@`, other capitalized names as types.
fn word_color(word: &str, after: &str, spec: &Spec) -> Color {
    let called = after.starts_with('(') || (spec.lifetimes && after.starts_with('!'));
    if spec.keywords.contains(&word) {
        KEYWORD
    } else if spec.types.contains(&word) {
        TYPE
    } else if called || word.starts_with('@') {
        FUNCTION
    } else if word.starts_with(char::is_uppercase) {
        TYPE
    } else {
        VARIABLE
    }
}

/// Shell: the word in command position is a function call, flags and arguments are plain,
/// `$vars` stand out, `|`, `&&`, `;` and redirections are operators.
fn shell(line: &str, state: &mut State, out: &mut Out) {
    let mut rest = line;
    let mut command = true;
    while !rest.is_empty() {
        if let Some(delim) = state.string {
            let Some(end) = close_quote(rest, delim, delim == "'") else {
                out.push(rest, Some(STRING));
                return;
            };
            let (string, after) = split(rest, end);
            out.push(string, Some(STRING));
            state.string = None;
            rest = after;
            continue;
        }
        let Some(c) = rest.chars().next() else {
            break;
        };
        let len = if c.is_whitespace() {
            let len = run(rest, char::is_whitespace);
            out.push(split(rest, len).0, None);
            len
        } else if c == '#' {
            out.push(rest, Some(COMMENT));
            rest.len()
        } else if c == '"' || c == '\'' {
            let delim = if c == '"' { "\"" } else { "'" };
            let body = rest.get(1..).unwrap_or("");
            if let Some(end) = close_quote(body, delim, c == '\'') {
                out.push(split(rest, end + 1).0, Some(STRING));
                command = false;
                end + 1
            } else {
                state.string = Some(delim);
                out.push(rest, Some(STRING));
                rest.len()
            }
        } else if c == '$' {
            let body = rest.get(1..).unwrap_or("");
            let len = if body.starts_with('(') {
                command = true;
                2
            } else if body.starts_with('{') {
                body.find('}').map_or(rest.len(), |end| end + 2)
            } else {
                1 + run(body, |ch| is_ident(ch) || "?#@!$*-".contains(ch)).max(1)
            }
            .min(rest.len());
            out.push(split(rest, len).0, Some(TYPE));
            len
        } else if "|&;<>(){}!".contains(c) {
            let len = run(rest, |ch| "|&;<>(){}!".contains(ch));
            let ops = split(rest, len).0;
            command = !ops.contains('>') && !ops.contains('<') && !ops.ends_with(')');
            out.push(ops, Some(OPERATOR));
            len
        } else {
            let len = run(rest, |ch| {
                !ch.is_whitespace() && !"|&;<>(){}\"'$".contains(ch)
            });
            let word = split(rest, len).0;
            let color = if command && SHELL_KEYWORDS.contains(&word) {
                KEYWORD
            } else if command && word.contains('=') {
                VARIABLE
            } else if command {
                command = false;
                FUNCTION
            } else if word.chars().all(|ch| ch.is_ascii_digit()) {
                NUMBER
            } else {
                VARIABLE
            };
            out.push(word, Some(color));
            len.max(1)
        };
        rest = split(rest, len).1;
    }
}

/// JSON, TOML and YAML: keys in accent, strings, numbers and literals in their colors.
fn data(line: &str, lang: Option<Lang>, out: &mut Out) {
    let trimmed = line.trim_start();
    let indent = split(line, line.len() - trimmed.len()).0;
    out.push(indent, None);
    let comment = match lang {
        Some(Lang::Json) => None,
        Some(Lang::Toml) => Some(['#', ';']),
        _ => Some(['#', '#']),
    };
    if comment.is_some_and(|marks| trimmed.starts_with(marks)) {
        out.push(trimmed, Some(COMMENT));
        return;
    }
    if lang == Some(Lang::Toml) && trimmed.starts_with('[') {
        out.push(trimmed, Some(TYPE));
        return;
    }
    let mut rest = trimmed;
    if lang == Some(Lang::Yaml)
        && let Some(item) = rest.strip_prefix("- ")
    {
        out.push("- ", Some(PUNCTUATION));
        rest = item;
    }
    let key_end = match lang {
        Some(Lang::Toml) => rest.find('='),
        Some(Lang::Yaml) => rest
            .find(": ")
            .or_else(|| rest.ends_with(':').then(|| rest.len() - 1)),
        _ => None,
    };
    if let Some(end) = key_end
        && !rest.starts_with(['"', '\''])
    {
        let (key, after) = split(rest, end);
        out.push(key, Some(KEYWORD));
        let (sep, value) = split(after, 1);
        out.push(sep, Some(OPERATOR));
        rest = value;
    }
    while !rest.is_empty() {
        rest = data_token(rest, lang, out);
    }
}

fn data_token<'a>(rest: &'a str, lang: Option<Lang>, out: &mut Out) -> &'a str {
    let Some(c) = rest.chars().next() else {
        return rest;
    };
    let len = if c.is_whitespace() {
        let len = run(rest, char::is_whitespace);
        out.push(split(rest, len).0, None);
        len
    } else if c == '"' || c == '\'' {
        let (quote, body) = split(rest, 1);
        let end = close_quote(body, quote, c == '\'').map_or(rest.len(), |end| end + 1);
        let (string, after) = split(rest, end);
        let key = lang == Some(Lang::Json) && after.trim_start().starts_with(':');
        out.push(string, Some(if key { KEYWORD } else { STRING }));
        end
    } else if c == '#' && lang != Some(Lang::Json) {
        out.push(rest, Some(COMMENT));
        rest.len()
    } else if PUNCTUATIONS.contains(c) || c == ':' {
        out.push(split(rest, c.len_utf8()).0, Some(PUNCTUATION));
        c.len_utf8()
    } else {
        let len = run(rest, |ch| !ch.is_whitespace() && !"[]{},#".contains(ch)).max(1);
        let word = split(rest, len).0;
        let color = if matches!(word, "true" | "false" | "null" | "~" | "yes" | "no") {
            KEYWORD
        } else if word.starts_with(|ch: char| ch.is_ascii_digit() || ch == '-' || ch == '+') {
            NUMBER
        } else {
            STRING
        };
        out.push(word, Some(color));
        len
    };
    split(rest, len).1
}

fn diff(line: &str, out: &mut Out) {
    let color = if line.starts_with("+++") || line.starts_with("---") {
        theme::DIM
    } else if line.starts_with("@@") {
        theme::ACCENT
    } else if line.starts_with('+') {
        theme::SUCCESS
    } else if line.starts_with('-') {
        theme::ERROR
    } else {
        theme::MUTED
    };
    out.push(line, Some(color));
}
