//! Post-edit lifecycle hook that appends fresh LSP diagnostics after file modifications.

use crate::pool::LspManager;
use hrn_kernel::serde_json::Value;
use hrn_kernel::{BoxFuture, HarnessHook, Result, ToolContext, ToolOutput};
use std::path::PathBuf;
use std::sync::Arc;

/// Appends compact `[LSP diagnostics]` (and optional `[LSP formatted]`) onto successful `edit`,
/// `write`, and `apply_patch` tool outputs whenever a matching LSP server reports results.
pub struct LspDiagnosticsHook {
    manager: Arc<LspManager>,
    cwd: PathBuf,
}

impl LspDiagnosticsHook {
    /// Construct a post-edit diagnostics hook bound to `manager` and `cwd`.
    pub fn new(manager: Arc<LspManager>, cwd: PathBuf) -> Self {
        Self { manager, cwd }
    }
}

impl HarnessHook for LspDiagnosticsHook {
    fn after_tool(
        &self,
        _ctx: &ToolContext,
        tool_name: &str,
        arguments: &Value,
        mut output: ToolOutput,
    ) -> BoxFuture<Result<ToolOutput>> {
        if !is_edit_tool(tool_name) || output.is_error {
            return Box::pin(async move { Ok(output) });
        }
        let paths = extract_edited_paths(tool_name, arguments);
        if paths.is_empty() {
            return Box::pin(async move { Ok(output) });
        }
        let mgr = Arc::clone(&self.manager);
        let cwd = self.cwd.clone();
        let is_write = tool_name == "write";
        let policy = mgr.hook_policy();
        let want_format = is_write && policy.format_on_write;
        let want_diags = if is_write {
            policy.diagnostics_on_write
        } else {
            policy.diagnostics_on_edit
        };
        if !want_format && !want_diags {
            return Box::pin(async move { Ok(output) });
        }
        Box::pin(async move {
            if want_format {
                let mut formatted = Vec::new();
                for path in &paths {
                    if let Some(label) = mgr.format_file(path, &cwd).await {
                        formatted.push(label);
                    }
                }
                if !formatted.is_empty() {
                    if !output.content.ends_with('\n') && !output.content.is_empty() {
                        output.content.push('\n');
                    }
                    output.content.push_str("\n[LSP formatted] ");
                    output.content.push_str(&formatted.join(", "));
                }
            }
            if want_diags && let Some(diags) = mgr.diagnostics_after_edit(&paths, &cwd).await {
                if !output.content.ends_with('\n') && !output.content.is_empty() {
                    output.content.push('\n');
                }
                output.content.push_str("\n[LSP diagnostics]\n");
                output.content.push_str(&diags);
            }
            Ok(output)
        })
    }
}

fn is_edit_tool(name: &str) -> bool {
    matches!(name, "edit" | "write" | "apply_patch")
}

fn path_arg(args: &Value) -> Vec<PathBuf> {
    args.get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|p| vec![PathBuf::from(p)])
        .unwrap_or_default()
}

pub(crate) fn extract_edited_paths(tool_name: &str, args: &Value) -> Vec<PathBuf> {
    match tool_name {
        "write" => path_arg(args),
        // `edit` supports all three flavours: hashline (`input`), patch (`patch`), replace (`path`).
        "edit" => {
            if let Some(input) = args.get("input").and_then(Value::as_str) {
                parse_hashline_paths(input)
            } else if let Some(patch) = args.get("patch").and_then(Value::as_str) {
                parse_patch_paths(patch)
            } else {
                path_arg(args)
            }
        }
        "apply_patch" => args
            .get("patch")
            .and_then(Value::as_str)
            .map(parse_patch_paths)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Files a hashline payload leaves behind: `[PATH#TAG]` headers, renamed by `MV`, dropped by `REM`.
fn parse_hashline_paths(input: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for line in input.lines().map(str::trim) {
        if let Some((path, _)) = line
            .strip_prefix('[')
            .and_then(|l| l.strip_suffix(']'))
            .and_then(|l| l.rsplit_once('#'))
        {
            out.push(PathBuf::from(path.trim()));
        } else if line == "REM" {
            out.pop();
        } else if let Some(dest) = line.strip_prefix("MV ") {
            let dest = dest.trim().trim_matches(['"', '\'']);
            if let Some(last) = out.last_mut() {
                *last = PathBuf::from(dest);
            }
        }
    }
    out.dedup();
    out
}

fn parse_patch_paths(patch: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in patch.lines() {
        let trimmed = line.trim();
        let target = trimmed
            .strip_prefix("*** Update File:")
            .or_else(|| trimmed.strip_prefix("*** Add File:"))
            .or_else(|| trimmed.strip_prefix("*** Move to:"));
        if let Some(p) = target.map(str::trim).filter(|s| !s.is_empty()) {
            let pb = PathBuf::from(p);
            if !out.contains(&pb) {
                out.push(pb);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::serde_json::json;

    #[test]
    fn extracts_paths_from_edit_write_and_patch() {
        assert_eq!(
            extract_edited_paths(
                "edit",
                &json!({"path": "src/lib.rs", "old": "a", "new": "b"})
            ),
            vec![PathBuf::from("src/lib.rs")]
        );
        let patch =
            "*** Begin Patch\n*** Update File: src/a.rs\n@@\n*** Add File: src/b.rs\n*** End Patch";
        assert_eq!(
            extract_edited_paths("apply_patch", &json!({"patch": patch})),
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")]
        );
        let hashline = "[a.rs#1A2B]\nPUT 1.=1:\n+x\n[b.rs#0000]\nREM\n[c.rs#FFFF]\nMV \"d e.rs\"";
        assert_eq!(
            extract_edited_paths("edit", &json!({"input": hashline})),
            vec![PathBuf::from("a.rs"), PathBuf::from("d e.rs")]
        );
        assert_eq!(
            extract_edited_paths("edit", &json!({"patch": patch})),
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")]
        );
    }

    #[tokio::test]
    async fn after_tool_uses_current_arguments_without_leaking_prior_calls() {
        let dir = std::env::temp_dir().join(format!("hrn-lsp-hook-noleak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let mgr = Arc::new(LspManager::new(crate::config::LspConfig::default()));
        let hook = LspDiagnosticsHook::new(mgr, dir.clone());
        let ctx = hrn_kernel::testing::tool_context(dir.clone());

        // Simulate a prior call that was short-circuited before `after_tool` ran, followed by a
        // write call with no valid path: must NOT replay the earlier call's path.
        let _ = hook
            .before_tool(&ctx, "write", &json!({"path": "first.rs"}))
            .await
            .expect("before");
        let out = hook
            .after_tool(&ctx, "write", &json!({}), ToolOutput::ok("noop"))
            .await
            .expect("after");
        assert_eq!(out.content, "noop");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
