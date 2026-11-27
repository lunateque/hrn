//! The scan, match and rewrite engine of the sidecar: candidate discovery, per-language pattern
//! compilation, match collection and staged rewrites over plain-data results.
//!
//! Every file is parsed in the language its extension maps to (or the request's override), so a
//! mixed-language scope works: a rule only rewrites the files it parses in. Parse problems are
//! collected, not fatal. Cancellation is the parent's job: it kills this process.

use crate::lang::Lang;
use ast_grep_core::meta_var::MetaVariable;
use ast_grep_core::tree_sitter::StrDoc;
use ast_grep_core::{AstGrep, NodeMatch, Pattern};
use hrn_edge_ast::protocol::{Change, Found, RewriteData, Rule, SearchData, SyntaxData};
use hrn_edge_tools::paths::display_rel;
use hrn_edge_tools::{scope_files, write_atomic};
use hrn_kernel::CancelFlag;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The outcome of one request: a result or one fatal message.
pub(crate) type Outcome<T> = std::result::Result<T, String>;

/// A candidate file and the language it is parsed in.
struct Candidate {
    path: PathBuf,
    shown: String,
    lang: Lang,
}

/// Collect the files in the shared edge scope (`;` list with `!glob` exclusions, gitignore-aware)
/// that have a usable grammar; `lang` forces every file. The parent process owns cancellation —
/// it kills this sidecar — so the walk runs against a flag nobody cancels.
///
/// # Errors
///
/// When the scope is empty or missing, a `lang` names a grammar this build did not compile, or
/// nothing in scope has a usable language.
fn candidates(cwd: &Path, path: &str, lang: Option<&str>) -> Outcome<Vec<Candidate>> {
    let files = scope_files(cwd, Some(path), &CancelFlag::new()).map_err(|e| e.to_string())?;
    let mut out: Vec<Candidate> = Vec::new();
    for file in files {
        let Some(resolved) = Lang::resolve(&file, lang)? else {
            continue;
        };
        out.push(Candidate {
            shown: display_rel(cwd, &file),
            path: file,
            lang: resolved,
        });
    }
    out.sort_by(|a, b| a.shown.cmp(&b.shown));
    if out.is_empty() {
        return Err(match lang {
            Some(l) => format!("found no files readable as `{l}` in the given path/glob"),
            None => "found no supported source files for the given path/glob".to_owned(),
        });
    }
    Ok(out)
}

/// A parsed file; match nodes borrow `grep`, whose source text is kept alongside.
struct Parsed {
    grep: AstGrep<StrDoc<Lang>>,
    source: String,
}

fn parse(candidate: &Candidate) -> std::result::Result<Parsed, String> {
    let bytes = fs::read(&candidate.path).map_err(|e| format!("cannot read: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| "file is not valid UTF-8".to_owned())?;
    let doc = StrDoc::try_new(&text, candidate.lang).map_err(|e| format!("cannot parse: {e}"))?;
    Ok(Parsed {
        grep: AstGrep::doc(doc),
        source: text,
    })
}

/// The parse problem of one file, or the empty string when its tree is clean.
fn issue(candidate: &Candidate, parsed: Option<&Parsed>) -> String {
    parsed.map_or_else(
        || format!("{}: parse error", candidate.shown),
        |p| {
            if p.grep.root().get_inner_node().has_error() {
                format!(
                    "{}: parse error (syntax tree contains error nodes)",
                    candidate.shown
                )
            } else {
                String::new()
            }
        },
    )
}

/// Compile `pattern` once per language present in `pool`, recording failures as parse issues.
fn compile_for(
    pattern: &str,
    pool: &[Candidate],
    errors: &mut Vec<String>,
) -> HashMap<Lang, Pattern> {
    let mut map = HashMap::new();
    for candidate in pool {
        if map.contains_key(&candidate.lang) {
            continue;
        }
        match compile_with_fallback(pattern, candidate.lang) {
            Ok(p) => {
                map.insert(candidate.lang, p);
            }
            Err(e) => errors.push(format!("{}: invalid pattern: {e}", candidate.shown)),
        }
    }
    map
}

/// Build the pattern, retrying a JSON member fragment (`"key": $V`) inside an object wrapper.
fn compile_with_fallback(pattern: &str, lang: Lang) -> Outcome<Pattern> {
    match Pattern::try_new(pattern, lang) {
        Ok(p) => Ok(p),
        Err(first) => {
            if lang != Lang::Json {
                return Err(format!("{first}"));
            }
            let context = format!("{{ {} }}", quote_json_metavars(pattern));
            Pattern::contextual(&context, "pair", lang).map_err(|_| format!("{first}"))
        }
    }
}

/// Quote bare `$NAME` / `$$$NAME` so a JSON wrapper parses; quoted ones are left alone.
fn quote_json_metavars(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let mut dollars = 1;
        while chars.peek() == Some(&'$') {
            let _ = chars.next();
            dollars += 1;
        }
        let mut name = String::new();
        while chars
            .peek()
            .is_some_and(|c| c.is_ascii_uppercase() || *c == '_' || c.is_ascii_digit())
        {
            if let Some(c) = chars.next() {
                name.push(c);
            }
        }
        if name.is_empty() {
            out.push_str(&"$".repeat(dollars));
            continue;
        }
        out.push('"');
        out.push_str(&"$".repeat(dollars));
        out.push_str(&name);
        out.push('"');
    }
    out
}

/// Metavariable bindings of one match, sorted by name; a multi-node binding reads `[a, b]`.
fn bindings(m: &NodeMatch<'_, StrDoc<Lang>>) -> Vec<(String, String)> {
    let env = m.get_env();
    let mut out = Vec::new();
    for var in env.get_matched_variables() {
        let (name, text) = match var {
            MetaVariable::Capture(name, _) => {
                let text = env.get_match(&name).map(|n| n.text().into());
                (name, text)
            }
            MetaVariable::MultiCapture(name) => {
                let parts: Vec<String> = env
                    .get_multiple_matches(&name)
                    .iter()
                    .map(|n| n.text().to_string())
                    .collect();
                (name, Some(format!("[{}]", parts.join(", "))))
            }
            _ => continue,
        };
        if let Some(text) = text {
            out.push((name, text));
        }
    }
    out.sort();
    out
}

/// Search every file in the scope for `pattern`; `skip`/`limit` page the sorted result.
///
/// # Errors
///
/// When the scope has no parsable file or the language is not compiled in.
pub(crate) fn search(
    cwd: &Path,
    path: &str,
    lang: Option<&str>,
    pattern: &str,
    skip: usize,
    limit: usize,
) -> Outcome<SearchData> {
    let pool = candidates(cwd, path, lang)?;
    let mut errors = Vec::new();
    let compiled = compile_for(pattern, &pool, &mut errors);
    let mut hits: Vec<Found> = Vec::new();
    let mut searched = 0usize;
    for candidate in &pool {
        let Some(pat) = compiled.get(&candidate.lang) else {
            continue;
        };
        let parsed = parse(candidate);
        let Ok(parsed) = &parsed else {
            errors.push(issue(candidate, None));
            continue;
        };
        searched += 1;
        let warning = issue(candidate, Some(parsed));
        if !warning.is_empty() {
            errors.push(warning);
        }
        for m in parsed.grep.root().find_all(pat) {
            hits.push(Found {
                path: candidate.shown.clone(),
                line: m.start_pos().line() + 1,
                text: m.text().to_string(),
                meta: bindings(&m),
            });
        }
    }
    hits.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then_with(|| a.text.cmp(&b.text))
    });
    let total = hits.len();
    let limit_reached = total > skip.saturating_add(limit);
    let matches = hits.into_iter().skip(skip).take(limit).collect();
    Ok(SearchData {
        matches,
        total,
        files_searched: searched,
        limit_reached,
        parse_errors: errors,
    })
}

/// Compute (and with `write` also apply) the rewrites of `rules` over the scope.
///
/// Rules run in pattern-string order; a file whose tree has syntax errors is skipped, identical
/// duplicate edits count once, overlapping ones abort, and a file whose content is unchanged is
/// not written.
///
/// # Errors
///
/// When the scope has no parsable file, edits overlap, or a file cannot be written.
pub(crate) fn rewrite(
    cwd: &Path,
    path: &str,
    lang: Option<&str>,
    rules: &[Rule],
    max_files: usize,
    write: bool,
) -> Outcome<RewriteData> {
    let mut rules: Vec<Rule> = rules.to_vec();
    rules.sort_by(|a, b| a.pat.cmp(&b.pat));
    let pool = candidates(cwd, path, lang)?;
    let mut errors = Vec::new();
    let compiled = compile_rules(&rules, &pool, &mut errors);
    let mut changes = Vec::new();
    let mut per_file: Vec<(String, usize)> = Vec::new();
    let mut searched = 0usize;
    let mut touched = 0usize;
    let mut limit_reached = false;
    for candidate in &pool {
        let Some(rules_for_lang) = compiled.get(&candidate.lang) else {
            continue;
        };
        if rules_for_lang.is_empty() {
            continue;
        }
        let parsed = parse(candidate);
        let Ok(parsed) = &parsed else {
            errors.push(issue(candidate, None));
            continue;
        };
        searched += 1;
        let warning = issue(candidate, Some(parsed));
        if !warning.is_empty() {
            errors.push(warning);
            continue;
        }
        let Some(mut staged) = plan(candidate, parsed, rules_for_lang)? else {
            continue;
        };
        if touched >= max_files {
            limit_reached = true;
            break;
        }
        let new_text = apply_edits(&parsed.source, &mut staged.edits, &candidate.shown)?;
        touched += 1;
        let count = staged.changes.len();
        changes.extend(staged.changes);
        per_file.push((candidate.shown.clone(), count));
        if write && new_text != parsed.source {
            write_atomic(&candidate.path, new_text.as_bytes())
                .map_err(|e| format!("cannot write `{}`: {e}", candidate.shown))?;
        }
    }
    changes.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then_with(|| a.before.cmp(&b.before))
    });
    let total = changes.len();
    Ok(RewriteData {
        changes,
        per_file,
        total,
        files_touched: touched,
        files_searched: searched,
        limit_reached,
        parse_errors: errors,
    })
}

/// Each rule set compiled once per language in the pool; failures become parse issues.
fn compile_rules(
    rules: &[Rule],
    pool: &[Candidate],
    errors: &mut Vec<String>,
) -> HashMap<Lang, Vec<(Pattern, String)>> {
    let mut compiled: HashMap<Lang, Vec<(Pattern, String)>> = HashMap::new();
    for candidate in pool {
        if compiled.contains_key(&candidate.lang) {
            continue;
        }
        let mut per_lang = Vec::new();
        for rule in rules {
            match compile_with_fallback(&rule.pat, candidate.lang) {
                Ok(p) => per_lang.push((p, rule.out.clone())),
                Err(e) => errors.push(format!("{}: invalid pattern: {e}", candidate.shown)),
            }
        }
        compiled.insert(candidate.lang, per_lang);
    }
    compiled
}

/// One pending byte-range replacement.
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// What one file's rewrite pass staged: its edits and the changes they report.
struct Staged {
    edits: Vec<Edit>,
    changes: Vec<Change>,
}

/// Every match of every rule in one parsed file, as pending edits.
fn plan(
    candidate: &Candidate,
    parsed: &Parsed,
    pats: &[(Pattern, String)],
) -> Outcome<Option<Staged>> {
    let source = parsed.source.as_str();
    let mut edits = Vec::new();
    let mut changes = Vec::new();
    for (pat, out) in pats {
        for m in parsed.grep.root().find_all(pat) {
            let edit = m.replace_by(out.as_str());
            let start = edit.position;
            let Some(end) = start.checked_add(edit.deleted_length) else {
                return Err(format!("`{}`: replacement range overflow", candidate.shown));
            };
            let text = String::from_utf8(edit.inserted_text)
                .map_err(|_| "replacement is not valid UTF-8".to_owned())?;
            let Some(before) = source.get(start..end) else {
                return Err(format!(
                    "`{}`: edit range is out of bounds",
                    candidate.shown
                ));
            };
            changes.push(Change {
                path: candidate.shown.clone(),
                line: m.start_pos().line() + 1,
                before: before.to_owned(),
                after: text.clone(),
            });
            edits.push(Edit { start, end, text });
        }
    }
    Ok((!edits.is_empty()).then_some(Staged { edits, changes }))
}

/// Deduplicate, overlap-check and apply `edits` left-to-right; the rewritten text.
fn apply_edits(source: &str, edits: &mut Vec<Edit>, shown: &str) -> Outcome<String> {
    edits.sort_by_key(|e| (e.start, e.end));
    let mut unique: Vec<Edit> = Vec::with_capacity(edits.len());
    for e in edits.drain(..) {
        if unique
            .last()
            .is_some_and(|p| p.start == e.start && p.end == e.end && p.text == e.text)
        {
            continue;
        }
        if unique.last().is_some_and(|p| p.end > e.start) {
            return Err(format!(
                "overlapping replacements detected in `{shown}`; refine the pattern to avoid ambiguous edits"
            ));
        }
        unique.push(e);
    }
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for e in &unique {
        if source.get(cursor..e.end).is_none() {
            return Err(format!("`{shown}`: edit range is out of bounds"));
        }
        out.push_str(source.get(cursor..e.start).unwrap_or_default());
        out.push_str(&e.text);
        cursor = e.end;
    }
    out.push_str(source.get(cursor..).unwrap_or_default());
    Ok(out)
}

/// Parse an in-memory buffer and report whether its tree-sitter CST contains syntax errors.
pub(crate) fn check_syntax(path: &str, lang: Option<&str>, content: &str) -> Outcome<SyntaxData> {
    let Some(resolved) = Lang::resolve(Path::new(path), lang)? else {
        return Ok(SyntaxData {
            supported: false,
            error_count: 0,
            errors: Vec::new(),
        });
    };
    let doc = StrDoc::try_new(content, resolved).map_err(|e| format!("cannot parse: {e}"))?;
    let grep = AstGrep::doc(doc);
    let root = grep.root();
    if !root.get_inner_node().has_error() {
        return Ok(SyntaxData {
            supported: true,
            error_count: 0,
            errors: Vec::new(),
        });
    }
    let mut error_count = 0usize;
    let mut errors = Vec::new();
    for node in root.dfs() {
        let inner = node.get_inner_node();
        if inner.is_error() || inner.is_missing() {
            error_count += 1;
            if errors.len() < 10 {
                let pos = node.start_pos();
                let kind = if inner.is_missing() {
                    format!("missing `{}`", node.kind())
                } else {
                    "syntax error".to_owned()
                };
                errors.push(format!(
                    "line {}:{}: {kind}",
                    pos.line() + 1,
                    pos.column(&node) + 1
                ));
            }
        }
    }
    if error_count == 0 {
        error_count = 1;
        errors.push("syntax tree contains error nodes".to_owned());
    }
    Ok(SyntaxData {
        supported: true,
        error_count,
        errors,
    })
}
