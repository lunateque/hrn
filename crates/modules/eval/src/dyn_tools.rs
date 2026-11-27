//! Session-scoped registry of kernel-defined `@tool` functions shared to `task` subagents
//! via [`hrn_edge_tools::DynamicToolLookup`].

use crate::kernel::{CallEnd, CallResult};
use crate::lang::Lang;
use crate::pool::WeakKernelSlot;
use crate::spill::apply_spill;
use hrn_edge_rt::tokio;
use hrn_edge_tools::DynamicToolLookup;
use hrn_kernel::serde_json::Value;
use hrn_kernel::{
    BoxFuture, HrnError, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Timeout waiting to acquire the owning kernel slot when a subagent invokes a `@tool`.
const SLOT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// Default execution timeout for a kernel-defined `@tool` call.
const DYN_TOOL_TIMEOUT: Duration = Duration::from_mins(1);

struct RegisteredTool {
    lang: Lang,
    spec: ToolSpec,
    slot: WeakKernelSlot,
}

/// Thread-safe registry of `@tool`-defined functions keyed by `(session_id, tool_name)`.
#[derive(Default)]
pub struct KernelToolStore {
    entries: Mutex<BTreeMap<(String, String), RegisteredTool>>,
}

impl KernelToolStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or overwrite) a kernel-defined tool for `session_id`.
    pub(crate) fn register(
        &self,
        session_id: &str,
        lang: Lang,
        spec: ToolSpec,
        slot: WeakKernelSlot,
    ) {
        let key = (session_id.to_owned(), spec.name.clone());
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.insert(key, RegisteredTool { lang, spec, slot });
    }

    /// Remove all tools belonging to `(session_id, lang)` (called on kernel reset, crash, or eviction).
    pub(crate) fn clear_kernel(&self, session_id: &str, lang: Lang) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|(sess, _), entry| !(sess == session_id && entry.lang == lang));
    }

    /// List tool names currently registered for `session_id`.
    #[must_use]
    pub fn names_for(&self, session_id: &str) -> Vec<String> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .keys()
            .filter(|(sess, _)| sess == session_id)
            .map(|(_, name)| name.clone())
            .collect()
    }
}

impl DynamicToolLookup for KernelToolStore {
    fn resolve(&self, session_id: &str, names: &[String]) -> Result<Vec<Arc<dyn Tool>>> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out: Vec<Arc<dyn Tool>> = Vec::with_capacity(names.len());
        for name in names {
            let key = (session_id.to_owned(), name.clone());
            let Some(reg) = entries.get(&key) else {
                let known: Vec<&str> = entries
                    .keys()
                    .filter(|(sess, _)| sess == session_id)
                    .map(|(_, n)| n.as_str())
                    .collect();
                let suffix = if known.is_empty() {
                    "none registered in this session".to_owned()
                } else {
                    format!("registered: {}", known.join(", "))
                };
                return Err(HrnError::Tool(format!(
                    "unknown eval kernel tool `{name}` ({suffix})"
                )));
            };
            out.push(Arc::new(KernelDefinedTool {
                spec: reg.spec.clone(),
                slot: reg.slot.clone(),
            }));
        }
        Ok(out)
    }
}

/// Callable [`Tool`] forwarding invocations back to the owning `eval` kernel's `@tool` function.
struct KernelDefinedTool {
    spec: ToolSpec,
    slot: WeakKernelSlot,
}

impl Tool for KernelDefinedTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::exec()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let ctx = ctx.clone();
        let name = self.spec.name.clone();
        let slot = self.slot.clone();
        Box::pin(async move {
            let Some(slot) = slot.upgrade() else {
                return Ok(ToolOutput::err(format!(
                    "eval kernel tool `{name}` is unavailable because its owning kernel stopped"
                )));
            };
            let mut guard = tokio::time::timeout(SLOT_LOCK_TIMEOUT, slot.lock())
                .await
                .map_err(|_| {
                    HrnError::Tool(format!(
                        "eval kernel tool `{name}` timed out waiting for the owning kernel"
                    ))
                })?;
            let Some(kernel) = guard.as_mut() else {
                return Ok(ToolOutput::err(format!(
                    "eval kernel tool `{name}` is unavailable because its owning kernel stopped"
                )));
            };
            if kernel.is_dead() {
                guard.take();
                return Ok(ToolOutput::err(format!(
                    "eval kernel tool `{name}` is unavailable because its owning kernel exited"
                )));
            }
            let token = kernel.bridge_token().to_owned();
            let CallResult {
                output,
                images,
                end,
            } = kernel
                .call_defined_tool(&ctx, &name, &args, Some(DYN_TOOL_TIMEOUT))
                .await;
            if matches!(end, CallEnd::Died | CallEnd::TimedOut | CallEnd::Cancelled)
                && let Some(dead) = guard.take()
            {
                let _ = dead.kill();
            }
            drop(guard);
            if matches!(end, CallEnd::Cancelled) {
                return Err(HrnError::Cancelled);
            }
            let (text, is_error) = match end {
                CallEnd::Reply { value, error } => {
                    let mut body = output;
                    if let Some(val) = value {
                        if !body.is_empty() && !body.ends_with('\n') {
                            body.push('\n');
                        }
                        body.push_str(&val);
                    }
                    if let Some(err) = error {
                        if !body.is_empty() && !body.ends_with('\n') {
                            body.push('\n');
                        }
                        body.push_str(&err);
                        (body, true)
                    } else {
                        (body, false)
                    }
                }
                CallEnd::TimedOut => (format!("eval kernel tool `{name}` timed out"), true),
                CallEnd::Died => (
                    format!("owning eval kernel exited while running `{name}`"),
                    true,
                ),
                CallEnd::Cancelled => return Err(HrnError::Cancelled),
            };
            let display = if text.trim().is_empty() {
                "(no output)".to_owned()
            } else {
                text.trim_end().to_owned()
            };
            Ok(apply_spill(&ctx, &display, images, Some(&token), is_error))
        })
    }
}
