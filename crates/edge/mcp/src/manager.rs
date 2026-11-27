//! All MCP servers of a session: tools come from the on-disk cache when it knows the server, and
//! a server process only starts on the first call that needs it.

use crate::cache::{Lookup, McpCache};
use crate::client::{McpClient, McpHandle, McpToolAdapter};
use crate::config::McpServerConfig;
use crate::protocol::{McpToolSpec, ResourcesReadResult};
use crate::resources::{AdvertisedResource, McpResources};
use crate::toolset::{McpToolSet, ServerTools};
use hrn_edge_http::HttpClient;
use hrn_kernel::{CancelFlag, Result, Tool};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;

/// How long a server without cached tools may take to answer at startup before it is given up
/// on (and recorded as failed).
const STARTUP_BUDGET: Duration = Duration::from_secs(20);

/// Startup record for one configured MCP server.
#[derive(Clone, Debug)]
pub struct McpServerStatus {
    /// Configured server name.
    pub name: String,
    /// Config origin (`config.toml`, `.mcp.json`, etc.).
    pub source: String,
    /// `true` when a process was started and answered at startup. `false` with no `error` means
    /// the tools came from the cache and the process starts on first use.
    pub connected: bool,
    /// Number of tools exposed after filtering.
    pub tool_count: usize,
    /// Error message when degraded/offline (or `"disabled"`).
    pub error: Option<String>,
}

/// Manages all MCP servers for a harness session.
#[derive(Default)]
pub struct McpManager {
    handles: Vec<Arc<McpHandle>>,
    servers: Vec<ServerTools>,
    statuses: Vec<McpServerStatus>,
    cache_warnings: Vec<String>,
    resources: McpResources,
}

enum Outcome {
    Listed(Box<McpClient>, Vec<McpToolSpec>),
    Failed(Box<McpServerConfig>, String),
}

async fn probe(cfg: McpServerConfig, http: HttpClient) -> Outcome {
    let attempt = async {
        let client = McpClient::connect(cfg.clone(), http).await?;
        match client.list_tools().await {
            Ok(specs) => Ok((client, specs)),
            Err(err) => {
                client.shutdown().await;
                Err(err)
            }
        }
    };
    match tokio::time::timeout(STARTUP_BUDGET, attempt).await {
        Ok(Ok((client, specs))) => Outcome::Listed(Box::new(client), specs),
        Ok(Err(err)) => Outcome::Failed(Box::new(cfg), err.to_string()),
        Err(_) => Outcome::Failed(
            Box::new(cfg),
            format!("no answer within {}s", STARTUP_BUDGET.as_secs()),
        ),
    }
}

impl McpManager {
    /// Set up all enabled servers. With a `cache`, servers whose tool list is cached start no
    /// process; the others are connected concurrently (bounded by a startup budget) and the
    /// outcome is cached. Without a cache every enabled server is connected.
    pub async fn open(
        configs: Vec<McpServerConfig>,
        http: HttpClient,
        cache: Option<&McpCache>,
    ) -> Self {
        let mut manager = Self::default();
        let snapshot = match cache.map(McpCache::snapshot).transpose() {
            Ok(snapshot) => snapshot,
            Err(err) => {
                manager.cache_warnings.push(err.to_string());
                None
            }
        };
        let mut set = JoinSet::new();
        for cfg in configs {
            if !cfg.enabled {
                manager.push_status(&cfg, false, 0, Some("disabled".to_owned()));
                continue;
            }
            match snapshot.as_ref().map_or(Lookup::Miss, |s| s.lookup(&cfg)) {
                Lookup::Tools(specs) => {
                    manager.push_status(&cfg, false, specs.len(), None);
                    manager.add_tools(&Arc::new(McpHandle::lazy(cfg, http.clone())), specs);
                }
                Lookup::Failed {
                    error,
                    retry_in_secs,
                } => {
                    let note = format!(
                        "{error} (not retried for {}m; `hrn mcp ping {}` retries now)",
                        retry_in_secs.div_ceil(60),
                        cfg.name
                    );
                    manager.push_status(&cfg, false, 0, Some(note));
                }
                Lookup::Miss => {
                    set.spawn(probe(cfg, http.clone()));
                }
            }
        }
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Outcome::Listed(client, specs)) => {
                    let handle = Arc::new(McpHandle::connected(*client, http.clone()));
                    manager.push_status(handle.config(), true, specs.len(), None);
                    if let Some(cache) = cache
                        && let Err(err) = cache.record_tools(handle.config(), specs.clone())
                    {
                        manager.cache_warnings.push(err.to_string());
                    }
                    manager.add_tools(&handle, specs);
                }
                Ok(Outcome::Failed(cfg, error)) => {
                    if let Some(cache) = cache
                        && let Err(err) = cache.record_failure(&cfg, error.clone())
                    {
                        manager.cache_warnings.push(err.to_string());
                    }
                    manager.push_status(&cfg, false, 0, Some(error));
                }
                Err(join_err) => manager.statuses.push(McpServerStatus {
                    name: "unknown".to_owned(),
                    source: String::new(),
                    connected: false,
                    tool_count: 0,
                    error: Some(format!("MCP connect task panicked: {join_err}")),
                }),
            }
        }
        manager.statuses.sort_by(|a, b| a.name.cmp(&b.name));
        manager.servers.sort_by(|a, b| a.name.cmp(&b.name));
        manager
            .handles
            .sort_by(|a, b| a.config().name.cmp(&b.config().name));
        manager.resources = McpResources::from_handles(manager.handles.clone());
        manager
    }

    fn push_status(
        &mut self,
        cfg: &McpServerConfig,
        connected: bool,
        tool_count: usize,
        error: Option<String>,
    ) {
        self.statuses.push(McpServerStatus {
            name: cfg.name.clone(),
            source: cfg.source.clone(),
            connected,
            tool_count,
            error,
        });
    }

    fn add_tools(&mut self, handle: &Arc<McpHandle>, specs: Vec<McpToolSpec>) {
        let tools = specs
            .into_iter()
            .map(|spec| -> Arc<dyn Tool> {
                Arc::new(McpToolAdapter::new(Arc::clone(handle), spec))
            })
            .collect();
        self.servers.push(ServerTools {
            name: handle.config().name.clone(),
            tools,
        });
        self.handles.push(Arc::clone(handle));
    }

    /// The harness-ready [`Tool`] adapters, split by schema size: servers whose schemas fit in
    /// `budget_tokens` (smallest first) are loaded, the rest are deferred behind `tool_search`.
    pub fn tool_set(&self, budget_tokens: usize) -> McpToolSet {
        McpToolSet::split(&self.servers, budget_tokens)
    }

    /// Per-server startup status records.
    pub fn statuses(&self) -> &[McpServerStatus] {
        &self.statuses
    }

    /// Non-fatal startup messages: servers that failed to connect and cache problems.
    pub fn warnings(&self) -> Vec<String> {
        self.statuses
            .iter()
            .filter(|s| !s.connected && s.error.as_deref() != Some("disabled"))
            .filter_map(|s| {
                s.error
                    .as_ref()
                    .map(|e| format!("MCP `{}` ({}) unavailable: {e}", s.name, s.source))
            })
            .chain(self.cache_warnings.iter().cloned())
            .collect()
    }

    /// Shared resource view for `mcp://` URL reads and `/mcp resources`.
    pub fn resources(&self) -> McpResources {
        self.resources.clone()
    }

    /// List every resource advertised across all connected/configured servers.
    pub async fn list_all_resources(&self, cancel: &CancelFlag) -> Result<Vec<AdvertisedResource>> {
        self.resources.list_all_resources(cancel).await
    }

    /// Read a single resource by URI across the session's MCP servers.
    pub async fn read_resource(
        &self,
        uri: &str,
        cancel: &CancelFlag,
    ) -> Result<ResourcesReadResult> {
        self.resources.read_resource(uri, cancel).await
    }

    /// Cleanly terminate every server process that was started.
    pub async fn shutdown(&self) {
        for handle in &self.handles {
            handle.shutdown().await;
        }
    }
}
