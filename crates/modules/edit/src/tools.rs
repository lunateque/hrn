//! The three `edit` tool flavours (`hashline`, `replace`, `apply_patch`) wired to shared
//! [`FileReadTracker`], `[edit]` policy knobs, and optional [`PostEditRepair`].

use crate::repair::PostEditRepair;
use hrn_edge_tools::args::{blocking, flag, required_str, summary_of};
use hrn_edge_tools::paths::resolve;
use hrn_edge_tools::{
    EditOpts, FileReadTracker, HashlineEditor, HashlineOpts, apply_patch_sync_with_opts,
    edit_file_sync_with_opts, header_paths,
};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::path::PathBuf;
use std::sync::Arc;

const HASHLINE_DESC: &str = include_str!("../prompts/edit.md");
const REPLACE_DESC: &str = include_str!("../prompts/edit-replace.md");
const PATCH_DESC: &str = include_str!("../prompts/edit-patch.md");

/// Hashline `edit` tool (`mode = "hashline"`).
pub(crate) struct HashlineEditTool {
    tracker: Arc<FileReadTracker>,
    editor: Arc<HashlineEditor>,
    repair: PostEditRepair,
}

impl HashlineEditTool {
    pub(crate) fn new(
        tracker: Arc<FileReadTracker>,
        opts: HashlineOpts,
        repair: PostEditRepair,
    ) -> Self {
        let editor = Arc::new(HashlineEditor::with_opts(Arc::clone(&tracker), opts));
        Self {
            tracker,
            editor,
            repair,
        }
    }
}

impl Tool for HashlineEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".to_owned(),
            description: HASHLINE_DESC.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "One or more `[PATH#TAG]` sections of hashline operations."
                    }
                },
                "required": ["input"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::write()
    }

    fn summarize_call(&self, args: &Value) -> String {
        let paths = args
            .get("input")
            .and_then(Value::as_str)
            .map(header_paths)
            .unwrap_or_default();
        match paths.as_slice() {
            [] => "<unknown>".to_owned(),
            [one] => one.clone(),
            [first, more @ ..] => format!("{first} +{} more", more.len()),
        }
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let editor = Arc::clone(&self.editor);
        let tracker = Arc::clone(&self.tracker);
        let repair = self.repair.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let input = required_str(&args, "input", "edit")?.to_owned();
            let before = if repair.is_active() {
                let targets: Vec<PathBuf> = header_paths(&input)
                    .into_iter()
                    .filter(|p| !p.contains("://"))
                    .map(|p| resolve(&ctx.cwd, &p))
                    .collect();
                repair.capture_before(&targets)
            } else {
                Vec::new()
            };
            let ctx_run = ctx.clone();
            let output = blocking("edit", move || editor.edit(&ctx_run, &input)).await?;
            repair.run(&ctx, &tracker, &before, output).await
        })
    }
}

/// Search-replace `edit` tool (`mode = "replace"` / `"patch"` / `"sloppy"`).
pub(crate) struct ReplaceEditTool {
    tracker: Arc<FileReadTracker>,
    opts: EditOpts,
    repair: PostEditRepair,
}

impl ReplaceEditTool {
    pub(crate) fn new(
        tracker: Arc<FileReadTracker>,
        opts: EditOpts,
        repair: PostEditRepair,
    ) -> Self {
        Self {
            tracker,
            opts,
            repair,
        }
    }
}

impl Tool for ReplaceEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".to_owned(),
            description: REPLACE_DESC.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File to edit (relative to cwd or absolute)." },
                    "old": { "type": "string", "description": "Text to replace." },
                    "new": { "type": "string", "description": "Replacement text." },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)." }
                },
                "required": ["path", "old", "new"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::write()
    }

    fn summarize_call(&self, args: &Value) -> String {
        summary_of(args, "path")
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let tracker = Arc::clone(&self.tracker);
        let opts = self.opts.clone();
        let repair = self.repair.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let path = resolve(&ctx.cwd, required_str(&args, "path", "edit")?);
            let old = required_str(&args, "old", "edit")?.to_owned();
            let new = required_str(&args, "new", "edit")?.to_owned();
            let replace_all = flag(&args, "replace_all");
            let before = repair.capture_before(std::slice::from_ref(&path));
            let tracker_run = Arc::clone(&tracker);
            let cwd = ctx.cwd.clone();
            let path_run = path.clone();
            let output = blocking("edit", move || {
                edit_file_sync_with_opts(
                    &tracker_run,
                    &cwd,
                    &path_run,
                    &old,
                    &new,
                    replace_all,
                    &opts,
                )
            })
            .await?;
            repair.run(&ctx, &tracker, &before, output).await
        })
    }
}

/// Codex multi-file patch `edit` tool (`mode = "apply_patch"`).
pub(crate) struct PatchEditTool {
    tracker: Arc<FileReadTracker>,
    opts: EditOpts,
}

impl PatchEditTool {
    pub(crate) fn new(tracker: Arc<FileReadTracker>, opts: EditOpts) -> Self {
        Self { tracker, opts }
    }
}

impl Tool for PatchEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".to_owned(),
            description: PATCH_DESC.to_owned(),
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
        let opts = self.opts.clone();
        let cwd = ctx.cwd.clone();
        Box::pin(async move {
            let patch = required_str(&args, "patch", "edit")?.to_owned();
            blocking("edit", move || {
                apply_patch_sync_with_opts(&tracker, &cwd, &patch, &opts)
            })
            .await
        })
    }
}
