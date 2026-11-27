//! Acceptance and unit tests for service-tier encoding across all provider adapters.

use super::*;
use crate::config::ModelPreset;
use hrn_edge_oauth::SubscriptionCredential;
use hrn_kernel::{LlmRequest, Msg, ThinkingLevel};
use serde_json::{Value, json};

fn req_with_tier(service_tier: Option<Tier>) -> LlmRequest {
    LlmRequest {
        model: None,
        messages: vec![Msg::system("sys"), Msg::user("hello")],
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        thinking_budgets: None,
        temperature: None,
        max_tokens: None,
        session_id: Some("ses_1".into()),
        service_tier,
    }
}

#[test]
fn encoder_service_tier_priority() {
    let req = req_with_tier(Some(Tier::Priority));
    let preset = ModelPreset::default();

    // OpenAI /chat/completions
    let chat = crate::openai_chat::encode_request("gpt-5", &req, false, &preset, false)
        .expect("openai chat");
    assert_eq!(chat.get("service_tier"), Some(&json!("priority")));

    // OpenAI /responses
    let resp =
        crate::openai_responses::encode_request("gpt-5.2-codex", &req, true).expect("responses");
    assert_eq!(resp.get("service_tier"), Some(&json!("priority")));

    // Codex subscription wire body (also checks leading key order)
    let codex_bytes = crate::codex::encode_body("gpt-5.2-codex", &req).expect("codex");
    let codex: Value = serde_json::from_slice(&codex_bytes).expect("codex json");
    assert_eq!(codex.get("service_tier"), Some(&json!("priority")));
    let raw_codex = String::from_utf8(codex_bytes).expect("utf8");
    let tier_pos = raw_codex.find("\"service_tier\"").expect("service_tier");
    let input_pos = raw_codex.find("\"input\"").expect("input");
    assert!(tier_pos < input_pos, "service_tier must lead input");

    // Gemini Developer API: priority & flex
    let gem_prio = crate::gemini::encode_request("gemini-2.5-flash", &req, false).expect("gemini");
    assert_eq!(gem_prio.get("serviceTier"), Some(&json!("priority")));
    let gem_flex =
        crate::gemini::encode_request("gemini-2.5-flash", &req_with_tier(Some(Tier::Flex)), false)
            .expect("gemini flex");
    assert_eq!(gem_flex.get("serviceTier"), Some(&json!("flex")));

    // Anthropic Messages: direct Claude emits priority; Bedrock/Vertex/OpenRouter omit it.
    let direct = crate::anthropic_wire::encode_request_for_host(
        "claude-sonnet-4-6",
        &req,
        false,
        false,
        true,
    )
    .expect("anthropic direct");
    assert_eq!(direct.get("service_tier"), Some(&json!("priority")));
    let non_direct = crate::anthropic_wire::encode_request_for_host(
        "claude-sonnet-4-6",
        &req,
        false,
        false,
        false,
    )
    .expect("anthropic non-direct");
    assert!(non_direct.get("service_tier").is_none());

    // Claude Code subscription body emits priority.
    let cred = SubscriptionCredential {
        provider: "claude".into(),
        access: "tok".into(),
        account_id: Some("acct-1".into()),
        ..SubscriptionCredential::default()
    };
    let shaped =
        crate::claude_code::encode("claude-sonnet-4-6", &req, &cred, "ses_1", "2.1.326", false)
            .expect("claude_code");
    let cc: Value = serde_json::from_slice(&shaped.body).expect("cc json");
    assert_eq!(cc.get("service_tier"), Some(&json!("priority")));
}

#[test]
fn tier_none_omits_key() {
    let preset = ModelPreset::default();
    let cred = SubscriptionCredential {
        provider: "claude".into(),
        access: "tok".into(),
        account_id: Some("acct-1".into()),
        ..SubscriptionCredential::default()
    };
    for tier in [None, Some(Tier::None)] {
        let req = req_with_tier(tier);
        let chat =
            crate::openai_chat::encode_request("gpt-5", &req, false, &preset, false).expect("chat");
        assert!(chat.get("service_tier").is_none(), "chat {tier:?}");

        let resp =
            crate::openai_responses::encode_request("gpt-5.2-codex", &req, true).expect("resp");
        assert!(resp.get("service_tier").is_none(), "responses {tier:?}");

        let codex_bytes = crate::codex::encode_body("gpt-5.2-codex", &req).expect("codex");
        let codex: Value = serde_json::from_slice(&codex_bytes).expect("codex json");
        assert!(codex.get("service_tier").is_none(), "codex {tier:?}");

        let gem = crate::gemini::encode_request("gemini-2.5-flash", &req, false).expect("gemini");
        assert!(gem.get("serviceTier").is_none(), "gemini {tier:?}");

        let anth = crate::anthropic_wire::encode_request_for_host(
            "claude-sonnet-4-6",
            &req,
            false,
            false,
            true,
        )
        .expect("anthropic");
        assert!(anth.get("service_tier").is_none(), "anthropic {tier:?}");

        let shaped =
            crate::claude_code::encode("claude-sonnet-4-6", &req, &cred, "ses_1", "2.1.326", false)
                .expect("cc");
        let cc: Value = serde_json::from_slice(&shaped.body).expect("cc json");
        assert!(cc.get("service_tier").is_none(), "claude_code {tier:?}");
    }
}

#[test]
fn direct_anthropic_url_blocks_bedrock_vertex_openrouter() {
    assert!(is_direct_anthropic_url("https://api.anthropic.com"));
    assert!(!is_direct_anthropic_url("https://openrouter.ai/api/v1"));
    assert!(!is_direct_anthropic_url(
        "https://bedrock-runtime.us-east-1.amazonaws.com"
    ));
    assert!(!is_direct_anthropic_url(
        "https://us-central1-aiplatform.googleapis.com"
    ));
}

#[test]
fn openai_responses_proxy_omits_service_tier() {
    let proxy = crate::config::ProviderEntry {
        kind: crate::config::ProviderKind::OpenaiResponses,
        base_url: Some("https://proxy.example.com/v1".into()),
        ..crate::config::ProviderEntry::default()
    };
    assert_eq!(
        provider_family(&proxy, "gpt-5.3-codex"),
        ProviderFamily::Other
    );
    let tier_cfg = crate::config_pool::LlmTierConfig {
        openai: Tier::Priority,
        ..crate::config_pool::LlmTierConfig::default()
    };
    let resolved = resolve_service_tier(
        Some(Tier::Priority),
        Some(Tier::Priority),
        &tier_cfg,
        &proxy,
        "gpt-5.3-codex",
    );
    assert_eq!(resolved, None, "custom proxy must not receive service_tier");

    let mut cfg = crate::config::LlmConfig::default();
    cfg.providers.insert("proxy".into(), proxy);
    cfg.presets.insert(
        "codex".into(),
        ModelPreset {
            provider: "proxy".into(),
            id: "gpt-5.3-codex".into(),
            ..ModelPreset::default()
        },
    );
    assert!(!supports_service_tier(&cfg, Some("codex")));

    let body =
        crate::openai_responses::encode_request("gpt-5.3-codex", &req_with_tier(resolved), true)
            .expect("encode");
    assert!(body.get("service_tier").is_none());

    // Official OpenAI / ChatGPT hosts remain supported.
    let official = crate::config::ProviderEntry {
        kind: crate::config::ProviderKind::OpenaiResponses,
        base_url: None,
        ..crate::config::ProviderEntry::default()
    };
    assert_eq!(
        provider_family(&official, "gpt-5.3-codex"),
        ProviderFamily::Openai
    );
    assert_eq!(
        resolve_service_tier(None, None, &tier_cfg, &official, "gpt-5.3-codex"),
        Some(Tier::Priority)
    );
}
