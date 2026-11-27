//! `apply_patch`: Codex-format multi-file patches.

use hrn_edge_tools::args::{blocking, required_str};
use hrn_edge_tools::{EditModeHint, FileReadTracker, apply_patch_sync_with_opts};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::sync::Arc;

/// The `apply_patch` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/apply_patch.md");

/// Codex-style multi-file patch applier.
pub(crate) struct ApplyPatchTool {
    tracker: Arc<FileReadTracker>,
    edit_hint: Arc<EditModeHint>,
}

impl ApplyPatchTool {
    pub(crate) fn new(tracker: Arc<FileReadTracker>, edit_hint: Arc<EditModeHint>) -> Self {
        Self { tracker, edit_hint }
    }
}

impl Tool for ApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "Patch text starting with `*** Begin Patch`." }
                },
                "required": ["patch"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::write()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let tracker = Arc::clone(&self.tracker);
        let opts = self.edit_hint.edit_opts();
        let cwd = ctx.cwd.clone();
        Box::pin(async move {
            let patch = required_str(&args, "patch", "apply_patch")?.to_owned();
            blocking("apply_patch", move || {
                apply_patch_sync_with_opts(&tracker, &cwd, &patch, &opts)
            })
            .await
        })
    }
}
