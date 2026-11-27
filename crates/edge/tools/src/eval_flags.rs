//! Process-wide shared coordination slots between `hrn-mod-eval`, `hrn-mod-desktop`, and the
//! binary crate via [`hrn_kernel::Shared`].
//!
//! Keeping these tiny port types in `hrn-edge-tools` preserves strict star-graph isolation and
//! module removability: neither module depends on the other, and `crates/bins/harness/src/wiring.rs`
//! never names module feature flags outside `modules.rs`.

use hrn_kernel::{Result, Tool};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Default subagent concurrency bound advertised to eval `workpool()` before wiring overrides it.
pub const DEFAULT_EVAL_MAX_CONCURRENCY: usize = 32;

/// Process-wide hints shared into `hrn-mod-eval` from `hrn-mod-desktop` and binary wiring.
pub struct EvalRuntimeHints {
    desktop_eval_prelude: AtomicBool,
    desktop_tool_registered: AtomicBool,
    max_concurrency: AtomicUsize,
    registered_tools: Mutex<BTreeSet<String>>,
}

impl Default for EvalRuntimeHints {
    fn default() -> Self {
        Self {
            desktop_eval_prelude: AtomicBool::new(false),
            desktop_tool_registered: AtomicBool::new(false),
            max_concurrency: AtomicUsize::new(DEFAULT_EVAL_MAX_CONCURRENCY),
            registered_tools: Mutex::new(BTreeSet::new()),
        }
    }
}

impl EvalRuntimeHints {
    /// Publish `[desktop] eval_prelude` from `hrn-mod-desktop::register`.
    pub fn set_desktop_eval_prelude(&self, enabled: bool) {
        self.desktop_eval_prelude.store(enabled, Ordering::Release);
    }

    /// Record whether the `desktop` tool is registered in the assembled tool registry.
    pub fn set_desktop_tool_registered(&self, registered: bool) {
        self.desktop_tool_registered
            .store(registered, Ordering::Release);
    }

    /// Whether the `computer.*` eval prelude should activate (requires BOTH `[desktop] eval_prelude`
    /// and a registered `desktop` tool).
    #[must_use]
    pub fn computer_prelude_active(&self) -> bool {
        self.desktop_eval_prelude.load(Ordering::Acquire)
            && self.desktop_tool_registered.load(Ordering::Acquire)
    }

    /// Publish `[task] max_concurrency` so eval's `workpool()` enforces the same bound.
    pub fn set_max_concurrency(&self, limit: usize) {
        self.max_concurrency.store(limit, Ordering::Release);
    }

    /// Effective `[task] max_concurrency` bound for eval's `workpool()`.
    #[must_use]
    pub fn max_concurrency(&self) -> usize {
        self.max_concurrency.load(Ordering::Acquire)
    }

    /// Publish the set of host/module/MCP tool names registered in the runtime so kernel `@tool`
    /// definitions can never shadow built-in or host tools.
    pub fn set_registered_tool_names<I>(&self, names: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut guard = self
            .registered_tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.clear();
        guard.extend(names);
    }

    /// Whether `name` collides with a tool registered in the host runtime.
    #[must_use]
    pub fn is_registered_tool(&self, name: &str) -> bool {
        self.registered_tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(name)
    }
}

/// Port implemented by `hrn-mod-eval` to expose kernel-defined `@tool` functions to `task` children.
pub trait DynamicToolLookup: Send + Sync {
    /// Resolve requested kernel tool names defined in `session_id` into callable [`Tool`] instances.
    ///
    /// # Errors
    /// Returns an error if any requested tool name is not registered for `session_id`.
    fn resolve(&self, session_id: &str, names: &[String]) -> Result<Vec<Arc<dyn Tool>>>;
}

/// Process-wide slot holding `hrn-mod-eval`'s active [`DynamicToolLookup`] when `[eval]
/// tools_enabled = true`.
#[derive(Default)]
pub struct EvalDynamicToolsSlot {
    lookup: Mutex<Option<Arc<dyn DynamicToolLookup>>>,
}

impl EvalDynamicToolsSlot {
    /// Install a dynamic tool lookup implementation.
    pub fn set(&self, lookup: Arc<dyn DynamicToolLookup>) {
        *self.lookup.lock().unwrap_or_else(PoisonError::into_inner) = Some(lookup);
    }

    /// Clear any installed dynamic tool lookup (e.g. when `[eval] tools_enabled = false`).
    pub fn clear(&self) {
        *self.lookup.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Snapshot the currently registered dynamic tool lookup, if any.
    #[must_use]
    pub fn get(&self) -> Option<Arc<dyn DynamicToolLookup>> {
        self.lookup
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether kernel-defined dynamic tools are currently enabled and available.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.lookup
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}
