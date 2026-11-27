//! Registry of callable tools keyed by name, including deferred tools whose schemas are only
//! sent once `tool_search` has activated them.

use crate::deferred::{Catalog, Entry, SEARCH_TOOL, ToolSearchTool, prompt_section};
use hrn_kernel::{DeferredTool, Tool, ToolEffect, ToolSpec};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

/// Registry of callable tools keyed by name.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    /// Every callable tool, deferred ones included (execution looks them up here).
    tools: BTreeMap<String, Arc<dyn Tool>>,
    /// The deferred subset; shared with the installed `tool_search`.
    deferred: Arc<Catalog>,
    /// Hidden tools (`goal`, `think`, `yield`): excluded from specs until activated per step.
    hidden: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a tool by its `spec().name`; hidden tools go to the hidden view.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let spec = tool.spec();
        if tool.hidden() {
            self.hidden.insert(spec.name, tool);
        } else {
            self.tools.insert(spec.name, tool);
        }
    }

    /// Register a slice/vec of tools.
    pub fn extend_tools(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) {
        for t in tools {
            self.register(t);
        }
    }

    /// Register tools whose schemas stay out of requests until `tool_search` activates them,
    /// and install `tool_search` itself.
    pub fn defer(&mut self, tools: impl IntoIterator<Item = DeferredTool>) {
        // Dropping the installed search tool leaves `deferred` unshared, so `make_mut` is free.
        self.tools.remove(SEARCH_TOOL);
        let catalog = Arc::make_mut(&mut self.deferred);
        for deferred in tools {
            let spec = deferred.tool.spec();
            catalog.insert(spec.name.clone(), Arc::new(Entry::new(&deferred, &spec)));
            self.tools.insert(spec.name, deferred.tool);
        }
        self.install_search();
    }

    /// (Re)install `tool_search` over the current deferred set; none when nothing is deferred.
    fn install_search(&mut self) {
        if self.deferred.is_empty() {
            self.tools.remove(SEARCH_TOOL);
        } else {
            let search = ToolSearchTool::new(Arc::clone(&self.deferred));
            self.tools.insert(SEARCH_TOOL.to_owned(), Arc::new(search));
        }
    }

    /// Look up a tool by name (hidden tools included; they stay callable).
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .get(name)
            .or_else(|| self.hidden.get(name))
            .cloned()
    }

    /// Names of every registered tool, deferred ones included. `tool_search` is left out: the
    /// registry keeps it installed exactly while tools are deferred, so allowlists never drop it.
    pub fn names(&self) -> Vec<String> {
        self.tools
            .keys()
            .chain(self.hidden.keys())
            .filter(|name| *name != SEARCH_TOOL)
            .cloned()
            .collect()
    }

    /// Whether `name` is a deferred tool (callable only once activated).
    pub fn is_deferred(&self, name: &str) -> bool {
        self.deferred.contains_key(name)
    }

    /// The names of registered hidden tools (`goal`, `think`, `yield`).
    #[must_use]
    pub fn hidden_names(&self) -> Vec<String> {
        self.hidden.keys().cloned().collect()
    }

    /// Number of deferred tools.
    pub fn deferred_count(&self) -> usize {
        self.deferred.len()
    }

    /// The system-prompt section naming deferred tool groups, if there are any.
    pub fn deferred_prompt(&self) -> Option<String> {
        prompt_section(
            &self.deferred,
            crate::prompt_data::budgets().deferred_groups,
        )
    }

    /// Deterministic sorted JSON schemas of the tools that are not deferred.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs_with(&[])
    }

    /// [`Self::specs`] plus the schemas of the `activated` deferred tools, appended in sorted
    /// name order (a stable array end keeps the cached prompt-tools prefix across activations).
    /// Names that are not deferred tools of this registry are ignored.
    pub fn specs_with(&self, activated: &[String]) -> Vec<ToolSpec> {
        let loaded = self
            .tools
            .iter()
            .filter(|(name, _)| !self.deferred.contains_key(*name))
            .map(|(_, tool)| tool.spec());
        let mut extra: Vec<&String> = activated
            .iter()
            .filter(|name| self.deferred.contains_key(*name))
            .collect();
        extra.sort_unstable();
        extra.dedup();
        let extra = extra
            .into_iter()
            .filter_map(|name| self.tools.get(name))
            .map(|tool| tool.spec());
        let hidden = self
            .hidden
            .iter()
            .filter(|(name, _)| activated.iter().any(|a| a == *name))
            .map(|(_, tool)| tool.spec());
        loaded.chain(extra).chain(hidden).collect()
    }

    /// Remove a tool by name (e.g. for read-only subagent profiles).
    pub fn remove(&mut self, name: &str) {
        self.tools.remove(name);
        self.hidden.remove(name);
        if self.deferred.contains_key(name) {
            let had_search = self.tools.remove(SEARCH_TOOL).is_some();
            Arc::make_mut(&mut self.deferred).remove(name);
            if had_search {
                self.install_search();
            }
        }
    }

    /// Clone a filtered view containing only `ReadOnly` tools.
    pub fn read_only_view(&self) -> Self {
        let mut view = Self {
            tools: self
                .tools
                .iter()
                .filter(|(_, t)| t.effect() == ToolEffect::ReadOnly)
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect(),
            deferred: Arc::clone(&self.deferred),
            hidden: self
                .hidden
                .iter()
                .filter(|(_, t)| t.effect() == ToolEffect::ReadOnly)
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect(),
        };
        let gone: Vec<String> = view
            .deferred
            .keys()
            .filter(|name| !view.tools.contains_key(*name))
            .cloned()
            .collect();
        if !gone.is_empty() {
            let had_search = view.tools.remove(SEARCH_TOOL).is_some();
            let catalog = Arc::make_mut(&mut view.deferred);
            for name in &gone {
                catalog.remove(name);
            }
            if had_search {
                view.install_search();
            }
        }
        view
    }

    /// Capture a non-owning snapshot of every registered tool (preventing `Arc` cycles when
    /// `EvalRuntimeParts` holds a back-reference to the owning runtime's tools).
    pub(crate) fn downgrade(&self) -> WeakToolRegistry {
        WeakToolRegistry {
            tools: self
                .tools
                .iter()
                .map(|(k, v)| (k.clone(), Arc::downgrade(v)))
                .collect(),
            deferred: Arc::clone(&self.deferred),
            hidden: self
                .hidden
                .iter()
                .map(|(k, v)| (k.clone(), Arc::downgrade(v)))
                .collect(),
        }
    }
}

/// Non-owning snapshot of a [`ToolRegistry`] whose entries upgrade only while the owning
/// runtime is still alive.
#[derive(Clone)]
pub(crate) struct WeakToolRegistry {
    tools: BTreeMap<String, Weak<dyn Tool>>,
    deferred: Arc<Catalog>,
    hidden: BTreeMap<String, Weak<dyn Tool>>,
}

impl WeakToolRegistry {
    /// Reconstruct a [`ToolRegistry`] from all tools still alive.
    pub(crate) fn upgrade(&self) -> ToolRegistry {
        ToolRegistry {
            tools: self
                .tools
                .iter()
                .filter_map(|(k, v)| v.upgrade().map(|t| (k.clone(), t)))
                .collect(),
            deferred: Arc::clone(&self.deferred),
            hidden: self
                .hidden
                .iter()
                .filter_map(|(k, v)| v.upgrade().map(|t| (k.clone(), t)))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::{BoxFuture, Result, ToolContext, ToolOutput};
    use serde_json::{Value, json};

    struct Stub(&'static str, ToolEffect);

    impl Tool for Stub {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.to_owned(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }
        }

        fn effect(&self) -> ToolEffect {
            self.1
        }

        fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
            Box::pin(async { Ok(ToolOutput::ok("")) })
        }
    }

    fn registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Stub("bash", ToolEffect::Mutating)));
        reg.defer(
            [
                ("mcp__s__read", ToolEffect::ReadOnly),
                ("mcp__s__write", ToolEffect::Mutating),
            ]
            .map(|(name, effect)| DeferredTool {
                group: "s".to_owned(),
                short_name: name.trim_start_matches("mcp__s__").to_owned(),
                tool: Arc::new(Stub(name, effect)),
            }),
        );
        reg
    }

    fn names(specs: Vec<ToolSpec>) -> Vec<String> {
        specs.into_iter().map(|s| s.name).collect()
    }

    #[test]
    fn deferred_schemas_join_only_once_activated() {
        let reg = registry();
        assert_eq!(names(reg.specs()), ["bash", "tool_search"]);
        let active = [
            "mcp__s__write".to_owned(),
            "bash".to_owned(),
            "nope".to_owned(),
        ];
        assert_eq!(
            names(reg.specs_with(&active)),
            ["bash", "tool_search", "mcp__s__write"],
            "activated tools are appended; non-deferred and unknown names are ignored"
        );
        assert_eq!(reg.deferred_count(), 2);
        assert!(reg.get("mcp__s__read").is_some());
    }

    #[test]
    fn removing_deferred_tools_keeps_the_search_catalog_in_step() {
        let mut reg = registry();
        reg.remove("mcp__s__write");
        assert_eq!(reg.deferred_count(), 1);
        assert!(reg.deferred_prompt().is_some_and(|p| !p.contains("write")));
        reg.remove("mcp__s__read");
        assert!(
            reg.get(SEARCH_TOOL).is_none(),
            "no deferred tools, no search"
        );
    }

    #[test]
    fn read_only_view_drops_mutating_deferred_tools() {
        let view = registry().read_only_view();
        assert_eq!(view.deferred_count(), 1);
        assert!(view.is_deferred("mcp__s__read"));
        assert!(view.get(SEARCH_TOOL).is_some());
    }
}
