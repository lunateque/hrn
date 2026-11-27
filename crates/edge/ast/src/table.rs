//! The embedded language-to-extension table (`languages.json`).
//!
//! Data only — no grammar is linked here. The `ast` module reads it to validate the `lang`
//! argument and to name the supported set in tool specs; the `hrn-ast` sidecar reads it to
//! infer languages from extensions and to look up each language's metavariable expando.
//! The table is built once on first use (`specs`), never at startup.

use serde::Deserialize;
use std::path::Path;
use std::sync::LazyLock;

/// The table, embedded at compile time.
const REGISTRY: &str = include_str!("../languages.json");

/// One entry of `languages.json`.
#[derive(Clone, Debug)]
pub struct Spec {
    /// The canonical language key (also the sidecar's grammar selector).
    pub name: String,
    /// Names the `lang` argument accepts.
    pub aliases: Vec<String>,
    /// File extensions that infer this language.
    pub extensions: Vec<String>,
    /// The character metavariables are rewritten to before parsing (`"$"` = none needed).
    pub expando: String,
}

impl Spec {
    /// The expando character; `'$'` means patterns are parsed verbatim.
    #[must_use]
    pub fn expando_char(&self) -> char {
        self.expando
            .chars()
            .next()
            .filter(|_| self.expando != "$")
            .unwrap_or('$')
    }
}

#[derive(Deserialize)]
struct RawSpec {
    name: String,
    aliases: Vec<String>,
    extensions: Vec<String>,
    expando: String,
}

/// Every table entry, in file order.
#[must_use]
pub fn specs() -> &'static [Spec] {
    static TABLE: LazyLock<Vec<Spec>> = LazyLock::new(|| {
        // Embedded, compile-time data: a malformed file is caught by the table test, and an
        // empty table only ever means "no language resolves" — never a wrong answer.
        let raw: Vec<RawSpec> = serde_json::from_str(REGISTRY).unwrap_or_default();
        raw.into_iter()
            .map(|r| Spec {
                name: r.name,
                aliases: r.aliases,
                extensions: r.extensions,
                expando: r.expando,
            })
            .collect()
    });
    TABLE.as_slice()
}

/// The language key an alias names (case-insensitive), `None` when the table has no such alias.
#[must_use]
pub fn key_of(alias: &str) -> Option<&'static str> {
    specs()
        .iter()
        .find(|s| s.aliases.iter().any(|a| a.eq_ignore_ascii_case(alias)))
        .map(|s| s.name.as_str())
}

/// The language key a file name's extension maps to.
#[must_use]
pub fn key_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?;
    specs()
        .iter()
        .find(|s| s.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
        .map(|s| s.name.as_str())
}

/// Every alias in the table, sorted and comma-separated, for tool specs and errors.
#[must_use]
pub fn aliases() -> String {
    let mut names: Vec<&str> = specs()
        .iter()
        .flat_map(|s| s.aliases.iter().map(String::as_str))
        .collect();
    names.sort_unstable();
    names.dedup();
    names.join(", ")
}
