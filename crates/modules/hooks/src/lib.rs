//! User extensibility: hook scripts, custom tools and skill management.
//!
//! - **User hooks** — executables under any layer's `hooks/` directory, commands in the
//!   `[hooks]` config table, paths from `--hook`, and Claude-Code-style `settings.json` hooks.
//!   Each gets a JSON event on stdin (`pre_tool`, `post_tool`, `turn_end`, `session_start`,
//!   `user_prompt`) and answers with a JSON decision (block, rewrite arguments, inject context)
//!   under a timeout.
//! - **Custom tools** — a `<name>.json` schema plus a `<name>` executable (or `.mjs` module) in
//!   any layer's `tools/` directory; arguments arrive as JSON on stdin, the result comes back on
//!   stdout.
//! - **`manage_skill`** — create/update/delete/list skills in `~/.harness/skills`.
//! - **`hooks://`** — the live inventory of discovered subscriptions per event.

mod config;
mod decision;
mod discover;
mod hook;
mod run;
mod tools;

use hrn_kernel::serde_json::json;
use hrn_kernel::{Registry, Result, Tool, ToolContext, UrlBody, UrlScheme};
use std::sync::Arc;

/// `hooks://`: which hook subscriptions each event currently has.
struct HooksScheme {
    runner: Arc<hook::UserHookRunner>,
}

impl UrlScheme for HooksScheme {
    fn scheme(&self) -> &'static str {
        "hooks"
    }

    fn about(&self) -> &'static str {
        "user hooks: bare `hooks://` lists events and their discovered subscriptions"
    }

    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        let rest = rest.trim_matches('/');
        if !rest.is_empty() {
            let subs = self.runner.subs(&ctx.cwd, rest);
            return Ok(UrlBody::Text(
                json!({"event": rest, "hooks": subs.iter().map(|s| s.label.clone()).collect::<Vec<_>>()}).to_string(),
            ));
        }
        let lines: Vec<String> = self
            .runner
            .inventory(&ctx.cwd)
            .into_iter()
            .map(|(event, count)| format!("{event}: {count}"))
            .collect();
        Ok(UrlBody::Text(lines.join("\n")))
    }
}

/// Register the module's tools, schemes, hooks and config.
///
/// # Errors
///
/// Fails when the `[hooks]` table is invalid.
pub fn register(registry: &mut Registry) -> Result {
    let config = Arc::new(config::load(registry)?);
    if !config.enabled {
        return Ok(());
    }
    let home = std::env::home_dir().filter(|p| !p.as_os_str().is_empty());
    let runner = Arc::new(hook::UserHookRunner::new(Arc::clone(&config), home.clone()));
    registry.hook(Arc::clone(&runner) as Arc<dyn hrn_kernel::HarnessHook>);
    registry.scheme(Arc::new(HooksScheme { runner }))?;
    registry.tool(Arc::new(tools::ManageSkillTool::new(home.clone())) as Arc<dyn Tool>)?;
    let cwd = registry.host().cwd.clone();
    let timeout = std::time::Duration::from_secs(config.timeout_secs.max(1));
    if config.discover_tools {
        let (defs, errors) = discover::discover_custom_tools(home.as_deref(), &cwd);
        // A broken tool definition is a config error: say so instead of half-loading it.
        if let Some(err) = errors.into_iter().next() {
            return Err(err);
        }
        for def in defs {
            registry.tool(Arc::new(tools::CustomTool::new(def, timeout)) as Arc<dyn Tool>)?;
        }
    }
    Ok(())
}
