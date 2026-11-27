//! The rule-injection hook: a glob rule's body reaches the model in the result of the first tool
//! call that touches a matching file.

use super::book::Rulebook;
use super::book::touched_paths;
use hrn_kernel::{BoxFuture, HarnessHook, Result, ToolContext, ToolOutput};
use serde_json::Value;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

/// Appends the body of every glob rule matching a file the call touched to that call's result —
/// once per rule per session (the seen set is `(session_id, rule)` pairs, shared runtime-wide).
pub struct RulesHook {
    book: Rulebook,
    seen: Arc<Mutex<std::collections::HashSet<(String, String)>>>,
}

impl RulesHook {
    /// A hook over `book` sharing the runtime-wide `seen` set (so a rule reaches a session once).
    #[must_use]
    pub fn new(
        book: Rulebook,
        seen: Arc<Mutex<std::collections::HashSet<(String, String)>>>,
    ) -> Self {
        Self { book, seen }
    }

    /// Whether `rule` is being injected into `session_id` for the first time.
    fn first_touch(&self, session_id: &str, rule: &str) -> bool {
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        seen.insert((session_id.to_owned(), rule.to_ascii_lowercase()))
    }
}

impl HarnessHook for RulesHook {
    fn after_tool(
        &self,
        ctx: &ToolContext,
        tool_name: &str,
        arguments: &Value,
        mut output: ToolOutput,
    ) -> BoxFuture<Result<ToolOutput>> {
        let mut blocks = String::new();
        if !output.is_error {
            for rel in touched_paths(&ctx.cwd, tool_name, arguments) {
                for rule in self.book.matching(&rel) {
                    if !self.first_touch(&ctx.session_id, &rule.name) {
                        continue;
                    }
                    let _ = write!(
                        blocks,
                        "\n\n<rule name=\"{name}\" path=\"{path}\">\n{body}\n</rule>",
                        name = rule.name,
                        path = rule.path.display(),
                        body = rule.body,
                    );
                }
            }
        }
        output.content.push_str(&blocks);
        Box::pin(async move { Ok(output) })
    }
}
