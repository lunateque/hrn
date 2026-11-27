//! Managed LSP server process helpers: document synchronization, initialize handshake, binary
//! resolution, path normalization, and diagnostic deduplication.

use crate::config::LspServerSpec;
use crate::rpc::{DiagnosticItem, LspClient};
use crate::uri::path_to_uri;
use hrn_edge_proc::resolve_on_path;
use hrn_edge_rt::tokio;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{HrnError, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub(crate) struct ManagedServer {
    pub(crate) name: String,
    pub(crate) root: PathBuf,
    pub(crate) client: LspClient,
    doc_versions: Mutex<BTreeMap<String, i32>>,
    pub(crate) last_used: Mutex<Instant>,
}

impl ManagedServer {
    pub(crate) fn new(name: String, root: PathBuf, client: LspClient) -> Self {
        Self {
            name,
            root,
            client,
            doc_versions: Mutex::new(BTreeMap::new()),
            last_used: Mutex::new(Instant::now()),
        }
    }

    pub(crate) async fn touch(&self) {
        *self.last_used.lock().await = Instant::now();
    }

    pub(crate) async fn sync_file(&self, path: &Path, lang_id: &str) -> Result<String> {
        self.touch().await;
        let text = tokio::fs::read_to_string(path).await.map_err(|e| {
            HrnError::Tool(format!("failed to read `{}` for LSP: {e}", path.display()))
        })?;
        let uri = path_to_uri(path);
        let mut versions = self.doc_versions.lock().await;
        if let Some(ver) = versions.get_mut(&uri) {
            *ver += 1;
            let next = *ver;
            drop(versions);
            self.client
                .notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": next },
                        "contentChanges": [{ "text": text }],
                    }),
                )
                .await?;
            self.client
                .notify(
                    "textDocument/didSave",
                    json!({
                        "textDocument": { "uri": uri },
                        "text": text,
                    }),
                )
                .await?;
        } else {
            versions.insert(uri.clone(), 1);
            drop(versions);
            self.client
                .notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": lang_id,
                            "version": 1,
                            "text": text,
                        }
                    }),
                )
                .await?;
        }
        Ok(uri)
    }
}

pub(crate) fn deduplicate_diagnostics(items: &mut Vec<DiagnosticItem>) {
    let mut seen = HashSet::new();
    items.retain(|d| seen.insert((d.line, d.col, d.severity, d.code.clone(), d.message.clone())));
}

pub(crate) async fn initialize_handshake(
    client: &LspClient,
    root: &Path,
    spec: &LspServerSpec,
    timeout: Duration,
) -> Result<()> {
    let root_uri = path_to_uri(root);
    let root_name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    let mut params = json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "workspaceFolders": [{ "uri": root_uri, "name": root_name }],
        "capabilities": {
            "textDocument": {
                "synchronization": { "didSave": true },
                "publishDiagnostics": { "relatedInformation": false },
                "hover": { "contentFormat": ["markdown", "plaintext"] },
                "definition": { "linkSupport": true },
                "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                "formatting": {},
                "rename": { "prepareSupport": false }
            },
            "workspace": {
                "symbol": {},
                "configuration": true,
                "workspaceEdit": { "documentChanges": true }
            }
        }
    });
    if let Some(init_opts) = &spec.init_options
        && let Some(obj) = params.as_object_mut()
    {
        obj.insert("initializationOptions".to_owned(), init_opts.clone());
    }
    let _ = client.request("initialize", params, timeout).await?;
    client.notify("initialized", json!({})).await?;
    if let Some(settings) = &spec.settings {
        let _ = client
            .notify(
                "workspace/didChangeConfiguration",
                json!({ "settings": settings }),
            )
            .await;
    }
    Ok(())
}

pub(crate) fn find_binary(candidates: &[String]) -> Option<PathBuf> {
    for cand in candidates {
        let p = Path::new(cand);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
        if let Some(found) = resolve_on_path(cand) {
            return Some(found);
        }
    }
    None
}

fn canonicalize_or_ancestor(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    while let Some(name) = cur.file_name() {
        tail.push(name.to_os_string());
        let Some(parent) = cur.parent() else {
            break;
        };
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Ok(mut canon) = std::fs::canonicalize(parent) {
            for seg in tail.into_iter().rev() {
                canon.push(seg);
            }
            return canon;
        }
        cur = parent;
    }
    path.to_path_buf()
}

fn normalized_components(path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::Prefix(p) => {
                out.push(p.as_os_str().to_string_lossy().replace('\\', "/"));
            }
            std::path::Component::RootDir => out.push("/".to_owned()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if matches!(out.last().map(String::as_str), Some(last) if last != "/" && !last.ends_with(':'))
                {
                    out.pop();
                }
            }
            std::path::Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
        }
    }
    out
}

/// Whether `path` (after canonicalizing existing ancestors) resides within `cwd`.
pub(crate) fn is_path_within_workspace(path: &Path, cwd: &Path) -> bool {
    let base = normalized_components(&canonicalize_or_ancestor(cwd));
    let target = normalized_components(&canonicalize_or_ancestor(path));
    if target.len() < base.len() {
        return false;
    }
    base.iter().zip(target.iter()).all(|(b, t)| {
        if cfg!(windows) {
            b.eq_ignore_ascii_case(t)
        } else {
            b == t
        }
    })
}

/// Resolve `path` against `cwd` and refuse any target outside workspace bounds (`cwd`).
pub(crate) fn resolve_path(path: &Path, cwd: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let resolved = canonicalize_or_ancestor(&joined);
    if !is_path_within_workspace(&resolved, cwd) {
        return Err(HrnError::Tool(format!(
            "refused LSP access outside workspace `{}`: `{}`",
            cwd.display(),
            path.display()
        )));
    }
    Ok(resolved)
}

pub(crate) fn text_doc_pos(uri: &str, line: u32, col: u32) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": {
            "line": line.saturating_sub(1),
            "character": col.saturating_sub(1),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_confines_to_workspace() {
        let root = std::env::temp_dir().join(format!("hrn-lsp-confine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).expect("ws");
        let inside = ws.join("main.rs");
        let outside = root.join("secret.rs");
        std::fs::write(&inside, "fn main() {}\n").expect("inside");
        std::fs::write(&outside, "const SECRET: &str = \"x\";\n").expect("outside");

        assert!(resolve_path(Path::new("main.rs"), &ws).is_ok());
        assert!(resolve_path(Path::new("../secret.rs"), &ws).is_err());
        assert!(resolve_path(&outside, &ws).is_err());
        assert!(!is_path_within_workspace(&outside, &ws));
        assert!(is_path_within_workspace(&inside, &ws));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn format_file_refuses_outside_workspace_and_generated_files() {
        use crate::config::{LspConfig, LspTableConfig, builtin_server_specs};
        use crate::pool::LspManager;
        use hrn_edge_tools::EditModeHint;
        use std::sync::Arc;

        let root = std::env::temp_dir().join(format!("hrn-lsp-fmt-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).expect("ws");
        let generated = ws.join("api.pb.rs");
        let outside = root.join("outside.rs");
        std::fs::write(&generated, "fn gen(){}\n").expect("gen");
        std::fs::write(&outside, "fn out(){}\n").expect("out");

        let hint = Arc::new(EditModeHint::new());
        hint.set_blocked_globs(vec!["*.pb.rs".to_owned()]);
        let cfg = LspConfig::from_table(&LspTableConfig::default(), builtin_server_specs(), vec![]);
        let mgr = LspManager::new(cfg).with_edit_hint(hint);

        assert!(
            mgr.format_file(Path::new("../outside.rs"), &ws)
                .await
                .is_none()
        );
        assert!(mgr.format_file(&outside, &ws).await.is_none());
        assert!(mgr.format_file(Path::new("api.pb.rs"), &ws).await.is_none());
        assert!(
            mgr.diagnostics(Path::new("../outside.rs"), &ws)
                .await
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
