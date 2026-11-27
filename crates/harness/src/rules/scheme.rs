//! `rule://<name>` — read one discovered rule's body; bare `rule://` lists the names.

use hrn_kernel::{HrnError, Result, ToolContext, UrlBody, UrlScheme};
use std::path::Path;
use std::sync::Arc;

/// Lists the rule bodies of a working directory (`(name, body)` pairs; discovery is wired by the
/// binary so the harness crate does not have to own the URL router).
pub type RuleResolver = Arc<dyn Fn(&Path) -> Result<Vec<(String, String)>> + Send + Sync>;

/// `rule://<name>`: one rule's body; bare `rule://` lists names.
pub struct RuleScheme {
    resolver: RuleResolver,
}

impl RuleScheme {
    /// Resolve rules through `resolver`.
    #[must_use]
    pub fn new(resolver: RuleResolver) -> Self {
        Self { resolver }
    }
}

impl UrlScheme for RuleScheme {
    fn scheme(&self) -> &'static str {
        "rule"
    }

    fn about(&self) -> &'static str {
        "workspace rules: `rule://<name>` is one rule's body, bare `rule://` lists names"
    }

    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        let rules = (self.resolver)(&ctx.cwd)?;
        let name = rest.trim_matches('/');
        if name.is_empty() {
            return Ok(UrlBody::Text(if rules.is_empty() {
                "(no rules discovered)".to_owned()
            } else {
                rules
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            }));
        }
        rules
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, body)| UrlBody::Text(body.clone()))
            .ok_or_else(|| {
                let known: Vec<&str> = rules.iter().map(|(n, _)| n.as_str()).collect();
                HrnError::Tool(format!(
                    "unknown rule `{name}` (known: {})",
                    if known.is_empty() {
                        "none".to_owned()
                    } else {
                        known.join(", ")
                    }
                ))
            })
    }
}
