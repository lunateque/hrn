//! Internal-URL port: a registry of `scheme://rest` resources (`local://`, `skill://`,
//! `history://`, …) that `read`, `write` and later tools resolve through one router.
//!
//! Zero I/O lives here; edge crates implement [`UrlScheme`] and the binary registers them.

use crate::error::{HrnError, Result};
use crate::traits::ToolContext;
use std::path::PathBuf;
use std::sync::Arc;

/// What a scheme resolved a URL to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UrlBody {
    /// Generated text (listings, transcripts); selectors page it in memory.
    Text(String),
    /// A resource backed by a local file or directory; read through the filesystem pipeline.
    Path(PathBuf),
}

/// One URL scheme implementation.
pub trait UrlScheme: Send + Sync {
    /// The scheme name without `://`, lower-case.
    fn scheme(&self) -> &'static str;

    /// One line for the system prompt: what `scheme://…` addresses and its sub-paths.
    fn about(&self) -> &'static str;

    /// Resolve `rest` (everything after `scheme://`, selectors already removed).
    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody>;

    /// Replace the content addressed by `rest`; read-only schemes keep the default refusal.
    fn write(&self, _ctx: &ToolContext, _rest: &str, _content: &str) -> Result {
        Err(HrnError::Tool(format!(
            "`{}://` is read-only",
            self.scheme()
        )))
    }

    /// Store `content` under a freshly allocated name and return its URL. Only the scratch
    /// scheme (`local`) implements this; it receives oversized tool outputs.
    fn spill(&self, _ctx: &ToolContext, _content: &str) -> Result<String> {
        Err(HrnError::Tool(format!(
            "`{}://` cannot hold spilled output",
            self.scheme()
        )))
    }
}

/// Registry of [`UrlScheme`]s, cheap to share behind an `Arc`.
#[derive(Default)]
pub struct UrlRouter {
    schemes: Vec<Arc<dyn UrlScheme>>,
    builtins: Vec<&'static str>,
}

/// Name of the scheme that receives spilled tool output.
const SPILL_SCHEME: &str = "local";

impl UrlRouter {
    /// An empty router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `scheme`, replacing an earlier registration of the same name.
    #[must_use]
    pub fn with(mut self, scheme: Arc<dyn UrlScheme>) -> Self {
        self.schemes.retain(|s| s.scheme() != scheme.scheme());
        self.schemes.push(scheme);
        self
    }

    /// Register a reserved built-in scheme that module schemes may not shadow.
    #[must_use]
    pub fn with_builtin(mut self, scheme: Arc<dyn UrlScheme>) -> Self {
        let name = scheme.scheme();
        if !self.builtins.contains(&name) {
            self.builtins.push(name);
        }
        self.with(scheme)
    }

    /// Register a module-supplied scheme, rejecting attempts to shadow a built-in scheme.
    ///
    /// # Errors
    ///
    /// Returns [`HrnError::Config`] when `scheme` has the same name as a built-in scheme.
    pub fn try_with_module(self, scheme: Arc<dyn UrlScheme>) -> Result<Self> {
        let name = scheme.scheme();
        if self.builtins.iter().any(|b| b.eq_ignore_ascii_case(name)) {
            return Err(HrnError::Config(format!(
                "module URL scheme `{name}://` cannot shadow built-in scheme"
            )));
        }
        Ok(self.with(scheme))
    }

    /// Split `scheme://rest`. Schemes are at least two characters, so Windows drive paths
    /// (`C://x`) are not mistaken for URLs.
    pub fn split(raw: &str) -> Option<(&str, &str)> {
        let (scheme, rest) = raw.split_once("://")?;
        let mut chars = scheme.chars();
        let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && scheme.len() >= 2
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        valid.then_some((scheme, rest))
    }

    fn get(&self, scheme: &str) -> Result<&Arc<dyn UrlScheme>> {
        self.schemes
            .iter()
            .find(|s| s.scheme().eq_ignore_ascii_case(scheme))
            .ok_or_else(|| {
                let known: Vec<_> = self.schemes.iter().map(|s| s.scheme()).collect();
                HrnError::Tool(format!(
                    "unknown URL scheme `{scheme}://` (known: {}); web pages go through `web_fetch`",
                    if known.is_empty() {
                        "none".to_owned()
                    } else {
                        known.join(", ")
                    }
                ))
            })
    }

    /// Resolve `scheme://rest` for reading.
    pub fn read(&self, ctx: &ToolContext, scheme: &str, rest: &str) -> Result<UrlBody> {
        self.get(scheme)?.read(ctx, rest)
    }

    /// Write `content` to `scheme://rest`.
    pub fn write(&self, ctx: &ToolContext, scheme: &str, rest: &str, content: &str) -> Result {
        self.get(scheme)?.write(ctx, rest, content)
    }

    /// Store `content` in the scratch scheme; `None` when no scratch scheme is registered.
    pub fn spill(&self, ctx: &ToolContext, content: &str) -> Result<Option<String>> {
        match self.get(SPILL_SCHEME) {
            Ok(scheme) => scheme.spill(ctx, content).map(Some),
            Err(_) => Ok(None),
        }
    }

    /// The system-prompt section naming every registered scheme (`None` when empty).
    pub fn reference(&self) -> Option<String> {
        if self.schemes.is_empty() {
            return None;
        }
        let mut out = String::from(
            "# Internal URLs\n\
             `read` (and `write` where noted) accept these `scheme://` paths; line selectors \
             such as `:50-80` or `:raw` apply to them too:",
        );
        for scheme in &self.schemes {
            out.push_str("\n- `");
            out.push_str(scheme.scheme());
            out.push_str("://`: ");
            out.push_str(scheme.about());
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{UrlBody, UrlRouter, UrlScheme};
    use crate::{Result, ToolContext};
    use std::sync::Arc;

    struct DummyScheme(&'static str);

    impl UrlScheme for DummyScheme {
        fn scheme(&self) -> &'static str {
            self.0
        }

        fn about(&self) -> &'static str {
            "dummy"
        }

        fn read(&self, _ctx: &ToolContext, _rest: &str) -> Result<UrlBody> {
            Ok(UrlBody::Text(self.0.to_owned()))
        }
    }

    #[test]
    fn splits_schemes_but_not_drive_letters_or_plain_paths() {
        assert_eq!(UrlRouter::split("local://1.log"), Some(("local", "1.log")));
        assert_eq!(UrlRouter::split("skill://a/b:3"), Some(("skill", "a/b:3")));
        assert_eq!(UrlRouter::split("C://x"), None);
        assert_eq!(UrlRouter::split("src/main.rs"), None);
        assert_eq!(UrlRouter::split("a b://x"), None);
    }

    #[test]
    fn url_router_rejects_builtin_shadowing() {
        let router = UrlRouter::new().with_builtin(Arc::new(DummyScheme("local")));
        assert!(
            router
                .try_with_module(Arc::new(DummyScheme("local")))
                .is_err()
        );
        let router = UrlRouter::new().with_builtin(Arc::new(DummyScheme("local")));
        assert!(
            router
                .try_with_module(Arc::new(DummyScheme("LOCAL")))
                .is_err()
        );
        let router = UrlRouter::new().with_builtin(Arc::new(DummyScheme("local")));
        assert!(
            router
                .try_with_module(Arc::new(DummyScheme("custom")))
                .is_ok()
        );
    }
}
