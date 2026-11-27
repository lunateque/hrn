//! `hrn-edge-ast`: the wire protocol and result rendering for the `ast_grep` / `ast_edit`
//! tools. The tree-sitter grammars are NOT linked here — they live in the `hrn-ast` sidecar
//! binary so `hrn` itself stays small and starts without touching a parser.
//!
//! This crate holds the plain-data request/response types ([`protocol`]) spoken over the
//! sidecar's stdin/stdout, and the omp-shaped renderers ([`mod@format`], [`render`]) that turn a
//! response into tool text in-process.

pub mod format;
pub mod protocol;
pub mod render;
pub mod table;

#[cfg(test)]
mod tests;

pub use format::{Shown, Tag, files_of, show_applied, show_rewrite, show_search};
pub use protocol::{
    Change, Data, Found, Request, Response, RewriteData, Rule, SearchData, SyntaxData,
};
