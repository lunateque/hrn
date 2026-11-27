use super::*;
use crate::common::testutil::{http_reply, serve};
use std::time::Instant;

#[tokio::test]
async fn ollama_discovery_from_stub() {
    let (base, _seen) = serve(3, |idx, head| {
        let first_line = head.lines().next().unwrap_or("");
        if first_line.starts_with("GET /api/tags") {
            http_reply(
                "200 OK",
                "application/json",
                r#"{"models":[{"name":"gpt-oss:20b"},{"name":"plain-local:latest"}]}"#,
            )
        } else if first_line.starts_with("POST /api/show") && idx == 1 {
            http_reply(
                "200 OK",
                "application/json",
                r#"{"model_info":{"llama.context_length":65536},"capabilities":["completion","tools","thinking"]}"#,
            )
        } else {
            http_reply("200 OK", "application/json", r#"{"model_info":{}}"#)
        }
    })
    .await;

    let http = HttpClient::new().expect("http");
    let cat = discover_ollama(&http, Some(&base), Duration::from_secs(2))
        .await
        .expect("ollama catalog");
    assert!(cat.providers.contains_key("ollama"));
    let gpt = &cat.presets["ollama/gpt-oss:20b"];
    assert_eq!(gpt.context_window, Some(65_536));
    assert_eq!(gpt.max_tokens, Some(8_192));
    assert_eq!(gpt.reasoning, Some(true));

    let plain = &cat.presets["ollama/plain-local:latest"];
    assert_eq!(plain.context_window, Some(128_000));
    assert_eq!(plain.max_tokens, Some(8_192));

    // Server down: pick an unused ephemeral port (bind then drop) and assert <300 ms, no error.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let closed_port = listener.local_addr().expect("addr").port();
    drop(listener);
    let dead_url = format!("http://127.0.0.1:{closed_port}");
    let start = Instant::now();
    let down = discover_ollama(&http, Some(&dead_url), DEFAULT_OLLAMA_TIMEOUT).await;
    let elapsed = start.elapsed();
    assert!(down.is_none());
    assert!(
        elapsed < Duration::from_millis(300),
        "ollama offline probe took {elapsed:?} (budget <300ms)"
    );
}

#[tokio::test]
async fn openrouter_discovery_filters_tools_only_and_parses_price() {
    let (base, _seen) = serve(1, |_idx, _head| {
        http_reply(
            "200 OK",
            "application/json",
            r#"{"data":[
                {"id":"anthropic/claude-sonnet-4.6","name":"Claude Sonnet","context_length":1000000,"supported_parameters":["tools","reasoning"],"pricing":{"prompt":"0.000003","completion":"0.000015"},"top_provider":{"max_completion_tokens":64000}},
                {"id":"no-tools/model","name":"Chat Only","context_length":32768,"supported_parameters":["temperature"]}
            ]}"#,
        )
    })
    .await;

    let tmp = std::env::temp_dir().join(format!("hrn-or-key-{}.key", std::process::id()));
    std::fs::write(&tmp, "sk-or-test").expect("key");
    let entry = ProviderEntry {
        kind: ProviderKind::OpenaiChat,
        base_url: Some(base),
        token_file: Some(tmp.to_string_lossy().into_owned()),
        ..ProviderEntry::default()
    };
    let http = HttpClient::new().expect("http");
    let cat = discover_openrouter(&http, &entry, Duration::from_secs(2))
        .await
        .expect("openrouter catalog");
    assert!(
        cat.presets
            .contains_key("openrouter/anthropic/claude-sonnet-4.6")
    );
    assert!(!cat.presets.contains_key("openrouter/no-tools/model"));
    let p = &cat.presets["openrouter/anthropic/claude-sonnet-4.6"];
    assert_eq!(p.context_window, Some(1_000_000));
    assert_eq!(p.max_tokens, Some(64_000));
    assert_eq!(p.thinking_format.as_deref(), Some("openrouter"));
    let price = p.price.expect("price");
    assert!((price.input - 3.0).abs() < 1e-6);
    assert!((price.output - 15.0).abs() < 1e-6);
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn openai_models_list_and_cache_roundtrip() {
    let (base, _seen) = serve(1, |_idx, _head| {
        http_reply(
            "200 OK",
            "application/json",
            r#"{"data":[{"id":"gpt-oss-120b","context_window":131072}]}"#,
        )
    })
    .await;
    let entry = ProviderEntry {
        kind: ProviderKind::OpenaiChat,
        base_url: Some(base),
        ..ProviderEntry::default()
    };
    let http = HttpClient::new().expect("http");
    let cat = openai_models_list(&http, "groq", &entry, Duration::from_secs(2))
        .await
        .expect("models list");
    assert_eq!(
        cat.presets["groq/gpt-oss-120b"].context_window,
        Some(131_072)
    );

    let dir = std::env::temp_dir().join(format!("hrn-cat-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    save_cache(&dir, "groq", &cat);
    let loaded = load_cache(&dir, "groq").expect("cached");
    assert_eq!(
        loaded.presets["groq/gpt-oss-120b"].context_window,
        Some(131_072)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn sample_agy_discovered() -> crate::agy_acp::DiscoveredAgyModels {
    crate::agy_acp::DiscoveredAgyModels {
        models: vec![
            crate::agy_acp::AcpModelOption {
                value: "gemini-3.1-pro-high".to_owned(),
                name: "Gemini 3.1 Pro (High)".to_owned(),
            },
            crate::agy_acp::AcpModelOption {
                value: "claude-sonnet-4-6".to_owned(),
                name: "Claude Sonnet 4.6".to_owned(),
            },
            crate::agy_acp::AcpModelOption {
                value: "antigravity-internal".to_owned(),
                name: "Blocked".to_owned(),
            },
        ],
        current_model: Some("claude-sonnet-4-6".to_owned()),
        tier_name: Some("Google AI Ultra".to_owned()),
        agent_version: "1.5.18".to_owned(),
    }
}

#[test]
fn build_agy_catalog_populates_context_roles_and_respects_user_default() {
    let entry = default_agy_provider();
    let cat = build_agy_catalog("agy", &entry, &sample_agy_discovered()).expect("agy catalog");
    assert_eq!(cat.presets.len(), 2);
    assert!(!cat.presets.contains_key("agy/antigravity-internal"));
    assert_eq!(
        cat.presets["agy/gemini-3.1-pro-high"].context_window,
        Some(1_000_000)
    );
    assert_eq!(cat.presets["agy/gemini-3.1-pro-high"].reasoning, Some(true));
    assert_eq!(
        cat.presets["agy/claude-sonnet-4-6"].context_window,
        Some(1_000_000)
    );
    assert_eq!(cat.presets["agy/claude-sonnet-4-6"].reasoning, Some(true));
    assert_eq!(
        cat.roles.get("agy").map(String::as_str),
        Some("agy/claude-sonnet-4-6")
    );
    assert_eq!(
        cat.roles.get("default").map(String::as_str),
        Some("agy/claude-sonnet-4-6")
    );

    // Zero-config machine: absorbing sets `default` role to agy's active model.
    let mut empty = LlmConfig::default();
    empty.absorb(cat.clone());
    assert_eq!(
        empty.roles.get("default").map(String::as_str),
        Some("agy/claude-sonnet-4-6")
    );
    assert_eq!(
        empty.roles.get("agy").map(String::as_str),
        Some("agy/claude-sonnet-4-6")
    );

    // Existing user default stays intact while `agy` role + presets become available.
    let mut configured = LlmConfig::default();
    configured
        .roles
        .insert("default".to_owned(), "local/main".to_owned());
    configured.absorb(cat);
    assert_eq!(
        configured.roles.get("default").map(String::as_str),
        Some("local/main")
    );
    assert_eq!(
        configured.roles.get("agy").map(String::as_str),
        Some("agy/claude-sonnet-4-6")
    );
    assert!(configured.presets.contains_key("agy/gemini-3.1-pro-high"));
}

#[test]
fn agy_cache_roundtrip_respects_ttl_and_patches_provider_entry() {
    let dir = std::env::temp_dir().join(format!("hrn-agy-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let entry = default_agy_provider();
    let cat = build_agy_catalog("agy", &entry, &sample_agy_discovered()).expect("agy catalog");
    save_cached_catalog(&dir, "agy", &cat);

    let fresh = load_fresh_agy_cache(&dir, "agy", AGY_CACHE_TTL).expect("fresh cache hit");
    assert_eq!(fresh.presets.len(), 2);

    std::thread::sleep(Duration::from_millis(15));
    assert!(load_fresh_agy_cache(&dir, "agy", Duration::from_millis(1)).is_none());

    let mut custom_entry = default_agy_provider();
    custom_entry.agy.tool_mode = crate::agy_acp::AgyToolMode::Native;
    let patched = patch_cached_agy(fresh, "agy", &custom_entry);
    assert_eq!(
        patched.providers["agy"].agy.tool_mode,
        crate::agy_acp::AgyToolMode::Native
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agy_defaults_and_cache_patch_upgrade_context_and_reasoning() {
    let entry = default_agy_provider();
    let discovered = crate::agy_acp::DiscoveredAgyModels {
        models: vec![
            crate::agy_acp::AcpModelOption {
                value: "claude-opus-5-5".to_owned(),
                name: "Claude Opus 5.5".to_owned(),
            },
            crate::agy_acp::AcpModelOption {
                value: "claude-sonnet-4-5".to_owned(),
                name: "Claude Sonnet 4.5".to_owned(),
            },
            crate::agy_acp::AcpModelOption {
                value: "gemini-4-neon".to_owned(),
                name: "Gemini 4 Neon".to_owned(),
            },
            crate::agy_acp::AcpModelOption {
                value: "gpt-oss-120b-medium".to_owned(),
                name: "GPT-OSS 120B (Medium)".to_owned(),
            },
        ],
        current_model: Some("claude-opus-5-5".to_owned()),
        tier_name: None,
        agent_version: "1.5.18".to_owned(),
    };
    let mut cat = build_agy_catalog("agy", &entry, &discovered).expect("agy catalog");
    assert_eq!(
        cat.presets["agy/claude-opus-5-5"].context_window,
        Some(1_000_000)
    );
    assert_eq!(cat.presets["agy/claude-opus-5-5"].reasoning, Some(true));
    assert_eq!(
        cat.presets["agy/claude-sonnet-4-5"].context_window,
        Some(200_000)
    );
    assert_eq!(
        cat.presets["agy/gemini-4-neon"].context_window,
        Some(1_000_000)
    );
    assert_eq!(cat.presets["agy/gemini-4-neon"].reasoning, Some(true));
    assert_eq!(
        cat.presets["agy/gpt-oss-120b-medium"].context_window,
        Some(131_072)
    );
    assert_eq!(cat.presets["agy/gpt-oss-120b-medium"].reasoning, Some(true));

    // Simulate a stale agy.json cache from an earlier hrn build (Claude 200k, Gemini reasoning=false).
    cat.presets
        .get_mut("agy/claude-opus-5-5")
        .expect("opus")
        .context_window = Some(200_000);
    cat.presets
        .get_mut("agy/gemini-4-neon")
        .expect("neon")
        .reasoning = Some(false);
    let patched = patch_cached_agy(cat, "agy", &entry);
    assert_eq!(
        patched.presets["agy/claude-opus-5-5"].context_window,
        Some(1_000_000)
    );
    assert_eq!(patched.presets["agy/gemini-4-neon"].reasoning, Some(true));
}

#[tokio::test]
async fn discover_startup_skips_agy_when_disabled() {
    let dir = std::env::temp_dir().join(format!("hrn-agy-disabled-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let http = HttpClient::new().expect("http");
    let cfg = LlmConfig {
        disabled_providers: vec![
            "ollama".to_owned(),
            "openrouter".to_owned(),
            "agy".to_owned(),
        ],
        ..LlmConfig::default()
    };
    let discovered = discover_startup(&http, &cfg, Some(&dir)).await;
    assert!(
        discovered
            .iter()
            .all(|cat| !cat.providers.contains_key("agy")),
        "disabled_providers=[\"agy\"] must suppress auto-discovery"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
