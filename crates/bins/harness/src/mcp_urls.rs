//! `mcp://<uri>`: read-only internal URL scheme exposing resources advertised by configured MCP
//! servers.

use crate::url_adapters::block_on_safe;
use hrn_edge_mcp::{MAX_RESOURCE_OUTPUT_BYTES, McpResources, sanitize_mcp_error};
use hrn_kernel::{HrnError, Result, ToolContext, UrlBody, UrlScheme};
use std::fmt::Write as _;
use std::time::Duration;
use tokio::runtime::Handle;

/// Hard timeout for a single `mcp://` list or read call.
const MCP_URL_TIMEOUT: Duration = Duration::from_secs(30);

/// `mcp://` URL scheme: lists or reads resources advertised by the session's MCP servers.
pub(crate) struct McpScheme {
    resources: McpResources,
    handle: Option<Handle>,
}

impl McpScheme {
    pub(crate) fn new(resources: McpResources, handle: Option<Handle>) -> Self {
        Self { resources, handle }
    }
}

impl UrlScheme for McpScheme {
    fn scheme(&self) -> &'static str {
        "mcp"
    }

    fn about(&self) -> &'static str {
        "MCP server resources (read-only): `mcp://` lists advertised resources; `mcp://<uri>` reads one"
    }

    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        let trimmed = rest.trim();
        let cancel = ctx.cancel.clone();
        let resources = self.resources.clone();
        if trimmed.is_empty() || trimmed == "/" {
            let list = block_on_safe(self.handle.as_ref(), MCP_URL_TIMEOUT, async move {
                resources.list_all_resources(&cancel).await
            })
            .map_err(sanitize_err)?;
            if list.is_empty() {
                return Ok(UrlBody::Text(
                    "No MCP resources are advertised by configured servers.".to_owned(),
                ));
            }
            let mut out = String::new();
            for entry in list {
                let _ = write!(out, "mcp://{}  [{}]", entry.resource.uri, entry.server);
                if !entry.resource.name.is_empty() && entry.resource.name != entry.resource.uri {
                    let _ = write!(out, "  {}", entry.resource.name);
                }
                if let Some(desc) = entry.resource.description.as_deref()
                    && !desc.trim().is_empty()
                {
                    let _ = write!(out, "  — {}", desc.trim());
                }
                out.push('\n');
            }
            return Ok(UrlBody::Text(out));
        }

        let uri = trimmed.to_owned();
        let res = block_on_safe(self.handle.as_ref(), MCP_URL_TIMEOUT, async move {
            resources.read_resource(&uri, &cancel).await
        })
        .map_err(sanitize_err)?;
        Ok(UrlBody::Text(
            res.render_text_capped(MAX_RESOURCE_OUTPUT_BYTES),
        ))
    }

    fn write(&self, _ctx: &ToolContext, _rest: &str, _content: &str) -> Result {
        Err(HrnError::Tool("`mcp://` is read-only".to_owned()))
    }
}

fn sanitize_err(err: HrnError) -> HrnError {
    match err {
        HrnError::Cancelled => HrnError::Cancelled,
        HrnError::Tool(msg) => HrnError::Tool(sanitize_mcp_error(&msg, &[])),
        other => HrnError::Tool(sanitize_mcp_error(&other.to_string(), &[])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_edge_http::HttpClient;
    use hrn_edge_mcp::{
        CallToolResult, McpHandle, McpResource, McpServerConfig, McpServerHandler, McpToolSpec,
        McpTransport, ResourceContent, ResourcesReadResult, serve_stdio,
    };
    use hrn_kernel::{AgentEvent, BoxFuture, CancelFlag, EventSink, PermissionMode, UrlRouter};
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    struct NoopSink;
    impl EventSink for NoopSink {
        fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
            Box::pin(async { true })
        }
    }

    fn test_ctx(cwd: PathBuf) -> ToolContext {
        ToolContext {
            session_id: "test-sess".to_owned(),
            turn_id: "t1".to_owned(),
            call_id: "c1".to_owned(),
            cwd,
            permission_mode: PermissionMode::Default,
            cancel: CancelFlag::new(),
            events: Arc::new(NoopSink),
            ask: None,
            approval: None,
            urls: Arc::new(UrlRouter::new()),
            root_session_id: "test-sess".to_owned(),
            inbox: Arc::default(),
            caller: None,
            todos: Arc::default(),
            yield_data: Arc::default(),
            is_subagent: false,
        }
    }

    struct ResourceTestServer;

    impl McpServerHandler for ResourceTestServer {
        fn server_name(&self) -> &'static str {
            "test-resources"
        }

        fn list_tools(&self) -> Vec<McpToolSpec> {
            Vec::new()
        }

        fn call_tool(&self, name: &str, _arguments: Value) -> BoxFuture<Result<CallToolResult>> {
            let name = name.to_owned();
            Box::pin(async move { Ok(CallToolResult::error(format!("unknown tool `{name}`"))) })
        }

        fn list_resources(&self) -> Vec<McpResource> {
            vec![
                McpResource {
                    uri: "doc://readme".to_owned(),
                    name: "README".to_owned(),
                    description: Some("Project overview".to_owned()),
                    mime_type: Some("text/plain".to_owned()),
                },
                McpResource {
                    uri: "big://blob".to_owned(),
                    name: "Big Blob".to_owned(),
                    description: None,
                    mime_type: Some("text/plain".to_owned()),
                },
                McpResource {
                    uri: "secret://fail".to_owned(),
                    name: "Failing Resource".to_owned(),
                    description: None,
                    mime_type: None,
                },
            ]
        }

        fn read_resource(&self, uri: &str) -> BoxFuture<Result<ResourcesReadResult>> {
            let uri = uri.to_owned();
            Box::pin(async move {
                match uri.as_str() {
                    "doc://readme" => Ok(ResourcesReadResult {
                        contents: vec![ResourceContent {
                            uri,
                            mime_type: Some("text/plain".to_owned()),
                            text: Some("hello from advertised MCP resource".to_owned()),
                            blob: None,
                        }],
                    }),
                    "big://blob" => Ok(ResourcesReadResult {
                        contents: vec![ResourceContent {
                            uri,
                            mime_type: Some("text/plain".to_owned()),
                            text: Some("x".repeat(MAX_RESOURCE_OUTPUT_BYTES + 2048)),
                            blob: None,
                        }],
                    }),
                    "secret://fail" => Err(HrnError::Tool(
                        "upstream failed with Authorization: Bearer supersecret_tok_999999"
                            .to_owned(),
                    )),
                    other => Err(HrnError::Tool(format!("unknown resource `{other}`"))),
                }
            })
        }
    }

    #[tokio::test]
    async fn mcp_url_test_server_helper() {
        if std::env::var("HRN_MCP_URL_TEST_SERVER").is_err() {
            return;
        }
        serve_stdio(&ResourceTestServer)
            .await
            .expect("serve mcp resources");
    }

    fn test_server_config() -> McpServerConfig {
        McpServerConfig {
            name: "docs".to_owned(),
            transport: McpTransport::Stdio {
                command: std::env::current_exe()
                    .expect("current exe")
                    .to_string_lossy()
                    .into_owned(),
                args: vec![
                    "--exact".to_owned(),
                    "mcp_urls::tests::mcp_url_test_server_helper".to_owned(),
                    "--nocapture".to_owned(),
                ],
                cwd: None,
                env: BTreeMap::from([("HRN_MCP_URL_TEST_SERVER".to_owned(), "1".to_owned())]),
            },
            enabled: true,
            allow_tools: Vec::new(),
            deny_tools: Vec::new(),
            timeout_secs: 15,
            auto_approve: true,
            read_only: true,
            source: "test".to_owned(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_scheme_reads_advertised_resource() {
        let http = HttpClient::new().expect("http");
        let handle = Arc::new(McpHandle::lazy(test_server_config(), http));
        let (resources, binder) = McpResources::connecting();
        let bg_handle = Arc::clone(&handle);
        // Simulate a still-connecting MCP server finishing startup shortly after the read begins.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            binder.resolve(vec![bg_handle]);
        });

        let scheme = Arc::new(McpScheme::new(resources, Some(Handle::current())));
        let ctx = test_ctx(PathBuf::from("."));

        // Bare `mcp://` waits for startup and lists advertised resources.
        let s1 = Arc::clone(&scheme);
        let c1 = ctx.clone();
        let listed = tokio::task::spawn_blocking(move || s1.read(&c1, ""))
            .await
            .expect("join")
            .expect("list mcp://");
        let UrlBody::Text(list_text) = listed else {
            panic!("expected listing text");
        };
        assert!(list_text.contains("mcp://doc://readme"), "{list_text}");
        assert!(list_text.contains("[docs]"), "{list_text}");
        assert!(list_text.contains("Project overview"), "{list_text}");

        // Reading advertised resource returns its text.
        let s2 = Arc::clone(&scheme);
        let c2 = ctx.clone();
        let body = tokio::task::spawn_blocking(move || s2.read(&c2, "doc://readme"))
            .await
            .expect("join")
            .expect("read doc://readme");
        assert_eq!(
            body,
            UrlBody::Text("hello from advertised MCP resource".to_owned())
        );

        // Server-qualified `mcp://<server>/<uri>` also resolves cleanly.
        let s3 = Arc::clone(&scheme);
        let c3 = ctx.clone();
        let qual = tokio::task::spawn_blocking(move || s3.read(&c3, "docs/doc://readme"))
            .await
            .expect("join")
            .expect("read docs/doc://readme");
        assert_eq!(
            qual,
            UrlBody::Text("hello from advertised MCP resource".to_owned())
        );

        // Oversized resources are capped cleanly.
        let s4 = Arc::clone(&scheme);
        let c4 = ctx.clone();
        let capped = tokio::task::spawn_blocking(move || s4.read(&c4, "big://blob"))
            .await
            .expect("join")
            .expect("read big://blob");
        let UrlBody::Text(big_text) = capped else {
            panic!("expected text");
        };
        assert!(
            big_text.contains("truncated MCP resource output"),
            "{big_text}"
        );
        assert!(big_text.len() <= MAX_RESOURCE_OUTPUT_BYTES + 256);

        // Error messages sanitize auth tokens / secrets.
        let s5 = Arc::clone(&scheme);
        let c5 = ctx.clone();
        let err = tokio::task::spawn_blocking(move || s5.read(&c5, "secret://fail"))
            .await
            .expect("join")
            .expect_err("secret://fail errors")
            .to_string();
        assert!(
            !err.contains("supersecret_tok_999999"),
            "leaked secret: {err}"
        );
        assert!(err.contains("<redacted>"), "{err}");

        // `mcp://` is strictly read-only.
        let write_err = scheme
            .write(&ctx, "doc://readme", "nope")
            .expect_err("read-only")
            .to_string();
        assert!(write_err.contains("read-only"), "{write_err}");

        handle.shutdown().await;
    }
}
