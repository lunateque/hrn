//! High-level MCP client, lazy per-server handle, and `hrn_kernel::Tool` adapter.

use crate::config::{McpServerConfig, McpTransport};
use crate::http::HttpMcpTransport;
use crate::protocol::{
    CallToolResult, InitializeResult, McpResource, McpToolSpec, ResourcesListResult,
    ResourcesReadResult, ToolsListResult, default_object_schema, sanitize_mcp_error,
    sanitize_tool_segment, tool_prefix,
};
use crate::stdio::StdioMcpTransport;
use hrn_edge_http::HttpClient;
use hrn_kernel::{
    BoxFuture, CancelFlag, HrnError, Result, Tool, ToolApproval, ToolContext, ToolEffect,
    ToolOutput, ToolSpec,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;

/// Cap pagination loops on `tools/list` so a buggy server cannot spin forever.
const MAX_LIST_PAGES: usize = 16;
/// Cap rendered MCP tool text sent back into LLM context.
const MAX_TOOL_OUTPUT_CHARS: usize = 64 * 1024;

enum Transport {
    Stdio(StdioMcpTransport),
    Http(HttpMcpTransport),
}

/// Active connection to one MCP server (stdio or Streamable HTTP).
pub struct McpClient {
    config: McpServerConfig,
    init: InitializeResult,
    transport: Transport,
}

impl McpClient {
    /// Connect to `config` and complete the MCP `initialize` handshake.
    pub async fn connect(config: McpServerConfig, http: HttpClient) -> Result<Self> {
        let timeout = Duration::from_secs(config.timeout_secs.max(1));
        let (transport, init) = match &config.transport {
            McpTransport::Stdio {
                command,
                args,
                cwd,
                env,
            } => {
                let (t, init) = StdioMcpTransport::connect(
                    config.name.clone(),
                    command.clone(),
                    args.clone(),
                    cwd.clone(),
                    env.clone(),
                    timeout,
                )
                .await?;
                (Transport::Stdio(t), init)
            }
            McpTransport::Http {
                url,
                headers,
                header_envs,
            } => {
                let (t, init) = HttpMcpTransport::connect(
                    config.name.clone(),
                    url.clone(),
                    headers.clone(),
                    header_envs.clone(),
                    timeout,
                    http,
                )
                .await?;
                (Transport::Http(t), init)
            }
        };
        Ok(Self {
            config,
            init,
            transport,
        })
    }

    /// Server configuration.
    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    /// Server `initialize` metadata.
    pub fn init_result(&self) -> &InitializeResult {
        &self.init
    }

    async fn rpc(&self, method: &str, params: Option<Value>, cancel: &CancelFlag) -> Result<Value> {
        match &self.transport {
            Transport::Stdio(t) => t.request(method, params, cancel).await,
            Transport::Http(t) => t.request(method, params, cancel).await,
        }
    }

    /// Send MCP `ping` and measure round-trip latency (falls back to `tools/list` if unsupported).
    pub async fn ping(&self) -> Result<Duration> {
        let start = Instant::now();
        let cancel = CancelFlag::new();
        match self.rpc("ping", None, &cancel).await {
            Ok(_) => Ok(start.elapsed()),
            Err(HrnError::Tool(msg)) if msg.contains("-32601") => {
                let _ = self.list_tools().await?;
                Ok(start.elapsed())
            }
            Err(err) => Err(err),
        }
    }

    /// List all tools offered by this server (applying `allow_tools` / `deny_tools` filters).
    pub async fn list_tools(&self) -> Result<Vec<McpToolSpec>> {
        let cancel = CancelFlag::new();
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let raw = self.rpc("tools/list", params, &cancel).await?;
            let page: ToolsListResult = serde_json::from_value(raw).map_err(|e| {
                HrnError::Tool(format!(
                    "MCP `{}`: invalid tools/list response: {e}",
                    self.config.name
                ))
            })?;
            for spec in page.tools {
                if self.config.allows_tool(&spec.name) {
                    out.push(spec);
                }
            }
            match page.next_cursor.filter(|c| !c.is_empty()) {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(out)
    }

    /// Invoke a remote tool by its unprefixed server tool name.
    pub async fn call_tool(
        &self,
        remote_name: &str,
        arguments: Value,
        cancel: &CancelFlag,
    ) -> Result<CallToolResult> {
        let args = if arguments.is_object() {
            arguments
        } else {
            json!({})
        };
        let params = json!({
            "name": remote_name,
            "arguments": args,
        });
        let raw = self.rpc("tools/call", Some(params), cancel).await?;
        serde_json::from_value(raw).map_err(|e| {
            HrnError::Tool(format!(
                "MCP `{}`: invalid tools/call response for `{remote_name}`: {e}",
                self.config.name
            ))
        })
    }

    /// List all resources advertised by this server (`resources/list`, bounded by pagination cap).
    /// Returns `Ok(vec![])` when the server does not implement `resources/list` (`-32601`).
    pub async fn list_resources(&self) -> Result<Vec<McpResource>> {
        let cancel = CancelFlag::new();
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let raw = match self.rpc("resources/list", params, &cancel).await {
                Ok(v) => v,
                Err(HrnError::Tool(msg)) if msg.contains("-32601") => return Ok(Vec::new()),
                Err(err) => {
                    let secrets = config_secrets(&self.config);
                    return Err(HrnError::Tool(sanitize_mcp_error(
                        &err.to_string(),
                        &secrets,
                    )));
                }
            };
            let page: ResourcesListResult = serde_json::from_value(raw).map_err(|e| {
                HrnError::Tool(format!(
                    "MCP `{}`: invalid resources/list response: {e}",
                    self.config.name
                ))
            })?;
            out.extend(page.resources);
            match page.next_cursor.filter(|c| !c.is_empty()) {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(out)
    }

    /// Read one resource by URI via MCP `resources/read`.
    pub async fn read_resource(
        &self,
        uri: &str,
        cancel: &CancelFlag,
    ) -> Result<ResourcesReadResult> {
        if uri.is_empty() || uri.chars().any(|c| c == '\0' || c.is_control()) {
            return Err(HrnError::Tool(
                "invalid MCP resource URI: empty or contains control characters".to_owned(),
            ));
        }
        let params = json!({ "uri": uri });
        let raw = self
            .rpc("resources/read", Some(params), cancel)
            .await
            .map_err(|err| match err {
                HrnError::Cancelled => HrnError::Cancelled,
                other => {
                    let secrets = config_secrets(&self.config);
                    HrnError::Tool(sanitize_mcp_error(&other.to_string(), &secrets))
                }
            })?;
        serde_json::from_value(raw).map_err(|e| {
            HrnError::Tool(format!(
                "MCP `{}`: invalid resources/read response for `{uri}`: {e}",
                self.config.name
            ))
        })
    }

    /// Shut down any underlying child process cleanly.
    pub async fn shutdown(&self) {
        if let Transport::Stdio(t) = &self.transport {
            t.shutdown().await;
        }
    }
}

/// Adapts one remote MCP tool to the harness [`Tool`] trait as `mcp__<server>__<tool>`.
pub struct McpToolAdapter {
    handle: Arc<McpHandle>,
    qualified_name: String,
    remote_name: String,
    description: String,
    input_schema: Value,
    effect: ToolEffect,
    approval: ToolApproval,
}

impl McpToolAdapter {
    /// Wrap `spec` from the server behind `handle` into a harness [`Tool`].
    pub fn new(handle: Arc<McpHandle>, spec: McpToolSpec) -> Self {
        let config = handle.config();
        let tool_seg = sanitize_tool_segment(&spec.name);
        let qualified_name = format!("{}{tool_seg}", tool_prefix(&config.name));
        let desc = spec.description.unwrap_or_default();
        let description = if desc.trim().is_empty() {
            format!("[mcp:{}] {}", config.name, spec.name)
        } else {
            format!("[mcp:{}] {}", config.name, desc.trim())
        };
        let input_schema = if spec.input_schema.is_object() {
            spec.input_schema
        } else {
            default_object_schema()
        };
        let effect = if config.read_only {
            ToolEffect::ReadOnly
        } else {
            ToolEffect::Mutating
        };
        let approval = if config.auto_approve {
            ToolApproval::read()
        } else {
            ToolApproval::exec()
        };
        Self {
            handle,
            qualified_name,
            remote_name: spec.name,
            description,
            input_schema,
            effect,
            approval,
        }
    }
}

impl Tool for McpToolAdapter {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.qualified_name.clone(),
            description: self.description.clone(),
            parameters: self.input_schema.clone(),
        }
    }

    fn effect(&self) -> ToolEffect {
        self.effect
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        self.approval.clone()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let handle = Arc::clone(&self.handle);
        let remote_name = self.remote_name.clone();
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            let res = handle
                .client()
                .await?
                .call_tool(&remote_name, args, &cancel)
                .await?;
            let rendered = cap_output(res.render_text());
            if res.is_error {
                Ok(ToolOutput::err(rendered))
            } else {
                Ok(ToolOutput::ok(rendered))
            }
        })
    }
}

fn cap_output(text: String) -> String {
    if text.len() <= MAX_TOOL_OUTPUT_CHARS {
        return text;
    }
    let cut = text.floor_char_boundary(MAX_TOOL_OUTPUT_CHARS);
    format!(
        "{}\n... [truncated MCP output, {} bytes total]",
        text.get(..cut).unwrap_or_default(),
        text.len()
    )
}

/// One configured server whose process starts on the first call that needs it, so listing
/// tools from the cache never spawns anything.
pub struct McpHandle {
    config: McpServerConfig,
    http: HttpClient,
    client: OnceCell<McpClient>,
}

impl McpHandle {
    /// A handle that connects on first use.
    pub fn lazy(config: McpServerConfig, http: HttpClient) -> Self {
        Self {
            config,
            http,
            client: OnceCell::new(),
        }
    }

    /// A handle over an already connected `client`.
    pub(crate) fn connected(client: McpClient, http: HttpClient) -> Self {
        Self {
            config: client.config.clone(),
            http,
            client: OnceCell::new_with(Some(client)),
        }
    }

    /// Server configuration.
    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    /// The live client, connecting first when nobody has yet. Concurrent first callers share
    /// one connection attempt; a failed attempt is retried by the next call.
    pub async fn client(&self) -> Result<&McpClient> {
        self.client
            .get_or_try_init(|| McpClient::connect(self.config.clone(), self.http.clone()))
            .await
    }

    /// Stop the server process if one was ever started.
    pub async fn shutdown(&self) {
        if let Some(client) = self.client.get() {
            client.shutdown().await;
        }
    }
}

/// Collect configured HTTP header secret values for error redaction.
pub(crate) fn config_secrets(cfg: &McpServerConfig) -> Vec<String> {
    let mut secrets = Vec::new();
    if let McpTransport::Http {
        headers,
        header_envs,
        ..
    } = &cfg.transport
    {
        for val in headers.values() {
            if !val.is_empty() {
                secrets.push(val.clone());
            }
        }
        for env_name in header_envs.values() {
            if let Ok(val) = std::env::var(env_name)
                && !val.is_empty()
            {
                secrets.push(val);
            }
        }
    }
    secrets
}
