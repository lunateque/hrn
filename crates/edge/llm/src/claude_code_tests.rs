use super::*;
use hrn_kernel::{Msg, ThinkingLevel, ToolCall, ToolSpec};

/// Reference values computed from the published algorithm with an independent implementation
/// (Bun's SHA-256 and `xxHash64`), not captured from Claude Code traffic.
#[test]
fn billing_suffix_and_cch_match_reference_fixtures() {
    assert_eq!(
        billing_suffix("Please refactor the parser module now", "2.1.326"),
        "799"
    );
    // Missing code units count as "0".
    assert_eq!(billing_suffix("hi", "2.1.326"), "bc0");

    let mut body = br#"{"model":"claude-x","system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.326.abc; cc_entrypoint=cli; cch=00000;"}],"stream":true}"#.to_vec();
    apply_cch(&mut body);
    let text = String::from_utf8(body).expect("utf8");
    assert!(text.contains("cch=905b3;"), "{text}");

    let mut untouched = br#"{"system":"cch=00000;"}"#.to_vec();
    apply_cch(&mut untouched);
    assert_eq!(untouched, br#"{"system":"cch=00000;"}"#);
}

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("{name} tool"),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

fn request(thinking: ThinkingLevel) -> LlmRequest {
    let call = ToolCall {
        id: "toolu_1".into(),
        name: "read_file".into(),
        arguments: r#"{"path":"a.rs"}"#.into(),
        signature: None,
    };
    LlmRequest {
        model: None,
        thinking_budgets: None,
        messages: vec![
            Msg::system("You are hrn."),
            Msg::system("Rules.\n<project-context>cwd: C:/x</project-context>"),
            Msg::system("<memories>none</memories>"),
            Msg::user("Please refactor the parser module now"),
            Msg::assistant_tool_calls("Reading.", None, vec![call]),
            Msg::tool_result("toolu_1", "read_file", "fn main() {}"),
        ],
        tools: vec![tool("read_file"), tool("web_search")],
        thinking,
        temperature: None,
        max_tokens: None,
        session_id: Some("ses_1".into()),
        service_tier: None,
    }
}

fn credential() -> SubscriptionCredential {
    SubscriptionCredential {
        provider: "claude".into(),
        access: "sk-ant-oat-test".into(),
        account_id: Some("acct-1".into()),
        email: Some("a@example.com".into()),
        ..SubscriptionCredential::default()
    }
}

#[test]
fn encode_shapes_body_like_claude_code() {
    let req = request(ThinkingLevel::Off);
    let shaped = encode(
        "claude-x",
        &req,
        &credential(),
        "sess-uuid",
        "2.1.326",
        false,
    )
    .expect("encode");
    let text = String::from_utf8(shaped.body.clone()).expect("utf8");
    assert!(
        text.starts_with(r#"{"model":"claude-x","messages":["#),
        "{text}"
    );
    assert!(text.contains(r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.326.799; cc_entrypoint=cli; cch="#));
    assert!(!text.contains("cch=00000"), "checksum applied");
    assert!(text.matches("cache_control").count() <= 4);

    // The checksum covers the body with the placeholder in place.
    let at = text.find("cch=").expect("cch") + 4;
    let mut placeholder = shaped.body.clone();
    placeholder[at..at + 5].copy_from_slice(b"00000");
    let expected = format!("{:05x}", xxh64(&placeholder, CCH_SEED) & 0xf_ffff);
    assert_eq!(&text[at..at + 5], expected);

    let body: Value = serde_json::from_slice(&shaped.body).expect("json");
    let system = body["system"].as_array().expect("system");
    let texts: Vec<&str> = system
        .iter()
        .map(|b| b["text"].as_str().expect("text"))
        .collect();
    assert_eq!(
        texts[1..],
        [
            IDENTITY,
            "You are hrn.",
            "Rules.\n",
            "<project-context>cwd: C:/x</project-context>",
            "<memories>none</memories>",
        ]
    );
    let cached: Vec<bool> = system
        .iter()
        .map(|b| b.get("cache_control").is_some())
        .collect();
    assert_eq!(cached, [false, false, false, true, false, false]);
    assert_eq!(
        system[3]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );

    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools[0]["name"], "_read_file");
    assert_eq!(tools[1]["name"], "web_search");
    assert!(tools[0].get("cache_control").is_none());
    assert!(tools[1].get("cache_control").is_some());

    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["content"][1]["name"], "_read_file");
    assert!(messages[0]["content"][0].get("cache_control").is_none());
    assert!(messages[1]["content"][1].get("cache_control").is_some());
    assert!(messages[2]["content"][0].get("cache_control").is_some());

    let user_id = body["metadata"]["user_id"].as_str().expect("user_id");
    assert!(user_id.starts_with(r#"{"device_id":""#), "{user_id}");
    assert!(
        user_id.ends_with(r#"","session_id":"sess-uuid","account_uuid":"acct-1"}"#),
        "{user_id}"
    );
    assert_eq!(
        shaped.betas,
        "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
         thinking-token-count-2026-05-13,context-management-2025-06-27,\
         prompt-caching-scope-2026-01-05,fallback-credit-2026-06-01"
    );
}

#[test]
fn betas_follow_the_turn_kind() {
    assert_eq!(
        betas(false, false),
        "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
         context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
         structured-outputs-2025-12-15"
    );
    assert!(betas(true, true).ends_with(",effort-2025-11-24,fallback-credit-2026-06-01"));
    let req = request(ThinkingLevel::High);
    let mut plain = req.clone();
    plain.tools.clear();
    plain.messages.truncate(4);
    plain.thinking = ThinkingLevel::Off;
    let shaped = encode("claude-x", &plain, &credential(), "s", "2.1.326", false).expect("encode");
    assert!(shaped.betas.ends_with("structured-outputs-2025-12-15"));
}

#[test]
fn haiku_3_5_gets_no_fingerprint_blocks() {
    let mut req = request(ThinkingLevel::Off);
    req.messages.truncate(1);
    req.messages.push(Msg::user("hi"));
    req.tools.clear();
    let shaped = encode(
        "claude-3-5-haiku-20241022",
        &req,
        &credential(),
        "s",
        "2.1.326",
        false,
    )
    .expect("encode");
    let body: Value = serde_json::from_slice(&shaped.body).expect("json");
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "You are hrn.", "cache_control": {"type": "ephemeral", "ttl": "1h"}}])
    );
}

#[test]
fn version_comes_from_env_then_newer_persisted_value() {
    let dir = std::env::temp_dir().join(format!("hrn-cc-version-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let store = dir.join("claude-code-version.txt");
    let _ = std::fs::remove_file(&store);
    assert_eq!(resolve_version(None, &store), DEFAULT_VERSION);
    assert_eq!(resolve_version(Some(" 2.2.0 ".into()), &store), "2.2.0");
    std::fs::write(&store, "2.1.500\n").expect("write");
    assert_eq!(resolve_version(Some("junk".into()), &store), "2.1.500");
    std::fs::write(&store, "2.0.1").expect("write");
    assert_eq!(resolve_version(None, &store), DEFAULT_VERSION);
    let _ = std::fs::remove_dir_all(&dir);

    let rejection = r#"{"type":"error","error":{"type":"invalid_request_error","message":"claude_code_version_too_old: Claude Code version 2.1.400 or newer is required"}}"#;
    assert_eq!(
        required_version(rejection, "2.1.326").as_deref(),
        Some("2.1.400")
    );
    assert_eq!(required_version(rejection, "2.1.400"), None);
    assert_eq!(required_version("overloaded", "2.1.326"), None);
}

#[test]
fn headers_carry_the_claude_code_identity() {
    let headers = headers("2.1.326", "tok", "sess", "b1,b2");
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        get("user-agent"),
        Some("claude-cli/2.1.326 (external, cli)")
    );
    assert_eq!(get("authorization"), Some("Bearer tok"));
    assert_eq!(get("anthropic-beta"), Some("b1,b2"));
    assert_eq!(get("x-app"), Some("cli"));
    assert_eq!(get("x-claude-code-session-id"), Some("sess"));
    assert_eq!(get("anthropic-version"), Some("2023-06-01"));
    assert_eq!(get("x-api-key"), None);
}
