//! Whole-file writes through the client: when the ACP client advertises `fs.writeTextFile`, a
//! `write` of a workspace file is handed to it (as omp's ACP bridge does) instead of being written
//! here, so an editor buffer stays authoritative. Everything else, including internal URIs and
//! paths outside the workspace, runs the tool's own local write.

use super::Conn;
use hrn_kernel::{
    BoxFuture, CancelFlag, HarnessHook, HookVerdict, Result, ToolContext, ToolOutput,
};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Short-circuits `write` in favour of the client's `fs/write_text_file`.
pub(crate) struct WriteThrough {
    conn: Arc<Conn>,
    session: String,
    cwd: PathBuf,
    cancel: CancelFlag,
}

impl WriteThrough {
    pub(crate) fn new(conn: Arc<Conn>, session: String, cwd: PathBuf, cancel: CancelFlag) -> Self {
        Self {
            conn,
            session,
            cwd,
            cancel,
        }
    }

    /// The absolute path of the tool's `path` argument, `None` when it is a URI or escapes `cwd`.
    fn target(&self, raw: &str) -> Option<PathBuf> {
        if raw.contains("://") {
            return None;
        }
        let given = Path::new(raw);
        if given.is_absolute() {
            return Some(given.to_path_buf());
        }
        let mut normalized = self.cwd.clone();
        for part in given.components() {
            match part {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !normalized.pop() {
                        return None;
                    }
                }
                Component::Normal(name) => normalized.push(name),
                // A prefix (`C:`) or root on a relative path: treat as absolute.
                Component::Prefix(_) | Component::RootDir => return None,
            }
        }
        normalized.starts_with(&self.cwd).then_some(normalized)
    }
}

impl HarnessHook for WriteThrough {
    fn before_tool(
        &self,
        _ctx: &ToolContext,
        tool_name: &str,
        arguments: &Value,
    ) -> BoxFuture<Result<HookVerdict>> {
        if tool_name != "write" || !self.conn.client_has(&["fs", "writeTextFile"]) {
            return Box::pin(async { Ok(HookVerdict::Continue) });
        }
        let Some(raw) = arguments.get("path").and_then(Value::as_str) else {
            return Box::pin(async { Ok(HookVerdict::Continue) });
        };
        let Some(content) = arguments.get("content").and_then(Value::as_str) else {
            return Box::pin(async { Ok(HookVerdict::Continue) });
        };
        let Some(target) = self.target(raw) else {
            return Box::pin(async { Ok(HookVerdict::Continue) });
        };
        let conn = Arc::clone(&self.conn);
        let session = self.session.clone();
        let cancel = self.cancel.clone();
        let text = target.to_string_lossy().replace('\\', "/");
        let content = content.to_owned();
        Box::pin(async move {
            match conn
                .write_text_file(&session, &text, &content, &cancel)
                .await
            {
                // A client that refused or failed to write: fall back to the local write.
                None => Ok(HookVerdict::Continue),
                Some(reply) if reply.get("error").is_some() => Ok(HookVerdict::Continue),
                Some(_) => Ok(HookVerdict::ShortCircuit(ToolOutput::ok(format!(
                    "Wrote `{text}` through the client ({} lines, {} bytes).",
                    content.lines().count(),
                    content.len()
                )))),
            }
        })
    }
}
