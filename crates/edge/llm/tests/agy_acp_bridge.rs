//! Integration tests for the loopback Streamable HTTP MCP bridge (`McpBridge`).

use bytes::Bytes;
use hrn_edge_llm::agy_acp::bridge::{
    BRIDGE_TOKEN_HEADER, BridgeToolResult, McpBridge, McpBridgeOptions,
};
use hrn_edge_llm::agy_acp::schema::{
    MAX_BRIDGE_WIRE_NAME_LEN, sanitize_tool_schema, wire_tool_name,
};
use hrn_kernel::ToolSpec;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use std::error::Error;
use std::time::Duration;

type TestResult<T = ()> = std::result::Result<T, Box<dyn Error>>;

async fn send_http(
    method: Method,
    url: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> TestResult<(StatusCode, String, String)> {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut builder = Request::builder().method(method).uri(url);
    if let Some(tok) = token {
        builder = builder.header(BRIDGE_TOKEN_HEADER, tok);
    }
    let payload = if let Some(val) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Bytes::from(serde_json::to_vec(val)?)
    } else {
        Bytes::new()
    };
    let req = builder.body(Full::new(payload))?;
    let resp = client.request(req).await?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let collected = resp.into_body().collect().await?.to_bytes();
    let text = String::from_utf8(collected.to_vec())?;
    Ok((status, content_type, text))
}

#[tokio::test]
async fn bridge_security_and_mcp_discovery() -> TestResult {
    let bridge = McpBridge::start(McpBridgeOptions::default()).await?;
    assert!(bridge.url().starts_with("http://127.0.0.1:"));
    assert_eq!(bridge.token().len(), 64);

    let empty = json!({});
    let bad_path = bridge.url().replace("/mcp", "/other");
    let (st, _, _) = send_http(Method::POST, &bad_path, Some(bridge.token()), Some(&empty)).await?;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, _, _) = send_http(Method::POST, &bridge.url(), None, Some(&empty)).await?;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _, _) = send_http(Method::POST, &bridge.url(), Some("wrong"), Some(&empty)).await?;
    assert_eq!(st, StatusCode::FORBIDDEN);

    let (st, _, _) = send_http(Method::GET, &bridge.url(), Some(bridge.token()), None).await?;
    assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED);

    let init_req = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    let (st, _, body) = send_http(
        Method::POST,
        &bridge.url(),
        Some(bridge.token()),
        Some(&init_req),
    )
    .await?;
    assert_eq!(st, StatusCode::OK);
    let init: Value = serde_json::from_str(&body)?;
    assert_eq!(
        init.pointer("/result/serverInfo/name")
            .and_then(Value::as_str),
        Some("hrn-bridge")
    );

    let notif = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let (st, _, _) = send_http(
        Method::POST,
        &bridge.url(),
        Some(bridge.token()),
        Some(&notif),
    )
    .await?;
    assert_eq!(st, StatusCode::ACCEPTED);

    bridge.set_tools(vec![
        ToolSpec {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            parameters: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }),
        },
        ToolSpec {
            name: "ping".to_owned(),
            description: "Zero-arg tool".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
    ]);
    let list_req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
    let (st, _, body) = send_http(
        Method::POST,
        &bridge.url(),
        Some(bridge.token()),
        Some(&list_req),
    )
    .await?;
    assert_eq!(st, StatusCode::OK);
    let list: Value = serde_json::from_str(&body)?;
    assert_eq!(
        list.pointer("/result/tools/0/name").and_then(Value::as_str),
        Some("hrn__read")
    );
    assert!(
        list.pointer("/result/tools/0/inputSchema/$schema")
            .is_none()
    );
    assert!(
        list.pointer("/result/tools/0/inputSchema/properties/type")
            .is_none()
    );
    assert_eq!(
        list.pointer("/result/tools/1/inputSchema/properties"),
        Some(&json!({}))
    );
    Ok(())
}

#[tokio::test]
async fn bridge_tool_call_suspend_and_sse_progress_keepalive() -> TestResult {
    let bridge = McpBridge::start(McpBridgeOptions {
        progress_interval: Duration::from_millis(35),
        call_timeout: Duration::from_secs(5),
    })
    .await?;
    let mut rx = bridge.attach_call_sink();

    // 1. Plain JSON tools/call without progressToken.
    let url = bridge.url();
    let token = bridge.token().to_owned();
    let plain_task = tokio::spawn(async move {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": { "name": "hrn__read", "arguments": { "path": "Cargo.toml" } }
        });
        send_http(Method::POST, &url, Some(&token), Some(&req))
            .await
            .map_err(|e| e.to_string())
    });

    let call = rx.recv().await.ok_or("expected bridged ToolCall")?;
    assert_eq!(call.name, "read");
    assert!(call.arguments.contains("Cargo.toml"));
    assert!(bridge.resolve_call(
        &call.id,
        BridgeToolResult {
            content: "file-body".to_owned(),
            is_error: false,
        }
    ));

    let (st, ct, body) = plain_task.await??;
    assert_eq!(st, StatusCode::OK);
    assert!(ct.contains("application/json"));
    let parsed: Value = serde_json::from_str(&body)?;
    assert_eq!(
        parsed
            .pointer("/result/content/0/text")
            .and_then(Value::as_str),
        Some("file-body")
    );
    assert_eq!(
        parsed.pointer("/result/isError").and_then(Value::as_bool),
        Some(false)
    );

    // 2. Long-running tools/call WITH _meta.progressToken -> streams SSE notifications/progress!
    let url2 = bridge.url();
    let token2 = bridge.token().to_owned();
    let sse_task = tokio::spawn(async move {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "hrn__bash",
                "arguments": { "cmd": "cargo test" },
                "_meta": { "progressToken": "tok-42" }
            }
        });
        send_http(Method::POST, &url2, Some(&token2), Some(&req))
            .await
            .map_err(|e| e.to_string())
    });

    let call2 = rx.recv().await.ok_or("expected second ToolCall")?;
    assert_eq!(call2.name, "bash");
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(bridge.resolve_call(
        &call2.id,
        BridgeToolResult {
            content: "all tests passed".to_owned(),
            is_error: false,
        }
    ));

    let (st2, ct2, sse_body) = sse_task.await??;
    assert_eq!(st2, StatusCode::OK);
    assert!(ct2.contains("text/event-stream"));
    let progress_count = sse_body.matches("\"notifications/progress\"").count();
    assert!(
        progress_count >= 2,
        "expected >= 2 progress notifications in SSE body, got {progress_count}: {sse_body}"
    );
    assert!(sse_body.contains("\"tok-42\""));
    assert!(sse_body.contains("all tests passed"));
    Ok(())
}

#[tokio::test]
async fn bridge_fail_closed_on_cancel_and_timeout() -> TestResult {
    let bridge = McpBridge::start(McpBridgeOptions {
        progress_interval: Duration::from_secs(10),
        call_timeout: Duration::from_millis(90),
    })
    .await?;
    let mut rx = bridge.attach_call_sink();

    let url = bridge.url();
    let token = bridge.token().to_owned();
    let cancel_task = tokio::spawn(async move {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/call",
            "params": { "name": "hrn__bash", "arguments": {} }
        });
        send_http(Method::POST, &url, Some(&token), Some(&req))
            .await
            .map_err(|e| e.to_string())
    });
    let _call = rx.recv().await.ok_or("expected call for cancel test")?;
    bridge.fail_all_pending("Cancelled by user");
    let (_, _, body) = cancel_task.await??;
    let parsed: Value = serde_json::from_str(&body)?;
    assert_eq!(
        parsed.pointer("/result/isError").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        parsed
            .pointer("/result/content/0/text")
            .and_then(Value::as_str),
        Some("Cancelled by user")
    );

    let url2 = bridge.url();
    let token2 = bridge.token().to_owned();
    let timeout_task = tokio::spawn(async move {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": { "name": "hrn__slow", "arguments": {} }
        });
        send_http(Method::POST, &url2, Some(&token2), Some(&req))
            .await
            .map_err(|e| e.to_string())
    });
    let _call2 = rx.recv().await.ok_or("expected call for timeout test")?;
    let (_, _, body2) = timeout_task.await??;
    let parsed2: Value = serde_json::from_str(&body2)?;
    assert_eq!(
        parsed2.pointer("/result/isError").and_then(Value::as_bool),
        Some(true)
    );
    let err_txt = parsed2
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(err_txt.contains("timed out"));
    Ok(())
}

#[test]
fn schema_sanitizer_fills_missing_array_items_and_collapses_unions() {
    let raw = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "dates": { "type": "array", "description": "Specific dates" },
            "dateRanges": { "type": "array", "description": "Date ranges" },
            "includedCategories": { "type": "array" },
            "excludedCategories": { "type": ["array", "null"] },
            "pageSize": { "type": "number", "default": 25, "exclusiveMinimum": 0 },
            "seconds": {
                "description": "Slow mode delay",
                "anyOf": [
                    { "const": 0, "type": "number" },
                    { "const": 10, "type": "number" },
                    { "const": 30, "type": "number" }
                ]
            },
            "confirm": { "const": true },
            "reactions": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "type": { "const": "all" },
                            "allowCustom": { "type": "boolean" }
                        },
                        "required": ["type"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "const": "some" },
                            "items": { "type": "array" }
                        },
                        "required": ["type", "items"]
                    }
                ]
            }
        },
        "required": ["dates", "ghost_field"]
    });
    let clean = sanitize_tool_schema(&raw);
    assert!(clean.get("$schema").is_none());
    assert!(clean.get("additionalProperties").is_none());
    for field in [
        "dates",
        "dateRanges",
        "includedCategories",
        "excludedCategories",
    ] {
        assert_eq!(
            clean.pointer(&format!("/properties/{field}/items")),
            Some(&json!({ "type": "string" })),
            "array field `{field}` must have `items`"
        );
    }
    assert_eq!(
        clean.pointer("/properties/excludedCategories/nullable"),
        Some(&json!(true))
    );
    assert!(clean.pointer("/properties/pageSize/default").is_none());
    assert!(
        clean
            .pointer("/properties/pageSize/exclusiveMinimum")
            .is_none()
    );
    assert_eq!(
        clean.pointer("/properties/seconds/type"),
        Some(&json!("integer"))
    );
    assert!(clean.pointer("/properties/seconds/anyOf").is_none());
    assert!(clean.pointer("/properties/seconds/enum").is_none());
    assert_eq!(
        clean.pointer("/properties/confirm/type"),
        Some(&json!("boolean"))
    );
    assert!(clean.pointer("/properties/confirm/const").is_none());
    assert_eq!(
        clean.pointer("/properties/reactions/type"),
        Some(&json!("object"))
    );
    assert_eq!(
        clean.pointer("/properties/reactions/properties/type/enum"),
        Some(&json!(["all", "some"]))
    );
    assert_eq!(
        clean.pointer("/properties/reactions/properties/items/items"),
        Some(&json!({ "type": "string" }))
    );
    assert_eq!(
        clean.pointer("/properties/reactions/required"),
        Some(&json!(["type"]))
    );
    assert_eq!(clean.pointer("/required"), Some(&json!(["dates"])));
}

#[test]
fn wire_tool_name_clamps_long_mcp_names_to_agy_64_char_limit() {
    assert_eq!(wire_tool_name("read"), "hrn__read");
    assert_eq!(wire_tool_name("hrn__bash"), "hrn__bash");

    let long_a = "mcp__google-photos__describe_filter_capabilities";
    let long_b = "mcp__google-photos__search_photos_by_location";
    let wire_a = wire_tool_name(long_a);
    let wire_b = wire_tool_name(long_b);
    assert!(wire_a.starts_with("hrn__"));
    assert!(wire_b.starts_with("hrn__"));
    assert!(wire_a.len() <= MAX_BRIDGE_WIRE_NAME_LEN);
    assert!(wire_b.len() <= MAX_BRIDGE_WIRE_NAME_LEN);
    assert_ne!(wire_a, wire_b);
    assert!(format!("mcp_hrn-bridge_{wire_a}").len() <= 64);
    assert!(
        wire_a
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    );
}

#[tokio::test]
async fn bridge_sanitizes_schemas_and_maps_clamped_long_tool_names() -> TestResult {
    let bridge = McpBridge::start(McpBridgeOptions::default()).await?;
    let real_name = "mcp__google-photos__describe_filter_capabilities";
    bridge.set_tools(vec![ToolSpec {
        name: real_name.to_owned(),
        description: "Describe Google Photos filter options".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "dates": { "type": "array" },
                "dateRanges": { "type": "array" }
            }
        }),
    }]);

    let list_req = json!({ "jsonrpc": "2.0", "id": 30, "method": "tools/list", "params": {} });
    let (st, _, body) = send_http(
        Method::POST,
        &bridge.url(),
        Some(bridge.token()),
        Some(&list_req),
    )
    .await?;
    assert_eq!(st, StatusCode::OK);
    let list: Value = serde_json::from_str(&body)?;
    let wire = list
        .pointer("/result/tools/0/name")
        .and_then(Value::as_str)
        .ok_or("missing wire tool name")?
        .to_owned();
    assert!(wire.starts_with("hrn__"));
    assert!(format!("mcp_hrn-bridge_{wire}").len() <= 64);
    assert_eq!(
        list.pointer("/result/tools/0/inputSchema/properties/dates/items"),
        Some(&json!({ "type": "string" }))
    );
    assert_eq!(
        list.pointer("/result/tools/0/inputSchema/properties/dateRanges/items"),
        Some(&json!({ "type": "string" }))
    );

    // Calling the clamped wire name must dispatch back to the original long `hrn` tool name.
    let mut rx = bridge.attach_call_sink();
    let url = bridge.url();
    let token = bridge.token().to_owned();
    let wire_for_call = wire.clone();
    let call_task = tokio::spawn(async move {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 31,
            "method": "tools/call",
            "params": { "name": wire_for_call, "arguments": { "dates": ["2026-01-01"] } }
        });
        send_http(Method::POST, &url, Some(&token), Some(&req))
            .await
            .map_err(|e| e.to_string())
    });

    let bridged = rx.recv().await.ok_or("expected bridged call")?;
    assert_eq!(bridged.name, real_name);
    assert!(bridge.resolve_call(
        &bridged.id,
        BridgeToolResult {
            content: "ok-filters".to_owned(),
            is_error: false,
        }
    ));
    let (st2, _, body2) = call_task.await??;
    assert_eq!(st2, StatusCode::OK);
    let parsed: Value = serde_json::from_str(&body2)?;
    assert_eq!(
        parsed
            .pointer("/result/content/0/text")
            .and_then(Value::as_str),
        Some("ok-filters")
    );
    Ok(())
}
