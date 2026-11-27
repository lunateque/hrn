//! Cached, race-free MCP resource discovery and reading across connected servers.
//!
//! [`McpResources`] holds the live [`McpHandle`]s for a session, caches the advertised resource
//! catalog, and waits for servers that are still connecting before answering `mcp://` reads.

use crate::client::{McpHandle, config_secrets};
use crate::protocol::{McpResource, ResourcesReadResult, sanitize_mcp_error};
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinSet;

/// Maximum time to wait for servers still finishing initial connection.
const CONNECT_WAIT_BUDGET: Duration = Duration::from_secs(20);

/// One resource advertised by a configured MCP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvertisedResource {
    /// Configured MCP server name.
    pub server: String,
    /// Resource metadata returned by `resources/list`.
    pub resource: McpResource,
}

struct McpResourcesInner {
    handles: RwLock<Vec<Arc<McpHandle>>>,
    cache: RwLock<Option<Vec<AdvertisedResource>>>,
    ready: watch::Receiver<bool>,
}

/// Resolves a pending [`McpResources`] created with [`McpResources::connecting`] once startup
/// finishes.
pub struct McpResourcesBinder {
    inner: Arc<McpResourcesInner>,
    ready_tx: watch::Sender<bool>,
}

impl McpResourcesBinder {
    /// Publish the connected/lazy server handles and wake any waiting `mcp://` callers.
    pub fn resolve(self, mut handles: Vec<Arc<McpHandle>>) {
        handles.sort_by(|a, b| a.config().name.cmp(&b.config().name));
        *self
            .inner
            .handles
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = handles;
        *self
            .inner
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let _ = self.ready_tx.send(true);
    }
}

/// Shared, cheaply cloneable view of MCP resources across a session's servers.
#[derive(Clone)]
pub struct McpResources {
    inner: Arc<McpResourcesInner>,
}

impl Default for McpResources {
    fn default() -> Self {
        Self::empty()
    }
}

impl McpResources {
    /// An empty resource registry (no configured servers, already ready).
    #[must_use]
    pub fn empty() -> Self {
        Self::from_handles(Vec::new())
    }

    /// Build a ready resource registry over `handles`.
    #[must_use]
    pub fn from_handles(mut handles: Vec<Arc<McpHandle>>) -> Self {
        handles.sort_by(|a, b| a.config().name.cmp(&b.config().name));
        let (_tx, ready) = watch::channel(true);
        Self {
            inner: Arc::new(McpResourcesInner {
                handles: RwLock::new(handles),
                cache: RwLock::new(None),
                ready,
            }),
        }
    }

    /// Build a pending registry whose callers wait until [`McpResourcesBinder::resolve`] runs.
    #[must_use]
    pub fn connecting() -> (Self, McpResourcesBinder) {
        let (ready_tx, ready) = watch::channel(false);
        let inner = Arc::new(McpResourcesInner {
            handles: RwLock::new(Vec::new()),
            cache: RwLock::new(None),
            ready,
        });
        let binder = McpResourcesBinder {
            inner: Arc::clone(&inner),
            ready_tx,
        };
        (Self { inner }, binder)
    }

    /// Replace the server handles in place and clear the cached resource list.
    pub fn replace_handles(&self, mut handles: Vec<Arc<McpHandle>>) {
        handles.sort_by(|a, b| a.config().name.cmp(&b.config().name));
        *self
            .inner
            .handles
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = handles;
        *self
            .inner
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Whether any server handle is currently registered (or still connecting).
    #[must_use]
    pub fn has_servers(&self) -> bool {
        if !*self.inner.ready.borrow() {
            return true;
        }
        !self
            .inner
            .handles
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    fn snapshot_handles(&self) -> Vec<Arc<McpHandle>> {
        self.inner
            .handles
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn cached(&self) -> Option<Vec<AdvertisedResource>> {
        self.inner
            .cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Wait for any in-flight startup connection before reading or listing resources.
    async fn wait_ready(&self, cancel: &CancelFlag) -> Result {
        if *self.inner.ready.borrow() {
            return Ok(());
        }
        let mut rx = self.inner.ready.clone();
        let deadline = tokio::time::Instant::now() + CONNECT_WAIT_BUDGET;
        loop {
            if *rx.borrow() {
                return Ok(());
            }
            if cancel.is_cancelled() {
                return Err(HrnError::Cancelled);
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(HrnError::Cancelled),
                changed = rx.changed() => {
                    if changed.is_err() || *rx.borrow() {
                        return Ok(());
                    }
                }
                () = tokio::time::sleep_until(deadline) => {
                    return Err(HrnError::Tool(format!(
                        "timed out after {}s waiting for MCP servers to connect",
                        CONNECT_WAIT_BUDGET.as_secs()
                    )));
                }
            }
        }
    }

    /// Return all advertised resources across configured servers (cached after the first query).
    pub async fn list_all_resources(&self, cancel: &CancelFlag) -> Result<Vec<AdvertisedResource>> {
        self.wait_ready(cancel).await?;
        if let Some(cached) = self.cached() {
            return Ok(cached);
        }
        self.refresh_resources(cancel).await
    }

    /// Query every registered server for its current `resources/list` and update the cache.
    pub async fn refresh_resources(&self, cancel: &CancelFlag) -> Result<Vec<AdvertisedResource>> {
        self.wait_ready(cancel).await?;
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        let handles = self.snapshot_handles();
        if handles.is_empty() {
            *self
                .inner
                .cache
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Vec::new());
            return Ok(Vec::new());
        }
        let mut set = JoinSet::new();
        for handle in handles {
            set.spawn(async move { fetch_server_resources(handle).await });
        }
        let mut entries = Vec::new();
        let mut successes = 0usize;
        let mut first_error: Option<String> = None;
        while let Some(joined) = set.join_next().await {
            if cancel.is_cancelled() {
                set.abort_all();
                return Err(HrnError::Cancelled);
            }
            match joined {
                Ok(Ok((server, list))) => {
                    successes += 1;
                    for resource in list {
                        entries.push(AdvertisedResource {
                            server: server.clone(),
                            resource,
                        });
                    }
                }
                Ok(Err(err)) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
                Err(join_err) => {
                    if first_error.is_none() {
                        first_error = Some(format!("MCP resource discovery failed: {join_err}"));
                    }
                }
            }
        }
        if successes == 0
            && let Some(err) = first_error
        {
            return Err(HrnError::Tool(err));
        }
        entries.sort_by(|a, b| {
            a.server
                .cmp(&b.server)
                .then_with(|| a.resource.uri.cmp(&b.resource.uri))
        });
        *self
            .inner
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(entries.clone());
        Ok(entries)
    }

    /// Read one resource by `uri` (either an exact advertised URI or `<server>/<uri>`).
    pub async fn read_resource(
        &self,
        uri: &str,
        cancel: &CancelFlag,
    ) -> Result<ResourcesReadResult> {
        let trimmed = uri.trim();
        if trimmed.is_empty() || trimmed.chars().any(|c| c == '\0' || c.is_control()) {
            return Err(HrnError::Tool(
                "invalid MCP resource URI: empty or contains control characters".to_owned(),
            ));
        }
        self.wait_ready(cancel).await?;
        let handles = self.snapshot_handles();
        if handles.is_empty() {
            return Err(HrnError::Tool(
                "no MCP servers are configured or connected".to_owned(),
            ));
        }

        let (server_name, target_uri) = self.resolve_target(trimmed, &handles, cancel).await?;
        let handle = handles
            .iter()
            .find(|h| h.config().name == server_name)
            .ok_or_else(|| {
                HrnError::Tool(format!("MCP server `{server_name}` is not connected"))
            })?;
        let secrets = config_secrets(handle.config());
        let client = handle
            .client()
            .await
            .map_err(|err| HrnError::Tool(sanitize_mcp_error(&err.to_string(), &secrets)))?;
        client.read_resource(&target_uri, cancel).await
    }

    async fn resolve_target(
        &self,
        uri: &str,
        handles: &[Arc<McpHandle>],
        cancel: &CancelFlag,
    ) -> Result<(String, String)> {
        let entries = self.list_all_resources(cancel).await?;
        if let Some(found) = match_advertised(&entries, uri) {
            return Ok(found);
        }
        // Cache may predate a newly-added resource; refresh once before giving up.
        let refreshed = self.refresh_resources(cancel).await?;
        if let Some(found) = match_advertised(&refreshed, uri) {
            return Ok(found);
        }
        // Explicit `<server>/<uri>` allows reading dynamic template resources directly.
        if let Some((server, inner)) = uri.split_once('/')
            && !inner.is_empty()
            && handles.iter().any(|h| h.config().name == server)
        {
            return Ok((server.to_owned(), inner.to_owned()));
        }
        Err(HrnError::Tool(format!(
            "MCP resource `{uri}` not found; read `mcp://` to list advertised resources"
        )))
    }
}

fn match_advertised(entries: &[AdvertisedResource], uri: &str) -> Option<(String, String)> {
    if let Some(entry) = entries.iter().find(|e| e.resource.uri == uri) {
        return Some((entry.server.clone(), entry.resource.uri.clone()));
    }
    if let Some((server, inner)) = uri.split_once('/')
        && let Some(entry) = entries
            .iter()
            .find(|e| e.server == server && e.resource.uri == inner)
    {
        return Some((entry.server.clone(), entry.resource.uri.clone()));
    }
    None
}

async fn fetch_server_resources(
    handle: Arc<McpHandle>,
) -> std::result::Result<(String, Vec<McpResource>), String> {
    let name = handle.config().name.clone();
    let secrets = config_secrets(handle.config());
    let timeout = Duration::from_secs(handle.config().timeout_secs.max(1));
    let fut = async {
        let client = handle.client().await?;
        client.list_resources().await
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(list)) => Ok((name, list)),
        Ok(Err(err)) => Err(sanitize_mcp_error(&err.to_string(), &secrets)),
        Err(_) => Err(format!(
            "MCP `{name}`: resources/list timed out after {}s",
            timeout.as_secs()
        )),
    }
}
