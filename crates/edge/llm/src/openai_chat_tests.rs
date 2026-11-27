use super::*;
use crate::common::testutil::VecSink;
use hrn_kernel::ToolCall;

async fn run(events: &[&str]) -> (Result<LlmResponse>, VecSink) {
    let sink = VecSink::default();
    let mut asm = ChatAssembler::new("glm".into());
    for e in events {
        if let Err(err) = asm.handle_data(e, &sink).await {
            return (Err(err), sink);
        }
        if asm.done {
            break;
        }
    }
    (asm.finish(), sink)
}

#[tokio::test]
async fn text_interleaved_tool_calls_and_trailing_usage_chunk() {
    let (resp, sink) = run(&[
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"hm"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"Let me "}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"check."}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"grep","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"pattern\":\"x\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4}}}"#,
        "[DONE]",
    ])
    .await;
    let resp = resp.expect("response");
    assert_eq!(resp.content, "Let me check.");
    assert_eq!(sink.reasoning(), "hm");
    assert_eq!(resp.tool_calls.len(), 2);
    assert_eq!(
        resp.tool_calls[0].name, "read_file",
        "name must not be appended per chunk"
    );
    assert_eq!(resp.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
    assert_eq!(resp.tool_calls[1].arguments, r#"{"pattern":"x"}"#);
    assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
    let usage = resp.usage.expect("usage");
    assert_eq!(
        (usage.prompt_tokens, usage.cache_read_tokens),
        (10, Some(4))
    );
}

#[tokio::test]
async fn gateway_reusing_index_zero_for_every_call() {
    let (resp, _) = run(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"a","arguments":"{}"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"b","arguments":"{}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ])
    .await;
    let resp = resp.expect("response");
    let names: Vec<&str> = resp.tool_calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
}

#[tokio::test]
async fn eof_without_finish_reason_or_done_is_truncation() {
    let (resp, _) = run(&[r#"{"choices":[{"delta":{"content":"par"}}]}"#]).await;
    assert!(resp.expect_err("truncated").is_retryable());
}

#[tokio::test]
async fn error_payloads_string_and_object_are_classified() {
    let (r, _) = run(&[r#"{"error":"boom"}"#]).await;
    assert!(!r.expect_err("e").is_retryable());
    let (r, _) = run(&[r#"{"error":{"message":"slow down","type":"rate_limit_exceeded"}}"#]).await;
    assert!(r.expect_err("e").is_retryable());
    let (r, _) = run(&[r#"{"error":{"message":"busy","code":503}}"#]).await;
    assert!(r.expect_err("e").is_retryable());
    let (r, _) = run(&[r#"{"choices":[{"delta":"#]).await;
    assert!(!r.expect_err("malformed").is_retryable());
}

#[test]
fn encodes_user_and_tool_images() {
    let img = ImageAttachment {
        media_type: "image/jpeg".into(),
        data_base64: "BBBB".into(),
        label: None,
        width: None,
        height: None,
    };
    let mut tool = Msg::tool_result("c1", "read_file", "Image: a.jpg");
    tool.images.push(img.clone());
    let req = LlmRequest {
        model: None,
        messages: vec![Msg::user_with_images("hi", vec![img]), tool],
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        thinking_budgets: None,
        temperature: None,
        max_tokens: None,
        session_id: None,
        service_tier: None,
    };
    let body =
        encode_request("gpt-4o", &req, false, &ModelPreset::default(), false).expect("encode");
    let msgs = body["messages"].as_array().expect("messages");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["content"][1]["type"], "image_url");
    assert_eq!(
        msgs[0]["content"][1]["image_url"]["url"],
        "data:image/jpeg;base64,BBBB"
    );
    assert_eq!(msgs[1]["role"], "tool");
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"][1]["type"], "image_url");
}

#[test]
fn openrouter_headers_and_no_max_tokens() {
    let prov = ProviderEntry {
        base_url: Some("https://openrouter.ai/api/v1".into()),
        ..ProviderEntry::default()
    };
    let preset = ModelPreset {
        provider: "openrouter".into(),
        id: "anthropic/claude-sonnet-4.6:nitro".into(),
        thinking_format: Some("openrouter".into()),
        reasoning: Some(true),
        ..ModelPreset::default()
    };
    let headers = build_headers(&prov, &preset, "sk-or", Some("ses_1"));
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "X-OpenRouter-Title" && v == "hrn"),
        "{headers:?}"
    );
    assert!(
        headers.iter().any(|(k, _)| k == "HTTP-Referer"),
        "{headers:?}"
    );

    let req = LlmRequest {
        model: None,
        messages: vec![Msg::system("sys prompt"), Msg::user("hi")],
        tools: Vec::new(),
        thinking: ThinkingLevel::High,
        thinking_budgets: None,
        temperature: None,
        max_tokens: None,
        session_id: Some("ses_1".into()),
        service_tier: None,
    };
    let body = encode_request(
        "anthropic/claude-sonnet-4.6:nitro",
        &req,
        false,
        &preset,
        true,
    )
    .expect("encode");
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["model"], "anthropic/claude-sonnet-4.6:nitro");
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
}

#[test]
fn moonshot_replays_reasoning_content_when_opted_in() {
    let asst = Msg::assistant_tool_calls(
        String::new(),
        Some("thinking trace".into()),
        vec![ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
            signature: None,
        }],
    );
    let req = LlmRequest {
        model: None,
        messages: vec![
            Msg::user("check a.rs"),
            asst,
            Msg::tool_result("c1", "read", "ok"),
        ],
        tools: Vec::new(),
        thinking: ThinkingLevel::Medium,
        thinking_budgets: None,
        temperature: None,
        max_tokens: None,
        session_id: None,
        service_tier: None,
    };

    let opted_in = ModelPreset {
        replay_reasoning_content: true,
        thinking_format: Some("zai".into()),
        ..ModelPreset::default()
    };
    let body_on = encode_request("kimi-k2.7-code", &req, false, &opted_in, false).expect("on");
    assert_eq!(
        body_on["messages"][1]["reasoning_content"],
        "thinking trace"
    );
    assert_eq!(body_on["thinking"]["type"], "enabled");

    let opted_out = ModelPreset::default();
    let body_off = encode_request("gpt-4o", &req, false, &opted_out, false).expect("off");
    assert!(body_off["messages"][1].get("reasoning_content").is_none());
}

#[test]
fn xai_effort_map_clamps() {
    let mut effort_map = BTreeMap::new();
    effort_map.insert(ThinkingLevel::Minimal, "low".into());
    effort_map.insert(ThinkingLevel::Xhigh, "high".into());
    effort_map.insert(ThinkingLevel::Max, "high".into());
    let preset = ModelPreset {
        provider: "xai".into(),
        id: "grok-4.6".into(),
        reasoning: Some(true),
        reasoning_effort_map: effort_map,
        prompt_cache_session_header: Some("x-grok-conv-id".into()),
        ..ModelPreset::default()
    };
    let prov = ProviderEntry {
        base_url: Some("https://api.x.ai/v1".into()),
        ..ProviderEntry::default()
    };
    let headers = build_headers(&prov, &preset, "sk-xai", Some("sess_42"));
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "x-grok-conv-id" && v == "sess_42"),
        "{headers:?}"
    );

    for (input, expected) in [
        (ThinkingLevel::Minimal, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::Xhigh, "high"),
        (ThinkingLevel::Max, "high"),
    ] {
        let req = LlmRequest {
            model: None,
            messages: vec![Msg::user("hi")],
            tools: Vec::new(),
            thinking: input,
            thinking_budgets: None,
            temperature: None,
            max_tokens: None,
            session_id: Some("sess_42".into()),
            service_tier: None,
        };
        let body = encode_request("grok-4.6", &req, false, &preset, false).expect("encode");
        assert_eq!(body["reasoning_effort"], expected, "for {input:?}");
    }
}

#[test]
fn rejected_reasoning_effort_remaps_to_nearest_or_strips() {
    let mut payload = json!({"model": "qwen3.8-flash-next", "reasoning_effort": "high"});
    let err = r#"{"error":{"message":"Unexpected reasoning effort high. Supported types are xhigh (default), medium, and low.","type":"BadRequestError","param":null,"code":400}}"#;
    assert!(adjust_rejected_reasoning_effort(
        &mut payload,
        ThinkingLevel::High,
        err
    ));
    assert_eq!(payload["reasoning_effort"], "xhigh");

    let mut payload_drop = json!({"model": "legacy", "reasoning_effort": "medium"});
    assert!(adjust_rejected_reasoning_effort(
        &mut payload_drop,
        ThinkingLevel::Medium,
        r#"{"error":{"message":"Unknown parameter: reasoning_effort is not supported"}}"#
    ));
    assert!(payload_drop.get("reasoning_effort").is_none());

    let mut unrelated = json!({"model": "m", "reasoning_effort": "high"});
    assert!(!adjust_rejected_reasoning_effort(
        &mut unrelated,
        ThinkingLevel::High,
        r#"{"error":{"message":"context length exceeded"}}"#
    ));
    assert_eq!(unrelated["reasoning_effort"], "high");
}

#[tokio::test]
async fn stream_retries_once_on_http_400_rejected_reasoning_effort() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let seen_srv = std::sync::Arc::clone(&seen);

    let srv = tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.expect("read");
            let req_str = String::from_utf8_lossy(&buf[..n]);
            let body_str = req_str.split("\r\n\r\n").nth(1).unwrap_or("");
            let parsed: Value = serde_json::from_str(body_str).expect("json body");
            seen_srv.lock().expect("lock").push(parsed);
            if attempt == 0 {
                let err_body = r#"{"error":{"message":"Unexpected reasoning effort high. Supported types are xhigh (default), medium, and low.","type":"BadRequestError","param":null,"code":400}}"#;
                let resp = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{err_body}",
                    err_body.len()
                );
                sock.write_all(resp.as_bytes()).await.expect("write 400");
            } else {
                let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\n\
                           data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                           data: [DONE]\n\n";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                    sse.len()
                );
                sock.write_all(resp.as_bytes()).await.expect("write 200");
            }
        }
    });

    let http = HttpClient::new().expect("http");
    let provider = ProviderEntry {
        base_url: Some(format!("http://{addr}/v1")),
        ..ProviderEntry::default()
    };
    let preset = ModelPreset {
        provider: "local-gw".into(),
        id: "example-flash".into(),
        reasoning: Some(true),
        stream_idle_timeout_ms: Some(600_000),
        ..ModelPreset::default()
    };
    let req = LlmRequest {
        model: None,
        messages: vec![Msg::user("hi")],
        tools: Vec::new(),
        thinking: ThinkingLevel::High,
        thinking_budgets: None,
        temperature: None,
        max_tokens: None,
        session_id: None,
        service_tier: None,
    };
    let sink = VecSink::default();
    let resp = stream(
        &http,
        &provider,
        &preset,
        "example-flash",
        "",
        &req,
        &CancelFlag::new(),
        &sink,
    )
    .await
    .expect("retry succeeds");
    srv.await.expect("srv join");
    assert_eq!(resp.content, "recovered");
    let calls = seen.lock().expect("lock");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["reasoning_effort"], "high");
    assert_eq!(calls[1]["reasoning_effort"], "xhigh");
}

#[test]
fn proactive_effort_clamping_maps_high_to_xhigh_before_request() {
    let supported = [
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::Xhigh,
    ];
    assert_eq!(
        clamp_effort_to_supported(ThinkingLevel::High, &supported),
        Some("xhigh")
    );
    assert_eq!(
        clamp_effort_to_supported(ThinkingLevel::Minimal, &supported),
        Some("low")
    );
    assert_eq!(
        clamp_effort_to_supported(ThinkingLevel::Medium, &supported),
        Some("medium")
    );
    assert_eq!(
        clamp_effort_to_supported(ThinkingLevel::Off, &supported),
        None
    );
    assert_eq!(
        clamp_effort_to_supported(ThinkingLevel::Auto, &supported),
        None
    );

    let preset = ModelPreset {
        provider: "local-gw".into(),
        id: "example-flash".into(),
        reasoning: Some(true),
        thinking_budgets: Some(hrn_kernel::ThinkingBudgets {
            efforts: Some(supported.to_vec()),
            default_level: Some(ThinkingLevel::Xhigh),
            effort_budgets: BTreeMap::new(),
        }),
        ..ModelPreset::default()
    };
    let req = LlmRequest {
        model: None,
        messages: vec![Msg::user("hi")],
        tools: Vec::new(),
        thinking: ThinkingLevel::High,
        thinking_budgets: None,
        temperature: None,
        max_tokens: None,
        session_id: None,
        service_tier: None,
    };
    let body = encode_request("qwen3.8-flash-next", &req, false, &preset, false).expect("encode");
    assert_eq!(body["reasoning_effort"], "xhigh");

    let mut off_req = req;
    off_req.thinking = ThinkingLevel::Off;
    let body_off =
        encode_request("qwen3.8-flash-next", &off_req, false, &preset, false).expect("encode off");
    assert!(body_off.get("reasoning_effort").is_none());
}
