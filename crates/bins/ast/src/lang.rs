//! The grammars compiled into this sidecar and the `ast-grep` `Language` glue.
//!
//! Which grammars are compiled in is this crate's cargo features; the aliases, extensions and
//! expando characters come from the shared `languages.json` table in `hrn-edge-ast`. A table
//! entry whose grammar is not compiled in is skipped: the request reports the language as
//! unsupported through the module layer, which reads the same table.

use ast_grep_core::language::Language as SgLanguage;
use ast_grep_core::matcher::{Pattern, PatternBuilder, PatternError};
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc, TSLanguage};
use hrn_edge_ast::table;
use std::borrow::Cow;

/// One language this sidecar can parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Lang {
    /// Rust.
    #[cfg(feature = "rust")]
    Rust,
    /// Python.
    #[cfg(feature = "python")]
    Python,
    /// JavaScript (and JSX).
    #[cfg(feature = "javascript")]
    JavaScript,
    /// TypeScript.
    #[cfg(feature = "typescript")]
    TypeScript,
    /// TypeScript with JSX.
    #[cfg(feature = "tsx")]
    Tsx,
    /// Go.
    #[cfg(feature = "go")]
    Go,
    /// JSON.
    #[cfg(feature = "json")]
    Json,
    /// Bash and POSIX shell.
    #[cfg(feature = "bash")]
    Bash,
}

impl Lang {
    fn from_key(key: &str) -> Option<Lang> {
        match key {
            #[cfg(feature = "rust")]
            "rust" => Some(Self::Rust),
            #[cfg(feature = "python")]
            "python" => Some(Self::Python),
            #[cfg(feature = "javascript")]
            "javascript" => Some(Self::JavaScript),
            #[cfg(feature = "typescript")]
            "typescript" => Some(Self::TypeScript),
            #[cfg(feature = "tsx")]
            "tsx" => Some(Self::Tsx),
            #[cfg(feature = "go")]
            "go" => Some(Self::Go),
            #[cfg(feature = "json")]
            "json" => Some(Self::Json),
            #[cfg(feature = "bash")]
            "bash" => Some(Self::Bash),
            _ => None,
        }
    }

    /// Resolve the language for a file: explicit `lang` alias wins, else the extension maps it.
    /// The `Err` variant names an alias the table knows but this build did not compile.
    pub(crate) fn resolve(
        path: &std::path::Path,
        lang: Option<&str>,
    ) -> Result<Option<Lang>, String> {
        let key =
            match lang {
                Some(alias) => Some(table::key_of(alias).ok_or_else(|| {
                    format!("language `{alias}` is not in the ast language table")
                })?),
                None => table::key_for_path(path),
            };
        let Some(key) = key else {
            return Ok(None);
        };
        if lang.is_some() && Self::from_key(key).is_none() {
            return Err(format!(
                "language `{key}` is not compiled into this hrn-ast build"
            ));
        }
        Ok(Self::from_key(key))
    }

    /// The tree-sitter grammar: a table pointer, fetched per parse, never at startup.
    fn ts(self) -> TSLanguage {
        match self {
            #[cfg(feature = "rust")]
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            #[cfg(feature = "python")]
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            #[cfg(feature = "javascript")]
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            #[cfg(feature = "typescript")]
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            #[cfg(feature = "tsx")]
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            #[cfg(feature = "go")]
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            #[cfg(feature = "json")]
            Self::Json => tree_sitter_json::LANGUAGE.into(),
            #[cfg(feature = "bash")]
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
        }
    }

    /// The expando character of this language from the table. Only reachable for a language
    /// that resolved through the table, so the entry exists; `'$'` keeps the fallback honest.
    fn expando(self) -> char {
        let key = self.key();
        match table::specs().iter().find(|s| s.name == key) {
            Some(spec) => spec.expando_char(),
            None => '$',
        }
    }

    fn key(self) -> &'static str {
        match self {
            #[cfg(feature = "rust")]
            Self::Rust => "rust",
            #[cfg(feature = "python")]
            Self::Python => "python",
            #[cfg(feature = "javascript")]
            Self::JavaScript => "javascript",
            #[cfg(feature = "typescript")]
            Self::TypeScript => "typescript",
            #[cfg(feature = "tsx")]
            Self::Tsx => "tsx",
            #[cfg(feature = "go")]
            Self::Go => "go",
            #[cfg(feature = "json")]
            Self::Json => "json",
            #[cfg(feature = "bash")]
            Self::Bash => "bash",
        }
    }
}

impl SgLanguage for Lang {
    fn kind_to_id(&self, kind: &str) -> u16 {
        self.ts().id_for_node_kind(kind, true)
    }

    fn field_to_id(&self, field: &str) -> Option<u16> {
        self.ts()
            .field_id_for_name(field)
            .map(std::num::NonZero::get)
    }

    fn expando_char(&self) -> char {
        self.expando()
    }

    /// Rewrite the `$` sigils of a pattern into this language's expando character, so `µ` can
    /// stand for a metavariable in grammars whose lexer rejects `$` in identifiers (rust/python/go).
    fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
        let expando = self.expando_char();
        if expando == '$' {
            return Cow::Borrowed(query);
        }
        let mut out = String::with_capacity(query.len());
        let mut sigils = 0usize;
        let flush = |n: usize, ch: char, out: &mut String| {
            for _ in 0..n {
                out.push(ch);
            }
        };
        for c in query.chars() {
            if c == '$' {
                sigils += 1;
                continue;
            }
            let metavariable = matches!(c, 'A'..='Z' | '_') || sigils == 3;
            let ch = if metavariable { expando } else { '$' };
            flush(std::mem::take(&mut sigils), ch, &mut out);
            out.push(c);
        }
        let ch = if sigils == 3 { expando } else { '$' };
        flush(sigils, ch, &mut out);
        Cow::Owned(out)
    }

    fn build_pattern(
        &self,
        builder: &PatternBuilder,
    ) -> std::result::Result<Pattern, PatternError> {
        builder.build(|src| StrDoc::try_new(src, *self))
    }
}

impl LanguageExt for Lang {
    fn get_ts_language(&self) -> TSLanguage {
        self.ts()
    }
}
