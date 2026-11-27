//! Lazy per-language / per-root LSP server pool with configurable timeouts, idle eviction,
//! document version tracking, formatting, and live `reload`.

use crate::config::{LspConfig, LspServerSpec};
use crate::format::{
    display_rel, format_diagnostics, format_hover, format_locations, format_symbols,
    format_workspace_edit,
};
use crate::lifecycle::{
    ManagedServer, deduplicate_diagnostics, find_binary, initialize_handshake, resolve_path,
    text_doc_pos,
};
use crate::rpc::{DiagnosticItem, LspClient};
use crate::servers::{cwd_has_root_marker, find_root, load_merged_servers, spec_for_path};
use crate::text_edit::{apply_text_edits, parse_text_edits};
use crate::uri::path_to_uri;
use hrn_edge_rt::tokio;
use hrn_edge_tools::{EditModeHint, FileReadTracker, check_generated_guard, write_atomic};
use hrn_kernel::serde_json::json;
use hrn_kernel::{HrnError, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Hook-relevant toggles snapshot read by [`crate::LspDiagnosticsHook`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct HookPolicy {
    pub(crate) diagnostics_on_write: bool,
    pub(crate) diagnostics_on_edit: bool,
    pub(crate) format_on_write: bool,
}

/// Manages lazily-spawned language servers and exposes semantic queries, formatting, reload,
/// idle eviction, and post-edit diagnostics.
pub struct LspManager {
    config: RwLock<LspConfig>,
    data_dir: Option<PathBuf>,
    pub(crate) tracker: Option<Arc<FileReadTracker>>,
    pub(crate) edit_hint: Option<Arc<EditModeHint>>,
    servers: Mutex<BTreeMap<(String, PathBuf), Arc<ManagedServer>>>,
}

impl LspManager {
    /// Construct a new manager with `config` (no processes are spawned until needed).
    pub fn new(config: LspConfig) -> Self {
        Self {
            config: RwLock::new(config),
            data_dir: None,
            tracker: None,
            edit_hint: None,
            servers: Mutex::new(BTreeMap::new()),
        }
    }

    /// Construct a manager bound to `data_dir` (for `lsp.toml` reloads) and shared `tracker`.
    pub fn with_context(
        config: LspConfig,
        data_dir: Option<PathBuf>,
        tracker: Option<Arc<FileReadTracker>>,
    ) -> Self {
        Self {
            config: RwLock::new(config),
            data_dir,
            tracker,
            edit_hint: None,
            servers: Mutex::new(BTreeMap::new()),
        }
    }

    /// Attach the shared [`EditModeHint`] so `format_file` enforces `[edit] block_auto_generated`.
    #[must_use]
    pub fn with_edit_hint(mut self, edit_hint: Arc<EditModeHint>) -> Self {
        self.edit_hint = Some(edit_hint);
        self
    }

    pub(crate) fn cfg(&self) -> LspConfig {
        self.config
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Snapshot hook-relevant toggles (`diagnostics_on_write`, `diagnostics_on_edit`, `format_on_write`).
    pub(crate) fn hook_policy(&self) -> HookPolicy {
        let cfg = self.cfg();
        HookPolicy {
            diagnostics_on_write: cfg.diagnostics_on_write,
            diagnostics_on_edit: cfg.diagnostics_on_edit,
            format_on_write: cfg.format_on_write,
        }
    }

    /// Register a pre-built client in tests so all queries and hooks run without external binaries.
    #[cfg(test)]
    pub(crate) async fn insert_test_server(&self, name: &str, root: PathBuf, client: LspClient) {
        let server = Arc::new(ManagedServer::new(name.to_owned(), root.clone(), client));
        self.servers
            .lock()
            .await
            .insert((name.to_owned(), root), server);
    }

    /// Backdate all active clients' `last_used` timestamp for deterministic idle-eviction tests.
    #[cfg(test)]
    pub(crate) async fn backdate_last_used_for_test(&self, age: Duration) {
        let active: Vec<Arc<ManagedServer>> = self.servers.lock().await.values().cloned().collect();
        let past = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        for srv in active {
            *srv.last_used.lock().await = past;
        }
    }

    /// Evict any warm LSP clients idle longer than `idle_timeout` (`Duration::ZERO` disables).
    pub async fn evict_idle(&self) -> usize {
        let idle = self.cfg().idle_timeout;
        if idle.is_zero() {
            return 0;
        }
        let now = Instant::now();
        let mut stale_keys = Vec::new();
        let mut stale_clients = Vec::new();
        {
            let map = self.servers.lock().await;
            for (k, srv) in map.iter() {
                let last = *srv.last_used.lock().await;
                if now.saturating_duration_since(last) >= idle {
                    stale_keys.push(k.clone());
                }
            }
        }
        if !stale_keys.is_empty() {
            let mut map = self.servers.lock().await;
            for key in stale_keys {
                if let Some(srv) = map.remove(&key) {
                    stale_clients.push(srv);
                }
            }
        }
        let count = stale_clients.len();
        for srv in stale_clients {
            srv.client.shutdown().await;
        }
        count
    }

    /// Re-read `<data_dir>/lsp.toml` and `<cwd>/.harness/lsp.toml` and shut down stale warm clients.
    pub async fn reload(&self, cwd: &Path) -> String {
        let loaded = load_merged_servers(self.data_dir.as_deref(), cwd);
        let stale: Vec<Arc<ManagedServer>> = {
            let mut map = self.servers.lock().await;
            let vals: Vec<_> = map.values().cloned().collect();
            map.clear();
            vals
        };
        for srv in &stale {
            srv.client.shutdown().await;
        }
        let enabled_count = loaded.servers.iter().filter(|s| !s.disabled).count();
        let disabled_count = loaded.servers.iter().filter(|s| s.disabled).count();
        let warnings = loaded.warnings.clone();
        {
            let mut guard = self.config.write().unwrap_or_else(PoisonError::into_inner);
            guard.servers = loaded.servers;
            if let Some(idle) = loaded.idle_timeout_override {
                guard.idle_timeout = idle;
            }
            guard.warnings = loaded.warnings;
        }
        let mut msg = format!(
            "Reloaded LSP config: {enabled_count} enabled server(s), {disabled_count} disabled; \
             shut down {} active client(s).",
            stale.len()
        );
        if !warnings.is_empty() {
            msg.push_str("\nWarnings:\n  - ");
            msg.push_str(&warnings.join("\n  - "));
        }
        msg
    }

    /// Query diagnostics for `path` (spawning the matching LSP if installed).
    pub async fn diagnostics(&self, path: &Path, cwd: &Path) -> Result<String> {
        let _ = self.evict_idle().await;
        let cfg = self.cfg();
        let abs = resolve_path(path, cwd)?;
        let (server, lang) = self.ensure_server_for_file(&abs, cwd).await?;
        let uri = path_to_uri(&abs);
        let prev_seq = server.client.diag_seq_for(&uri).await;
        let uri = server.sync_file(&abs, &lang).await?;
        let mut items = server
            .client
            .wait_diagnostics(&uri, prev_seq, cfg.tool_diag_wait)
            .await;
        if cfg.diagnostics_deduplicate {
            deduplicate_diagnostics(&mut items);
        }
        if items.is_empty() {
            Ok(format!("No diagnostics for {}.", display_rel(&abs, cwd)))
        } else {
            Ok(format_diagnostics(&abs, cwd, &items, 40)
                .trim_end()
                .to_owned())
        }
    }

    /// Go to definition at 1-based `(line, col)` in `path`.
    pub async fn definition(&self, path: &Path, line: u32, col: u32, cwd: &Path) -> Result<String> {
        let _ = self.evict_idle().await;
        let timeout = self.cfg().request_timeout;
        let abs = resolve_path(path, cwd)?;
        let (server, lang) = self.ensure_server_for_file(&abs, cwd).await?;
        let uri = server.sync_file(&abs, &lang).await?;
        let params = text_doc_pos(&uri, line, col);
        let val = server
            .client
            .request("textDocument/definition", params, timeout)
            .await?;
        Ok(format_locations(&val, cwd, 20))
    }

    /// Find references at 1-based `(line, col)` in `path`.
    pub async fn references(
        &self,
        path: &Path,
        line: u32,
        col: u32,
        include_declaration: bool,
        cwd: &Path,
    ) -> Result<String> {
        let _ = self.evict_idle().await;
        let timeout = self.cfg().request_timeout;
        let abs = resolve_path(path, cwd)?;
        let (server, lang) = self.ensure_server_for_file(&abs, cwd).await?;
        let uri = server.sync_file(&abs, &lang).await?;
        let mut params = text_doc_pos(&uri, line, col);
        if let Some(obj) = params.as_object_mut() {
            obj.insert(
                "context".to_owned(),
                json!({ "includeDeclaration": include_declaration }),
            );
        }
        let val = server
            .client
            .request("textDocument/references", params, timeout)
            .await?;
        Ok(format_locations(&val, cwd, 50))
    }

    /// Hover documentation / type signature at 1-based `(line, col)` in `path`.
    pub async fn hover(&self, path: &Path, line: u32, col: u32, cwd: &Path) -> Result<String> {
        let _ = self.evict_idle().await;
        let timeout = self.cfg().request_timeout;
        let abs = resolve_path(path, cwd)?;
        let (server, lang) = self.ensure_server_for_file(&abs, cwd).await?;
        let uri = server.sync_file(&abs, &lang).await?;
        let val = server
            .client
            .request("textDocument/hover", text_doc_pos(&uri, line, col), timeout)
            .await?;
        Ok(format_hover(&val))
    }

    /// Document symbols (when `path` is given) or workspace symbol search (when `query` is given).
    pub async fn symbols(
        &self,
        path: Option<&Path>,
        query: Option<&str>,
        cwd: &Path,
    ) -> Result<String> {
        let _ = self.evict_idle().await;
        let timeout = self.cfg().request_timeout;
        if let Some(p) = path {
            let abs = resolve_path(p, cwd)?;
            let (server, lang) = self.ensure_server_for_file(&abs, cwd).await?;
            let uri = server.sync_file(&abs, &lang).await?;
            let val = server
                .client
                .request(
                    "textDocument/documentSymbol",
                    json!({ "textDocument": { "uri": uri } }),
                    timeout,
                )
                .await?;
            return Ok(format_symbols(&val, cwd, 80));
        }
        let q = query.unwrap_or_default();
        let active: Vec<Arc<ManagedServer>> = self.servers.lock().await.values().cloned().collect();
        if active.is_empty() {
            return Err(HrnError::Tool(
                "no active LSP server for workspace symbol search; pass `path` first to warm a server"
                    .to_owned(),
            ));
        }
        let mut sections = Vec::new();
        for srv in active {
            srv.touch().await;
            if let Ok(val) = srv
                .client
                .request("workspace/symbol", json!({ "query": q }), timeout)
                .await
            {
                let text = format_symbols(&val, cwd, 40);
                if !text.starts_with("No symbols") {
                    sections.push(format!("[{}]\n{text}", srv.name));
                }
            }
        }
        if sections.is_empty() {
            Ok("No workspace symbols found.".to_owned())
        } else {
            Ok(sections.join("\n\n"))
        }
    }

    /// Compute a semantic rename plan (`WorkspaceEdit`) without mutating files on disk.
    pub async fn rename_plan(
        &self,
        path: &Path,
        line: u32,
        col: u32,
        new_name: &str,
        cwd: &Path,
    ) -> Result<String> {
        let _ = self.evict_idle().await;
        let timeout = self.cfg().request_timeout;
        let abs = resolve_path(path, cwd)?;
        let (server, lang) = self.ensure_server_for_file(&abs, cwd).await?;
        let uri = server.sync_file(&abs, &lang).await?;
        let mut params = text_doc_pos(&uri, line, col);
        if let Some(obj) = params.as_object_mut() {
            obj.insert("newName".to_owned(), json!(new_name));
        }
        let val = server
            .client
            .request("textDocument/rename", params, timeout)
            .await?;
        Ok(format_workspace_edit(&val, cwd))
    }

    /// Report active LSP servers and enabled configured/detected servers.
    pub async fn status(&self) -> String {
        let _ = self.evict_idle().await;
        let cfg = self.cfg();
        let active: Vec<Arc<ManagedServer>> = self.servers.lock().await.values().cloned().collect();
        let mut lines = Vec::new();
        if active.is_empty() {
            lines.push("Active LSP servers: none (lazy-started on first query).".to_owned());
        } else {
            lines.push("Active LSP servers:".to_owned());
            for srv in active {
                let state = if srv.client.is_closed() {
                    "disconnected"
                } else {
                    "running"
                };
                let diags = srv.client.total_diagnostics().await;
                lines.push(format!(
                    "  - {} [{}] root={} cached_diags={}",
                    srv.name,
                    state,
                    srv.root.display(),
                    diags
                ));
            }
        }
        let configured: Vec<String> = cfg
            .servers
            .iter()
            .filter(|s| !s.disabled)
            .map(|spec| {
                let bin = find_binary(&spec.commands)
                    .map_or_else(|| "not on PATH".to_owned(), |b| b.display().to_string());
                let scope = if spec.languages.is_empty() {
                    spec.file_types.join(", ")
                } else {
                    spec.languages.join(", ")
                };
                format!("  - {} ({scope}) -> {bin}", spec.name)
            })
            .collect();
        if configured.is_empty() {
            lines.push("Configured servers: none.".to_owned());
        } else {
            lines.push("Configured servers:".to_owned());
            lines.extend(configured);
        }
        lines.join("\n")
    }

    /// Request `textDocument/formatting` from a warm (or available) server and atomically apply
    /// returned edits on disk, updating the shared [`FileReadTracker`] so subsequent edits stay fresh.
    ///
    /// Enforces workspace confinement (`cwd`) and shared `[edit] block_auto_generated` globs.
    pub async fn format_file(&self, path: &Path, cwd: &Path) -> Option<String> {
        let cfg = self.cfg();
        let abs = resolve_path(path, cwd).ok()?;
        if let Some(hint) = &self.edit_hint {
            let globs = hint.blocked_globs();
            let joined = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            let shown = display_rel(&abs, cwd);
            if check_generated_guard(&joined, cwd, &shown, &globs).is_err()
                || check_generated_guard(&abs, cwd, &shown, &globs).is_err()
            {
                return None;
            }
        }
        let (spec, lang) = spec_for_path(&cfg.servers, &abs)?;
        let root = find_root(cwd, &spec.root_markers);
        let key = (spec.name.clone(), root);
        let server = match self.servers.lock().await.get(&key).cloned() {
            Some(srv) if !srv.client.is_closed() => srv,
            _ => self.ensure_server_for_file(&abs, cwd).await.ok()?.0,
        };
        let uri = server.sync_file(&abs, &lang).await.ok()?;
        let resp = server
            .client
            .request(
                "textDocument/formatting",
                json!({
                    "textDocument": { "uri": uri },
                    "options": { "tabSize": 4, "insertSpaces": true }
                }),
                cfg.request_timeout,
            )
            .await
            .ok()?;
        let edits = parse_text_edits(&resp);
        if edits.is_empty() {
            return None;
        }
        let source = tokio::fs::read_to_string(&abs).await.ok()?;
        let formatted = apply_text_edits(&source, &edits)?;
        write_atomic(&abs, formatted.as_bytes()).ok()?;
        if let Some(tracker) = &self.tracker {
            tracker.record_current(&abs);
        }
        let _ = server.sync_file(&abs, &lang).await;
        Some(display_rel(&abs, cwd))
    }

    /// Fast hook helper: notify warm servers of edited files and return compact error/warn text.
    pub async fn diagnostics_after_edit(
        self: &Arc<Self>,
        paths: &[PathBuf],
        cwd: &Path,
    ) -> Option<String> {
        let _ = self.evict_idle().await;
        let cfg = self.cfg();
        let mut combined = String::new();
        for raw in paths {
            let Ok(abs) = resolve_path(raw, cwd) else {
                continue;
            };
            let Some((spec, lang)) = spec_for_path(&cfg.servers, &abs) else {
                continue;
            };
            let root = find_root(cwd, &spec.root_markers);
            let key = (spec.name.clone(), root);
            let warm = self.servers.lock().await.get(&key).cloned();
            if let Some(server) = warm {
                if server.client.is_closed() {
                    continue;
                }
                let uri = path_to_uri(&abs);
                let prev = server.client.diag_seq_for(&uri).await;
                if let Ok(uri) = server.sync_file(&abs, &lang).await {
                    let mut items: Vec<DiagnosticItem> = server
                        .client
                        .wait_diagnostics(&uri, prev, cfg.hook_diag_wait)
                        .await
                        .into_iter()
                        .filter(|d| d.severity <= 2)
                        .collect();
                    if cfg.diagnostics_deduplicate {
                        deduplicate_diagnostics(&mut items);
                    }
                    let block = format_diagnostics(&abs, cwd, &items, 15);
                    if !block.is_empty() {
                        combined.push_str(&block);
                    }
                }
            } else if cfg.warm_on_edit
                && cwd_has_root_marker(cwd, &spec.root_markers)
                && find_binary(&spec.commands).is_some()
            {
                let mgr = Arc::clone(self);
                let abs_bg = abs.clone();
                let cwd_bg = cwd.to_path_buf();
                tokio::spawn(async move {
                    let _ = mgr.ensure_server_for_file(&abs_bg, &cwd_bg).await;
                });
            }
        }
        let trimmed = combined.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    /// Eagerly warm servers whose root markers exist in `cwd` (used when `[lsp] lazy = false`).
    pub(crate) fn warm_matching_in_background(self: &Arc<Self>, cwd: PathBuf) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let cfg = self.cfg();
        let specs: Vec<LspServerSpec> = cfg
            .servers
            .into_iter()
            .filter(|s| {
                !s.disabled
                    && cwd_has_root_marker(&cwd, &s.root_markers)
                    && find_binary(&s.commands).is_some()
            })
            .collect();
        if specs.is_empty() {
            return;
        }
        let mgr = Arc::clone(self);
        drop(handle.spawn(async move {
            for spec in specs {
                let root = find_root(&cwd, &spec.root_markers);
                let _ = mgr.spawn_spec(&spec, &root).await;
            }
        }));
    }

    pub(crate) async fn ensure_server_for_file(
        &self,
        abs: &Path,
        cwd: &Path,
    ) -> Result<(Arc<ManagedServer>, String)> {
        let cfg = self.cfg();
        let (spec, lang) = spec_for_path(&cfg.servers, abs).ok_or_else(|| {
            HrnError::Tool(format!(
                "no enabled LSP server configured for `{}`",
                abs.display()
            ))
        })?;
        let spec = spec.clone();
        let root = find_root(cwd, &spec.root_markers);
        let key = (spec.name.clone(), root.clone());
        if let Some(existing) = self.servers.lock().await.get(&key).cloned()
            && !existing.client.is_closed()
        {
            existing.touch().await;
            return Ok((existing, lang));
        }
        let server = self.spawn_spec(&spec, &root).await?;
        Ok((server, lang))
    }

    async fn spawn_spec(&self, spec: &LspServerSpec, root: &Path) -> Result<Arc<ManagedServer>> {
        let key = (spec.name.clone(), root.to_path_buf());
        if let Some(existing) = self.servers.lock().await.get(&key).cloned()
            && !existing.client.is_closed()
        {
            return Ok(existing);
        }
        let bin = find_binary(&spec.commands).ok_or_else(|| {
            HrnError::Tool(format!(
                "LSP server `{}` not found on PATH (tried: {}).",
                spec.name,
                spec.commands.join(", ")
            ))
        })?;
        let cfg = self.cfg();
        let init_timeout = spec.warmup_timeout_ms.map_or_else(
            || cfg.request_timeout.max(Duration::from_secs(15)),
            Duration::from_millis,
        );
        let program = bin.to_string_lossy().into_owned();
        let client =
            LspClient::spawn(&program, &spec.args, root, &spec.env, spec.settings.clone())?;
        initialize_handshake(&client, root, spec, init_timeout).await?;
        let server = Arc::new(ManagedServer::new(
            spec.name.clone(),
            root.to_path_buf(),
            client,
        ));
        self.servers.lock().await.insert(key, Arc::clone(&server));
        Ok(server)
    }
}
