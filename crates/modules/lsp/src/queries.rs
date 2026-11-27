//! Additional semantic LSP operations on [`LspManager`]: `type_definition`, `implementation`,
//! `incoming_calls`, `outgoing_calls`, `code_actions`, and explicit `format_document`.

use crate::format::{display_rel, format_call_hierarchy, format_code_actions, format_locations};
use crate::lifecycle::{resolve_path, text_doc_pos};
use crate::pool::LspManager;
use crate::text_edit::{apply_text_edits, parse_text_edits};
use hrn_edge_rt::tokio;
use hrn_edge_tools::{check_generated_guard, write_atomic};
use hrn_kernel::serde_json::json;
use hrn_kernel::{HrnError, Result};
use std::path::Path;

impl LspManager {
    /// Go to type definition at 1-based `(line, col)` in `path`.
    pub async fn type_definition(
        &self,
        path: &Path,
        line: u32,
        col: u32,
        cwd: &Path,
    ) -> Result<String> {
        let _ = self.evict_idle().await;
        let timeout = self.cfg().request_timeout;
        let abs = resolve_path(path, cwd)?;
        let (server, lang) = self.ensure_server_for_file(&abs, cwd).await?;
        let uri = server.sync_file(&abs, &lang).await?;
        let val = server
            .client
            .request(
                "textDocument/typeDefinition",
                text_doc_pos(&uri, line, col),
                timeout,
            )
            .await?;
        Ok(format_locations(&val, cwd, 20))
    }

    /// Find implementations of a trait / interface / abstract symbol at 1-based `(line, col)`.
    pub async fn implementation(
        &self,
        path: &Path,
        line: u32,
        col: u32,
        cwd: &Path,
    ) -> Result<String> {
        let _ = self.evict_idle().await;
        let timeout = self.cfg().request_timeout;
        let abs = resolve_path(path, cwd)?;
        let (server, lang) = self.ensure_server_for_file(&abs, cwd).await?;
        let uri = server.sync_file(&abs, &lang).await?;
        let val = server
            .client
            .request(
                "textDocument/implementation",
                text_doc_pos(&uri, line, col),
                timeout,
            )
            .await?;
        Ok(format_locations(&val, cwd, 30))
    }

    /// List callers (`callHierarchy/incomingCalls`) of the symbol at 1-based `(line, col)`.
    pub async fn incoming_calls(
        &self,
        path: &Path,
        line: u32,
        col: u32,
        cwd: &Path,
    ) -> Result<String> {
        self.call_hierarchy(
            path,
            line,
            col,
            cwd,
            "callHierarchy/incomingCalls",
            "incoming",
        )
        .await
    }

    /// List callees (`callHierarchy/outgoingCalls`) invoked from the symbol at 1-based `(line, col)`.
    pub async fn outgoing_calls(
        &self,
        path: &Path,
        line: u32,
        col: u32,
        cwd: &Path,
    ) -> Result<String> {
        self.call_hierarchy(
            path,
            line,
            col,
            cwd,
            "callHierarchy/outgoingCalls",
            "outgoing",
        )
        .await
    }

    async fn call_hierarchy(
        &self,
        path: &Path,
        line: u32,
        col: u32,
        cwd: &Path,
        method: &str,
        direction: &str,
    ) -> Result<String> {
        let _ = self.evict_idle().await;
        let timeout = self.cfg().request_timeout;
        let abs = resolve_path(path, cwd)?;
        let (server, lang) = self.ensure_server_for_file(&abs, cwd).await?;
        let uri = server.sync_file(&abs, &lang).await?;
        let prepared = server
            .client
            .request(
                "textDocument/prepareCallHierarchy",
                text_doc_pos(&uri, line, col),
                timeout,
            )
            .await?;
        let Some(first) = prepared.as_array().and_then(|arr| arr.first()).cloned() else {
            return Ok("No call hierarchy item at this position.".to_owned());
        };
        let calls = server
            .client
            .request(method, json!({ "item": first }), timeout)
            .await?;
        Ok(format_call_hierarchy(&calls, direction, cwd, 50))
    }

    /// List available quick fixes and refactorings (`textDocument/codeAction`) at 1-based range.
    pub async fn code_actions(
        &self,
        path: &Path,
        line: u32,
        col: u32,
        end_line: Option<u32>,
        end_col: Option<u32>,
        cwd: &Path,
    ) -> Result<String> {
        let _ = self.evict_idle().await;
        let timeout = self.cfg().request_timeout;
        let abs = resolve_path(path, cwd)?;
        let (server, lang) = self.ensure_server_for_file(&abs, cwd).await?;
        let uri = server.sync_file(&abs, &lang).await?;
        let start_l = line.saturating_sub(1);
        let start_c = col.saturating_sub(1);
        let end_l = end_line.unwrap_or(line).saturating_sub(1);
        let end_c = end_col.unwrap_or(col).saturating_sub(1);
        let params = json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": start_l, "character": start_c },
                "end": { "line": end_l, "character": end_c }
            },
            "context": { "diagnostics": [] }
        });
        let val = server
            .client
            .request("textDocument/codeAction", params, timeout)
            .await?;
        Ok(format_code_actions(&val, cwd, 30))
    }

    /// Explicitly format `path` via `textDocument/formatting` and atomically apply returned edits.
    pub async fn format_document(&self, path: &Path, cwd: &Path) -> Result<String> {
        let _ = self.evict_idle().await;
        let timeout = self.cfg().request_timeout;
        let abs = resolve_path(path, cwd)?;
        let shown = display_rel(&abs, cwd);
        if let Some(hint) = &self.edit_hint {
            let globs = hint.blocked_globs();
            let joined = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            check_generated_guard(&joined, cwd, &shown, &globs)?;
            check_generated_guard(&abs, cwd, &shown, &globs)?;
        }
        let (server, lang) = self.ensure_server_for_file(&abs, cwd).await?;
        let uri = server.sync_file(&abs, &lang).await?;
        let resp = server
            .client
            .request(
                "textDocument/formatting",
                json!({
                    "textDocument": { "uri": uri },
                    "options": { "tabSize": 4, "insertSpaces": true }
                }),
                timeout,
            )
            .await?;
        let edits = parse_text_edits(&resp);
        if edits.is_empty() {
            return Ok(format!("No formatting changes needed for {shown}."));
        }
        let source = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| HrnError::Tool(format!("read `{shown}`: {e}")))?;
        let formatted = apply_text_edits(&source, &edits).ok_or_else(|| {
            HrnError::Tool(format!("LSP returned invalid formatting edits for {shown}"))
        })?;
        write_atomic(&abs, formatted.as_bytes())
            .map_err(|e| HrnError::Tool(format!("write formatted `{shown}`: {e}")))?;
        if let Some(tracker) = &self.tracker {
            tracker.record_current(&abs);
        }
        let _ = server.sync_file(&abs, &lang).await;
        Ok(format!("Formatted {shown} ({} edit(s)).", edits.len()))
    }
}
