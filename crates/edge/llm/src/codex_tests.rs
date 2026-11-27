use super::*;
use crate::common::testutil::{VecSink, header, serve};
use crate::config::{AuthMode, ProviderKind};
use hrn_edge_http::HttpClient;
use hrn_edge_oauth::{OAuthManager, Subscription};
use hrn_kernel::{Msg, ThinkingLevel, ToolCall, ToolSpec};
use std::path::{Path, PathBuf};

fn request(messages: Vec<Msg>) -> LlmRequest {
    LlmRequest {
        model: None,
        messages,
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "read_file tool".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }],
        thinking: ThinkingLevel::Medium,
        thinking_budgets: None,
        temperature: Some(0.2),
        max_tokens: Some(100),
        session_id: Some("ses_1".into()),
        service_tier: None,
    }
}

#[test]
fn body_snapshot() {
    let call = ToolCall {
        id: "call.1/x".into(),
        name: "read_file".into(),
        arguments: r#"{"path":"a.rs"}"#.into(),
        signature: None,
    };
    let req = request(vec![
        Msg::system("You are hrn."),
        Msg::system("Second rule."),
        Msg::user("hi"),
        Msg::assistant_tool_calls("", None, vec![call]),
        Msg::tool_result("call.1/x", "read_file", "fn main() {}"),
    ]);
    let body = String::from_utf8(encode_body("gpt-5-codex", &req).expect("encode")).expect("utf8");
    let expected = concat!(
        r#"{"model":"gpt-5-codex","stream":true,"include":["reasoning.encrypted_content"],"#,
        r#""input":[{"content":[{"text":"You are hrn.","type":"input_text"}],"#,
        r#""internal_chat_message_metadata_passthrough":{"content_item_kinds":["model.base_instructions"]},"#,
        r#""role":"developer","type":"message"},"#,
        r#"{"content":[{"text":"Second rule.","type":"input_text"}],"role":"developer","type":"message"},"#,
        r#"{"content":"hi","role":"user"},"#,
        r#"{"arguments":"{\"path\":\"a.rs\"}","call_id":"call_1_x","name":"read_file","type":"function_call"},"#,
        r#"{"call_id":"call_1_x","output":"fn main() {}","type":"function_call_output"}],"#,
        r#""prompt_cache_key":"ses_1","reasoning":{"effort":"medium","summary":"auto"},"store":false,"#,
        r#""tools":[{"description":"read_file tool","name":"read_file","parameters":{"properties":{},"type":"object"},"type":"function"}]}"#,
    );
    assert!(
        body.starts_with(r#"{"model":"gpt-5-codex","stream":true,"#),
        "routing keys must serialize first: {body}"
    );
    let actual: Value = serde_json::from_str(&body).expect("actual json");
    let expected: Value = serde_json::from_str(expected).expect("expected json");
    assert_eq!(actual, expected);
}

#[test]
fn instructions_only_request_echoes_the_last_instruction_as_the_user_turn() {
    let req = request(vec![Msg::system("A"), Msg::system("B")]);
    let body: Value =
        serde_json::from_slice(&encode_body("m", &req).expect("encode")).expect("json");
    let input = body["input"].as_array().expect("input");
    assert_eq!(input.len(), 3);
    assert_eq!(input[2], json!({"role": "user", "content": "B"}));
}

#[test]
fn long_or_odd_call_ids_are_sanitized() {
    let mut item = json!({"call_id": "x".repeat(70)});
    sanitize_call_id(&mut item);
    assert_eq!(item["call_id"].as_str().expect("id").len(), 64);
    let mut clean = json!({"call_id": "call_AbC-1"});
    sanitize_call_id(&mut clean);
    assert_eq!(clean["call_id"], "call_AbC-1");
}

fn login_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hrn-codex-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let file = dir.join("chatgpt.json");
    let cred = SubscriptionCredential {
        provider: "chatgpt".into(),
        access: "tok".into(),
        refresh: "rt".into(),
        expires: u64::MAX / 2,
        chatgpt_account_id: Some("acct-9".into()),
        plan: Some("plus".into()),
        ..SubscriptionCredential::default()
    };
    std::fs::write(&file, serde_json::to_vec(&cred).expect("json")).expect("write");
    file
}

async fn run(base: &str, file: &Path) -> Result<LlmResponse> {
    let http = HttpClient::new().expect("http");
    let oauth = OAuthManager::default();
    let kind = Subscription::Chatgpt;
    let cred = oauth.subscription(&http, kind, file).await.expect("login");
    let mut auth = SubscriptionAuth::new(&http, &oauth, kind, file, cred);
    let prov = ProviderEntry {
        kind: ProviderKind::OpenaiResponses,
        base_url: Some(base.into()),
        auth_mode: AuthMode::Chatgpt,
        token_file: Some(file.to_string_lossy().into_owned()),
        ..ProviderEntry::default()
    };
    let req = request(vec![Msg::system("sys"), Msg::user("hi")]);
    let sink = VecSink::default();
    stream(
        &mut auth,
        &prov,
        "gpt-5-codex",
        &req,
        &CancelFlag::new(),
        &sink,
    )
    .await
}

fn limited(extra_headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\n{extra_headers}\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn usage_limit_reply_names_the_plan_and_reset() {
    let file = login_file("usage");
    let body =
        r#"{"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":1234}}"#;
    let (base, seen) = serve(1, move |_, _| limited("", body)).await;
    let err = run(&base, &file).await.expect_err("limited");
    assert_eq!(
        err.to_string(),
        "provider error: You have hit your ChatGPT usage limit (plus plan). Try again in ~21 min."
    );
    assert!(err.is_retryable());
    assert_eq!(err.retry_after_secs(), Some(1234));

    let seen = seen.lock().expect("seen").clone();
    let head = &seen[0];
    assert!(head.starts_with("POST /codex/responses HTTP/1.1"), "{head}");
    assert_eq!(header(head, "authorization"), Some("Bearer tok"));
    assert_eq!(header(head, "chatgpt-account-id"), Some("acct-9"));
    assert_eq!(header(head, "originator"), Some("hrn"));
    assert_eq!(header(head, "version"), Some("0.169.0"));
    assert_eq!(
        header(head, "x-codex-routing-hint"),
        Some("model=gpt-5-codex")
    );
    assert_eq!(
        header(head, "session-id"),
        header(head, "x-client-request-id")
    );
}

#[tokio::test]
async fn rate_limit_reply_uses_the_exhausted_window_reset() {
    let file = login_file("rate");
    let reset_at = now_secs() + 90;
    let headers = format!(
        "x-codex-primary-used-percent: 40\r\nx-codex-primary-reset-at: {}\r\n\
         x-codex-secondary-used-percent: 100.0\r\nx-codex-secondary-reset-at: {reset_at}\r\n",
        reset_at + 9000
    );
    let body = r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#;
    let (base, _) = serve(1, move |_, _| limited(&headers, body)).await;
    let err = run(&base, &file).await.expect_err("limited");
    assert_eq!(
        err.to_string(),
        "provider error: ChatGPT rate limit exceeded. Try again in ~2 min."
    );
}
