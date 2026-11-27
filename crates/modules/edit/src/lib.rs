//! `edit`: dedicated file-editing module (`hrn-mod-edit`) owning `[edit]` configuration,
//! mode selection (`hashline` | `replace` | `apply_patch` | `none`), model-variant & family
//! resolution, auto-generated file protection, seen-line enforcement, and optional AST syntax
//! auto-repair.
//!
//! Shares [`FileReadTracker`] and [`EditModeHint`] with `hrn-mod-fs` strictly via
//! [`hrn_kernel::Shared`], so removing `edit` or removing `fs` compiles and passes tests cleanly.
//!
//! Removable with `cargo xtask module remove edit`.

mod config;
mod repair;
mod tools;

#[cfg(test)]
mod tests;

pub use config::{
    EditConfig, EditMode, ResolvedMode, generated_globs, matches_replace_family, parse_mode,
    replace_families,
};

use config::RawEditTable;
use hrn_edge_tools::{EditModeHint, EditOpts, FileReadTracker, HashlineOpts};
use hrn_kernel::{Registry, Result, Tool};
use repair::PostEditRepair;
use std::sync::Arc;
use tools::{HashlineEditTool, PatchEditTool, ReplaceEditTool};

/// Collect active model identifiers (`provider/id`, `id`, `alias`) from `hint` and `registry.llm()`.
fn active_model_strings(registry: &Registry, hint: &EditModeHint) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(override_model) = hint.active_model()
        && !override_model.trim().is_empty()
    {
        out.push(override_model);
    }
    if let Some(llm) = registry.llm()
        && let Some(def) = llm.models().into_iter().find(|m| m.default)
    {
        if !def.provider.is_empty() && !def.id.is_empty() {
            out.push(format!("{}/{}", def.provider, def.id));
        }
        if !def.id.is_empty() {
            out.push(def.id);
        }
        if !def.alias.is_empty() {
            out.push(def.alias);
        }
    }
    out
}

/// Register the `edit` tool (when resolved mode is not `none`) and publish shared hints for
/// `read`/`grep` anchor formatting and `turn.rs` inline-edit recovery.
///
/// # Errors
///
/// When `[edit]` is malformed, an invalid mode/variant is configured, or `edit` is already taken.
pub fn register(registry: &mut Registry) -> Result {
    let raw: RawEditTable = registry.config()?;
    let config = EditConfig::from_raw(raw)?;
    let env_variant = std::env::var("HRN_EDIT_VARIANT").ok();
    let strict_env = std::env::var("HRN_STRICT_EDIT_MODE").ok();
    register_with_env(
        registry,
        &config,
        env_variant.as_deref(),
        strict_env.as_deref(),
    )
}

/// Registration helper with injectable environment overrides for deterministic testing.
pub(crate) fn register_with_env(
    registry: &mut Registry,
    config: &EditConfig,
    env_variant: Option<&str>,
    strict_env: Option<&str>,
) -> Result {
    let tracker = registry
        .shared()
        .get_or_init(|| Ok(FileReadTracker::new()))?;
    let hint = registry.shared().get_or_init(|| Ok(EditModeHint::new()))?;
    let active_models = active_model_strings(registry, &hint);
    let resolved = config.resolve_mode(&active_models, env_variant, strict_env)?;

    hint.set_hashline_anchors(resolved.mode.uses_hashline_anchors());
    hint.set_recover_inline_edits(config.recover_inline_edits);
    let blocked_globs = config.blocked_globs();
    hint.set_edit_policy(
        config.enforce_seen_lines,
        blocked_globs.clone(),
        config.fuzzy_match,
        config.fuzzy_threshold,
    );
    if let Some(note) = resolved.debug_note {
        hint.push_debug_note(note);
    }

    let repair = PostEditRepair::new(
        config.auto_repair,
        config.blackbox,
        config.enforce_seen_lines,
        registry.llm(),
    );
    let tool: Option<Arc<dyn Tool>> = match resolved.mode {
        EditMode::Hashline => Some(Arc::new(HashlineEditTool::new(
            tracker,
            HashlineOpts {
                enforce_seen_lines: config.enforce_seen_lines,
                fuzzy_match: config.fuzzy_match,
                fuzzy_threshold: config.fuzzy_threshold,
                blocked_globs,
            },
            repair,
        ))),
        EditMode::Replace => Some(Arc::new(ReplaceEditTool::new(
            tracker,
            EditOpts {
                fuzzy_match: config.fuzzy_match,
                fuzzy_threshold: config.fuzzy_threshold,
                enforce_seen_lines: config.enforce_seen_lines,
                blocked_globs,
            },
            repair,
        ))),
        EditMode::ApplyPatch => Some(Arc::new(PatchEditTool::new(
            tracker,
            EditOpts {
                fuzzy_match: config.fuzzy_match,
                fuzzy_threshold: config.fuzzy_threshold,
                enforce_seen_lines: config.enforce_seen_lines,
                blocked_globs,
            },
        ))),
        EditMode::None => None,
    };
    if let Some(editor) = tool {
        registry.tool(editor)?;
    }
    Ok(())
}
