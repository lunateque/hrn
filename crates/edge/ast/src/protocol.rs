//! The stdin/stdout JSON protocol between `hrn` and its `hrn-ast` sidecar, and the plain-data
//! result shapes both sides speak.
//!
//! The sidecar owns the tree-sitter grammars; this crate owns only the wire types so the tool
//! text can be formatted in-process without linking a single parser.

use serde::{Deserialize, Serialize};

/// One rewrite rule as given to `ast_edit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// The AST pattern to match.
    pub pat: String,
    /// The rewrite template; empty deletes the match.
    pub out: String,
}

/// A request to the sidecar, one JSON value per line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Structural search.
    Search {
        /// Paths in the response are displayed relative to here.
        cwd: String,
        /// The scope: a `;`-separated list of files, directories and globs with `!glob`
        /// exclusions, in the shared edge scope grammar.
        path: String,
        /// Language alias forcing every file; `None` infers per extension.
        lang: Option<String>,
        /// The AST pattern.
        pattern: String,
        /// Matches to skip when paging.
        skip: usize,
        /// Matches to return.
        limit: usize,
    },
    /// Structural rewrite; with `write` false it only computes the proposal.
    Rewrite {
        /// Paths in the response are displayed relative to here.
        cwd: String,
        /// The scope, in the shared edge scope grammar (see [`Request::Search`]).
        path: String,
        /// Language alias forcing every file; `None` infers per extension.
        lang: Option<String>,
        /// The rules, run in pattern order.
        rules: Vec<Rule>,
        /// Files one pass may touch.
        max_files: usize,
        /// Whether to write the files (apply), or only report (preview).
        write: bool,
    },
    /// Parse an in-memory buffer and report tree-sitter syntax errors (used by `[edit] auto_repair`).
    CheckSyntax {
        /// File path whose extension selects the grammar when `lang` is `None`.
        path: String,
        /// Optional explicit language alias override.
        lang: Option<String>,
        /// UTF-8 file text to parse.
        content: String,
    },
}

/// One structural match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    /// The file the match lives in, displayed relative to `cwd`.
    pub path: String,
    /// 1-based line of the match's first line.
    pub line: usize,
    /// The matched source text.
    pub text: String,
    /// Metavariable bindings, `NAME`/`text` (a multi binding reads `[a, b]`).
    pub meta: Vec<(String, String)>,
}

/// The result of one search pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchData {
    /// Matches sorted by path and position, already paged.
    pub matches: Vec<Found>,
    /// Total matches before paging.
    pub total: usize,
    /// Files parsed and searched.
    pub files_searched: usize,
    /// Whether paging cut the result short.
    pub limit_reached: bool,
    /// Non-fatal parse and pattern problems.
    pub parse_errors: Vec<String>,
}

/// One staged replacement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// The file it applies to, displayed relative to `cwd`.
    pub path: String,
    /// 1-based line of the replaced span.
    pub line: usize,
    /// The matched text.
    pub before: String,
    /// The rewritten text.
    pub after: String,
}

/// The result of one rewrite pass (dry or applied).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewriteData {
    /// Every staged change, ordered by file and position.
    pub changes: Vec<Change>,
    /// Per touched file: the displayed path and its change count.
    pub per_file: Vec<(String, usize)>,
    /// Total staged changes.
    pub total: usize,
    /// Files with at least one change.
    pub files_touched: usize,
    /// Files parsed.
    pub files_searched: usize,
    /// Whether the file cap stopped the pass.
    pub limit_reached: bool,
    /// Non-fatal parse and pattern problems.
    pub parse_errors: Vec<String>,
}

/// Syntax-check outcome for one in-memory buffer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxData {
    /// Whether a tree-sitter grammar was available for the file/language.
    pub supported: bool,
    /// Number of syntax error / missing nodes found in the CST.
    pub error_count: usize,
    /// Compact 1-based `line:col` error descriptions (capped).
    pub errors: Vec<String>,
}

/// The payload of a successful request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Data {
    /// Payload of [`Request::Search`].
    Search(SearchData),
    /// Payload of [`Request::Rewrite`].
    Rewrite(RewriteData),
    /// Payload of [`Request::CheckSyntax`].
    Syntax(SyntaxData),
}

/// The sidecar's one-line answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// A fatal message for the request (bad pattern scope, overlapping edits, write failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The payload, absent when `error` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Data>,
}

impl Response {
    /// A success response carrying `data`.
    #[must_use]
    pub fn ok(data: Data) -> Self {
        Self {
            error: None,
            data: Some(data),
        }
    }

    /// A failure response carrying one message.
    #[must_use]
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            data: None,
        }
    }

    /// Borrow the data or the error message.
    ///
    /// # Errors
    ///
    /// With the sidecar's message when the request failed or the answer was empty.
    pub fn into_data(self) -> Result<Data, String> {
        match (self.data, self.error) {
            (Some(data), _) => Ok(data),
            (None, Some(error)) => Err(error),
            (None, None) => Err("empty response from hrn-ast".to_owned()),
        }
    }
}
