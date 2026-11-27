use super::*;
use crate::common::testutil::{VecSink, header, http_reply, serve};
use crate::config::{AuthMode, LlmConfig, ModelPreset, ProviderEntry, ProviderKind};
use crate::config_pool::KimiApiFormat;
use crate::router::LlmRouter;
use hrn_edge_oauth::{
    OAuthManager, Subscription, SubscriptionCredential, load_or_create_kimi_device_id,
    upsert_subscription_account,
};
use hrn_kernel::{CancelFlag, LlmProvider, LlmRequest, Msg, ThinkingLevel};

const OPENAI_SSE_OK: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"kimi-openai\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

const ANTHROPIC_SSE_OK: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":4}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"kimi-anthropic\"}}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

const KIMI_USAGES_JSON: &str = r#"{
  "usage": { "limit": "100", "used": "25", "remaining": "75", "resetTime": "2099-01-01T00:00:00Z" },
  "limits": [
    {
      "window": { "duration": 300, "timeUnit": "TIME_UNIT_MINUTE" },
      "detail": { "limit": 100, "used": 40, "reset_after_seconds": 900 }
    }
  ]
}"#;

fn build_kimi_cfg(coding_base: &str, cred_file: &Path, api_format: KimiApiFormat) -> LlmConfig {
    let mut cfg = LlmConfig::default();
    cfg.kimi.api_format = api_format;
    cfg.providers.insert(
        "kimi-code".into(),
        ProviderEntry {
            kind: ProviderKind::OpenaiChat,
            base_url: Some(coding_base.to_owned()),
            auth_mode: AuthMode::KimiCode,
            token_file: Some(cred_file.to_string_lossy().into_owned()),
            ..ProviderEntry::default()
        },
    );
    cfg.presets.insert(
        "kimi-for-coding".into(),
        ModelPreset {
            provider: "kimi-code".into(),
            id: "kimi-for-coding".into(),
            replay_reasoning_content: true,
            ..ModelPreset::default()
        },
    );
    cfg.roles.insert("default".into(), "kimi-for-coding".into());
    cfg
}

fn assert_kimi_fingerprint_headers(reqs: &[String], expected_device_id: &str) {
    assert_eq!(reqs.len(), 3);
    assert!(reqs[0].starts_with("POST /coding/v1/chat/completions HTTP/1.1"));
    assert!(reqs[1].starts_with("POST /coding/v1/messages HTTP/1.1"));
    assert!(reqs[2].starts_with("GET /coding/v1/usages HTTP/1.1"));
    assert_eq!(header(&reqs[1], "anthropic-version"), Some("2023-06-01"));

    for (idx, raw) in reqs.iter().enumerate() {
        assert_eq!(
            header(raw, "authorization"),
            Some("Bearer kimi-access-tok"),
            "req {idx}"
        );
        assert!(
            header(raw, "user-agent").is_some_and(|ua| ua.starts_with("KimiCLI/")),
            "req {idx}: {raw}"
        );
        assert_eq!(header(raw, "x-msh-platform"), Some("kimi_cli"), "req {idx}");
        assert!(header(raw, "x-msh-version").is_some(), "req {idx}");
        assert!(header(raw, "x-msh-device-name").is_some(), "req {idx}");
        assert!(header(raw, "x-msh-device-model").is_some(), "req {idx}");
        assert!(header(raw, "x-msh-os-version").is_some(), "req {idx}");
        assert_eq!(
            header(raw, "x-msh-device-id"),
            Some(expected_device_id),
            "req {idx}"
        );
    }
}

#[tokio::test]
async fn kimi_code_headers() {
    let dir = std::env::temp_dir().join(format!("hrn-kimi-hdrs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let cred_file = dir.join(Subscription::KimiCode.file_name());
    let cred = SubscriptionCredential {
        provider: "kimi-code".into(),
        access: "kimi-access-tok".into(),
        refresh: "kimi-refresh-tok".into(),
        expires: u64::MAX / 2,
        email: Some("dev@kimi.test".into()),
        ..SubscriptionCredential::default()
    };
    upsert_subscription_account(Subscription::KimiCode, &cred_file, cred.clone()).expect("seed");

    let official = ProviderEntry {
        kind: ProviderKind::OpenaiChat,
        auth_mode: AuthMode::KimiCode,
        token_file: Some(cred_file.to_string_lossy().into_owned()),
        ..ProviderEntry::default()
    };
    assert_eq!(
        official.effective_base_url(),
        "https://api.kimi.com/coding/v1"
    );
    assert!(crate::config::validate_provider("kimi-code", &official).is_ok());

    let mut evil = official.clone();
    evil.base_url = Some("https://evil.example.com/coding/v1".into());
    assert!(crate::config::validate_provider("kimi-code", &evil).is_err());

    let (base, seen) = serve(3, |i, _| match i {
        0 => http_reply("200 OK", "text/event-stream", OPENAI_SSE_OK),
        1 => http_reply("200 OK", "text/event-stream", ANTHROPIC_SSE_OK),
        _ => http_reply("200 OK", "application/json", KIMI_USAGES_JSON),
    })
    .await;
    let coding_base = format!("{base}/coding/v1");
    let req = LlmRequest {
        model: Some("kimi-for-coding".into()),
        messages: vec![Msg::user("hello kimi")],
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        thinking_budgets: None,
        temperature: None,
        max_tokens: None,
        session_id: Some("ses_kimi".into()),
        service_tier: None,
    };
    let http = HttpClient::new().expect("http");

    let r1 = LlmRouter::try_new(
        build_kimi_cfg(&coding_base, &cred_file, KimiApiFormat::Auto),
        http.clone(),
        OAuthManager::default(),
    )
    .expect("router openai")
    .stream_chat(&req, &CancelFlag::new(), &VecSink::default())
    .await
    .expect("openai stream");
    assert_eq!(r1.content, "kimi-openai");

    let r2 = LlmRouter::try_new(
        build_kimi_cfg(&coding_base, &cred_file, KimiApiFormat::Anthropic),
        http.clone(),
        OAuthManager::default(),
    )
    .expect("router anthropic")
    .stream_chat(&req, &CancelFlag::new(), &VecSink::default())
    .await
    .expect("anthropic stream");
    assert_eq!(r2.content, "kimi-anthropic");

    let usage = fetch_from_url(&http, &cred, &dir, &format!("{coding_base}/usages")).await;
    assert_eq!(usage.problem, None);
    assert_eq!(usage.identity, "dev@kimi.test");
    assert_eq!(usage.windows.len(), 2);
    assert_eq!(usage.windows[0].label, "Kimi Weekly");
    assert_eq!(usage.windows[0].used_percent, Some(25.0));
    assert_eq!(usage.windows[1].label, "Kimi 5h");
    assert_eq!(usage.windows[1].used_percent, Some(40.0));
    assert_eq!(usage.windows[1].resets_in_secs, Some(900));

    let expected_device_id = load_or_create_kimi_device_id(&dir)
        .await
        .expect("persisted device id");
    assert!(dir.join("kimi-device-id").is_file());
    let reqs = seen.lock().expect("seen").clone();
    assert_kimi_fingerprint_headers(&reqs, &expected_device_id);
    let _ = std::fs::remove_dir_all(&dir);
}
