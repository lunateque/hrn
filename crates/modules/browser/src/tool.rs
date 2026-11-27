//! The `browser` tool backed by named CDP / Chrome-relay tabs and compact AX tree snapshots.

use crate::actions::run_action;
use crate::tabs::BrowserManager;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::sync::Arc;

/// Native CDP / Chrome-relay browser automation tool (`open`, `snapshot`, `click`, `fill`,
/// `press`, `eval`, `screenshot`, `console`, `network`, `tabs`, `close`).
pub struct BrowserTool {
    manager: Arc<BrowserManager>,
}

impl BrowserTool {
    /// Wrap a shared [`BrowserManager`].
    pub fn new(manager: Arc<BrowserManager>) -> Self {
        Self { manager }
    }
}

/// The `browser` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/browser.md");

impl Tool for BrowserTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["open", "snapshot", "click", "fill", "press", "eval", "screenshot", "console", "network", "tabs", "close"],
                        "description": "Browser operation to execute."
                    },
                    "app": {
                        "type": "string",
                        "description": "Backend selector: \"relay\" (user's Chrome), \"auto\", a loopback CDP URL (`http://...`/`ws://...`), or an executable path."
                    },
                    "name": {
                        "type": "string",
                        "description": "Named tab handle (default \"default\"). A new name opens a background tab."
                    },
                    "target": {
                        "type": "string",
                        "description": "Borrow an existing Chrome/CDP tab by id, URL, or title substring."
                    },
                    "url": { "type": "string", "description": "URL to navigate to (`open`)." },
                    "ref": { "type": "string", "description": "Accessibility element ref like `@e5` from the latest `snapshot` (`click`, `fill`)." },
                    "selector": { "type": "string", "description": "CSS selector fallback when `ref` is omitted (`click`, `fill`)." },
                    "value": { "type": "string", "description": "Text value to type/set (`fill`)." },
                    "submit": { "type": "boolean", "description": "Press Enter after `fill` (default false)." },
                    "key": { "type": "string", "description": "Key name for `press` (e.g. Enter, Tab, Escape, ArrowDown)." },
                    "expression": { "type": "string", "description": "JavaScript expression to evaluate (`eval`)." },
                    "full_page": { "type": "boolean", "description": "Capture full scrollable page (`screenshot`, default false)." },
                    "max_nodes": { "type": "integer", "description": "Max accessibility tree lines (`snapshot`, default 250)." },
                    "wait_ms": { "type": "integer", "description": "Optional settle delay in ms after `open` (default 500, max 10000)." },
                    "clear": { "type": "boolean", "description": "Clear buffered entries after reading (`console`, `network`)." }
                },
                "required": ["action"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, args: &Value) -> ToolApproval {
        if args
            .get("app")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
        {
            return ToolApproval::exec();
        }
        match args.get("action").and_then(Value::as_str) {
            Some("snapshot" | "screenshot" | "tabs") => ToolApproval::read(),
            Some("console" | "network") => {
                if args.get("clear").and_then(Value::as_bool).unwrap_or(false) {
                    ToolApproval::write()
                } else {
                    ToolApproval::read()
                }
            }
            _ => ToolApproval::exec(),
        }
    }

    fn summarize_call(&self, args: &Value) -> String {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("?");
        if let Some(url) = args.get("url").and_then(Value::as_str) {
            return format!("{action} {url}");
        }
        if let Some(target) = args.get("target").and_then(Value::as_str) {
            return format!("{action} target={target}");
        }
        if let Some(r) = args.get("ref").and_then(Value::as_str) {
            return format!("{action} {r}");
        }
        if let Some(sel) = args.get("selector").and_then(Value::as_str) {
            return format!("{action} {sel}");
        }
        if let Some(name) = args.get("name").and_then(Value::as_str) {
            return format!("{action} [{name}]");
        }
        action.to_owned()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let mgr = Arc::clone(&self.manager);
        let is_subagent = ctx.is_subagent;
        Box::pin(async move { run_action(&mgr, &args, is_subagent).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BrowserConfig;

    #[test]
    fn browser_approval_splits_read_write_and_exec() {
        let tool = BrowserTool::new(Arc::new(BrowserManager::new(BrowserConfig::default())));
        for action in ["snapshot", "screenshot", "tabs", "console", "network"] {
            assert_eq!(
                tool.approval(&json!({"action": action})).tier,
                hrn_kernel::ToolTier::Read,
                "{action}"
            );
        }
        assert_eq!(
            tool.approval(&json!({"action": "console", "clear": true}))
                .tier,
            hrn_kernel::ToolTier::Write
        );
        for action in ["open", "click", "fill", "press", "eval", "close"] {
            assert_eq!(
                tool.approval(&json!({"action": action})).tier,
                hrn_kernel::ToolTier::Exec,
                "{action}"
            );
        }
        // Supplying non-empty `app` on ANY action (even read-only actions) requires `Exec` tier
        // because `app` can spawn an arbitrary local executable.
        for action in ["snapshot", "screenshot", "tabs", "console", "network"] {
            assert_eq!(
                tool.approval(&json!({"action": action, "app": "/usr/bin/calc"}))
                    .tier,
                hrn_kernel::ToolTier::Exec,
                "non-empty app on `{action}` must be Exec"
            );
            assert_eq!(
                tool.approval(&json!({"action": action, "app": "   "})).tier,
                hrn_kernel::ToolTier::Read,
                "blank app on `{action}` stays Read"
            );
        }
    }
}
