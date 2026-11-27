//! `help://<page>`: embedded reference pages cut out of tool descriptions.
//!
//! The pages are static text (embedded `.md` files), so the kernel owns the scheme: modules
//! register theirs at startup through `Registry::help_page`, and the finished registry hands
//! them to the binary, which installs [`HelpScheme`] like any other scheme.

use crate::error::{HrnError, Result};
use crate::traits::ToolContext;
use crate::urls::{UrlBody, UrlScheme};
use std::collections::BTreeMap;

/// The static reference pages served under `help://`, keyed by page name.
pub type HelpPages = BTreeMap<String, String>;

/// `help://<page>`: one reference page; bare `help://` lists the page names.
pub struct HelpScheme {
    pages: HelpPages,
}

impl HelpScheme {
    /// Serve `pages`.
    #[must_use]
    pub const fn new(pages: HelpPages) -> Self {
        Self { pages }
    }
}

impl UrlScheme for HelpScheme {
    fn scheme(&self) -> &'static str {
        "help"
    }

    fn about(&self) -> &'static str {
        "embedded reference pages a tool description points to; `help://<page>` reads one, \
         bare `help://` lists them"
    }

    fn read(&self, _ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        let page = rest.trim_matches('/');
        if page.is_empty() {
            let mut names: Vec<&str> = self.pages.keys().map(String::as_str).collect();
            names.sort_unstable();
            return Ok(UrlBody::Text(if names.is_empty() {
                "(no help pages)".to_owned()
            } else {
                names
                    .iter()
                    .map(|n| format!("help://{n}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }));
        }
        self.pages
            .get(page)
            .map(|body| UrlBody::Text(body.clone()))
            .ok_or_else(|| {
                let known: Vec<&str> = self.pages.keys().map(String::as_str).collect();
                HrnError::Tool(format!(
                    "unknown help page `{page}` (known: {})",
                    if known.is_empty() {
                        "none".to_owned()
                    } else {
                        known.join(", ")
                    }
                ))
            })
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::{HelpPages, HelpScheme};
    use crate::{ToolContext, UrlScheme};

    fn pages() -> HelpPages {
        let mut pages = HelpPages::new();
        pages.insert("read/sources".to_owned(), "sqlite selectors".to_owned());
        pages.insert("debug".to_owned(), "dap actions".to_owned());
        pages
    }

    fn ctx() -> ToolContext {
        crate::testing::tool_context(std::env::temp_dir())
    }

    #[test]
    fn a_named_page_reads_and_an_unknown_one_names_the_known_ones() {
        let scheme = HelpScheme::new(pages());
        let body = scheme.read(&ctx(), "read/sources").expect("page");
        assert_eq!(body, crate::UrlBody::Text("sqlite selectors".to_owned()));
        let err = scheme.read(&ctx(), "nope").expect_err("unknown");
        assert!(
            err.to_string().contains("debug"),
            "the error lists known pages: {err}"
        );
    }

    #[test]
    fn a_bare_scheme_lists_the_page_urls() {
        let scheme = HelpScheme::new(pages());
        let body = scheme.read(&ctx(), "").expect("listing");
        assert_eq!(
            body,
            crate::UrlBody::Text("help://debug\nhelp://read/sources".to_owned())
        );
    }

    #[test]
    fn pages_are_read_only() {
        let scheme = HelpScheme::new(pages());
        assert!(scheme.write(&ctx(), "debug", "x").is_err());
    }
}
