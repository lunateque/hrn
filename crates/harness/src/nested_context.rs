//! Nested context files: `AGENTS.md` / `CLAUDE.md` / `GEMINI.md` in directories *below* the
//! session's `cwd`, injected into the result of the first tool call that touches a file under
//! them (once per directory per session). Same convention and seen-set shape as the rules
//! auto-inject hook — harness-forced inclusion, no model-initiated reads required.

use crate::rules::touched_paths;
use hrn_kernel::{BoxFuture, HarnessHook, PathKind, Result, ToolContext, ToolOutput};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The file names a nested context directory may carry, in precedence order.
const NESTED_NAMES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", "GEMINI.md"];
/// Longest nested context file injected.
const MAX_NESTED_BYTES: usize = 16 * 1024;

/// Injects nested instruction files on first touch of their directory.
pub struct NestedContextHook {
    fs: Arc<dyn hrn_kernel::WorkspaceFs>,
    /// Directories already injected, keyed by `(session_id, directory)`.
    seen: Arc<Mutex<HashSet<(String, String)>>>,
}

impl NestedContextHook {
    /// A hook reading through `fs`, sharing one runtime-wide seen set.
    #[must_use]
    pub fn new(fs: Arc<dyn hrn_kernel::WorkspaceFs>) -> Self {
        Self {
            fs,
            seen: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Whether `dir` is being injected into `session_id` for the first time.
    fn first_touch(&self, session_id: &str, dir: &Path) -> bool {
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        seen.insert((session_id.to_owned(), dir.display().to_string()))
    }

    /// The directories between `cwd` (exclusive) and the touched file (inclusive parent),
    /// outermost first.
    fn chain(cwd: &Path, file: &Path) -> Vec<PathBuf> {
        let Ok(rel) = file.strip_prefix(cwd) else {
            return Vec::new();
        };
        let mut dirs = Vec::new();
        let mut cur = cwd.to_path_buf();
        for comp in rel.parent().unwrap_or(Path::new("")).components() {
            cur = cur.join(comp);
            dirs.push(cur.clone());
        }
        dirs
    }
}

impl HarnessHook for NestedContextHook {
    fn after_tool(
        &self,
        ctx: &ToolContext,
        tool_name: &str,
        arguments: &Value,
        mut output: ToolOutput,
    ) -> BoxFuture<Result<ToolOutput>> {
        if !output.is_error {
            for rel in touched_paths(&ctx.cwd, tool_name, arguments) {
                let file = if Path::new(&rel).is_absolute() {
                    PathBuf::from(&rel)
                } else {
                    ctx.cwd.join(&rel)
                };
                for dir in Self::chain(&ctx.cwd, &file) {
                    if !self.first_touch(&ctx.session_id, &dir) {
                        continue;
                    }
                    for name in NESTED_NAMES {
                        let path = dir.join(name);
                        if self.fs.kind(&path) != PathKind::File {
                            continue;
                        }
                        let Ok(Some(raw)) = self.fs.read_to_string(&path) else {
                            continue;
                        };
                        let trimmed = raw.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let kept = crate::prompt_data::clip_bytes(trimmed, MAX_NESTED_BYTES);
                        let _ = write!(
                            output.content,
                            "\n\n# Nested Instructions ({})\n{kept}",
                            path.display()
                        );
                    }
                }
            }
        }
        Box::pin(async move { Ok(output) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::hook_ctx;
    use serde_json::json;

    struct Fs;
    impl hrn_kernel::WorkspaceFs for Fs {
        fn read_to_string(&self, path: &Path) -> Result<Option<String>> {
            Ok(if path == Path::new("/ws/pkg/AGENTS.md") {
                Some("pkg rules".to_owned())
            } else {
                None
            })
        }
        fn list_dir(&self, _path: &Path) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
        fn kind(&self, path: &Path) -> PathKind {
            if path == Path::new("/ws/pkg/AGENTS.md") {
                PathKind::File
            } else {
                PathKind::Missing
            }
        }
        fn home_dir(&self) -> Option<PathBuf> {
            None
        }
    }

    #[test]
    fn a_touched_directory_injects_its_nested_file_once() {
        let hook = NestedContextHook::new(Arc::new(Fs));
        let ctx = hook_ctx();
        let args = json!({"path": "pkg/a.rs"});
        let first =
            crate::testutil::block_on(hook.after_tool(&ctx, "read", &args, ToolOutput::ok("body")))
                .expect("hook");
        assert!(first.content.contains("pkg rules"), "{}", first.content);
        let second =
            crate::testutil::block_on(hook.after_tool(&ctx, "read", &args, ToolOutput::ok("body")))
                .expect("hook");
        assert!(!second.content.contains("pkg rules"), "once per session");
    }
}
