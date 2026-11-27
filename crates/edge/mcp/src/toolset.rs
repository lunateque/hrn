//! Splits a session's MCP tools into those whose schemas ride in every request and those that
//! are deferred behind `tool_search`, by a schema-token budget.

use crate::protocol::tool_prefix;
use hrn_kernel::{DeferredTool, Tool};
use std::sync::Arc;

/// The tools of one server.
pub(crate) struct ServerTools {
    pub(crate) name: String,
    pub(crate) tools: Vec<Arc<dyn Tool>>,
}

/// A session's MCP tools, split by schema-token budget.
#[derive(Clone, Default)]
pub struct McpToolSet {
    /// Tools whose schemas are sent with every request.
    pub loaded: Vec<Arc<dyn Tool>>,
    /// Tools listed by name only until `tool_search` activates them.
    pub deferred: Vec<DeferredTool>,
}

/// Approximate request cost of a tool's schema (4 bytes per token, as the harness estimates).
fn schema_tokens(tool: &dyn Tool) -> usize {
    let spec = tool.spec();
    (spec.name.len() + spec.description.len() + spec.parameters.to_string().len()) / 4
}

impl McpToolSet {
    /// Load whole servers, smallest first, while their schemas fit in `budget_tokens`; defer the
    /// rest. A server is never split: its tools are either all loaded or all deferred.
    pub(crate) fn split(servers: &[ServerTools], budget_tokens: usize) -> Self {
        let mut sized: Vec<(usize, &ServerTools)> = servers
            .iter()
            .map(|s| (s.tools.iter().map(|t| schema_tokens(t.as_ref())).sum(), s))
            .collect();
        sized.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        let mut set = Self::default();
        let mut used = 0usize;
        for (tokens, server) in sized {
            if set.deferred.is_empty() && used.saturating_add(tokens) <= budget_tokens {
                used += tokens;
                set.loaded.extend(server.tools.iter().cloned());
                continue;
            }
            let prefix = tool_prefix(&server.name);
            set.deferred.extend(server.tools.iter().map(|tool| {
                let name = tool.spec().name;
                DeferredTool {
                    group: server.name.clone(),
                    short_name: name.strip_prefix(&prefix).unwrap_or(&name).to_owned(),
                    tool: Arc::clone(tool),
                }
            }));
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::{BoxFuture, Result, ToolContext, ToolOutput, ToolSpec};
    use serde_json::{Value, json};

    struct Stub(String, usize);

    impl Tool for Stub {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.clone(),
                description: "d".repeat(self.1),
                parameters: json!({"type": "object"}),
            }
        }

        fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
            Box::pin(async { Ok(ToolOutput::ok("")) })
        }
    }

    fn server(name: &str, tools: usize, description_bytes: usize) -> ServerTools {
        ServerTools {
            name: name.to_owned(),
            tools: (0..tools)
                .map(|i| -> Arc<dyn Tool> {
                    Arc::new(Stub(
                        format!("{}t{i}", tool_prefix(name)),
                        description_bytes,
                    ))
                })
                .collect(),
        }
    }

    #[test]
    fn small_servers_load_whole_and_the_big_one_defers_whole() {
        let servers = [
            server("big", 10, 400),
            server("small", 2, 40),
            server("mid", 3, 100),
        ];
        let set = McpToolSet::split(&servers, 200);
        assert_eq!(
            set.loaded.len(),
            5,
            "small and mid fit; servers are never split"
        );
        assert_eq!(set.deferred.len(), 10);
        assert!(set.deferred.iter().all(|d| d.group == "big"));
        assert_eq!(
            set.deferred.first().map(|d| d.short_name.as_str()),
            Some("t0")
        );
    }

    #[test]
    fn zero_budget_defers_everything_and_a_huge_one_defers_nothing() {
        let servers = [server("a", 2, 40), server("b", 2, 40)];
        assert_eq!(McpToolSet::split(&servers, 0).deferred.len(), 4);
        assert_eq!(McpToolSet::split(&servers, usize::MAX).loaded.len(), 4);
    }
}
