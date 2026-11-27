//! Unit tests covering config, binary discovery helpers, `/usage` parsing, ephemeral agent
//! profiles, DAG `SessionMap`, two-tier handoff, and ACP wire/error classification.

use hrn_edge_llm::agy_acp::agent_profile::{
    EphemeralAgentProfile, cleanup_stale_with_checker, parse_agent_dir_pid, render_agent_markdown,
    validate_builtin_tools,
};
use hrn_edge_llm::agy_acp::binary::{is_supported_version, merged_no_proxy_from, parse_semver};
use hrn_edge_llm::agy_acp::classify::{
    classify_jsonrpc_error, detect_quota_exhaustion, retry_after_from_quota, thinking_level_to_acp,
};
use hrn_edge_llm::agy_acp::client::build_permission_reply;
use hrn_edge_llm::agy_acp::config::{AgyProviderConfig, AgyToolMode};
use hrn_edge_llm::agy_acp::handoff::{
    BootstrapStrategy, HandoffSummarizer, build_restored_branch_context, estimate_messages_tokens,
    prepare_bootstrap_prompt, truncate_chars,
};
use hrn_edge_llm::agy_acp::session_map::{
    AgySessionEntry, RouteDecision, SessionMap, hash_messages_prefix,
};
use hrn_edge_llm::agy_acp::usage::parse_agy_usage_json;
use hrn_edge_llm::agy_acp::wire::{
    AcpPermissionRequest, AcpPromptResult, InlineQuota, is_hrn_bridge_permission,
    parse_capabilities, parse_session_new, read_bounded_line,
};
use hrn_kernel::{
    CancelFlag, HrnError, Msg, Result as HrnResult, ScopedFuture, ThinkingLevel, Usage,
};
use serde_json::json;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

type TestResult = std::result::Result<(), Box<dyn Error>>;

#[test]
fn config_defaults_toml_roundtrip_and_validation() -> TestResult {
    let def = AgyProviderConfig::default();
    assert_eq!(def.tool_mode, AgyToolMode::Hrn);
    assert_eq!(def.handoff_threshold_tokens, 12_000);
    assert_eq!(def.handoff_model, "smol");
    assert_eq!(def.turn_timeout_ms, 600_000);
    assert_eq!(def.stall_timeout_ms, 120_000);
    assert_eq!(def.tool_stall_timeout_ms, 300_000);
    def.validate("agy")?;

    let encoded = toml::to_string(&def)?;
    let decoded: AgyProviderConfig = toml::from_str(&encoded)?;
    assert_eq!(decoded, def);

    let mut bad = def.clone();
    bad.binary = Some("   ".to_owned());
    assert!(matches!(bad.validate("agy"), Err(HrnError::Config(_))));

    let mut bad_turn = def.clone();
    bad_turn.turn_timeout_ms = 1_000;
    assert!(matches!(bad_turn.validate("agy"), Err(HrnError::Config(_))));

    let mut bad_stall = def.clone();
    bad_stall.stall_timeout_ms = 500;
    assert!(matches!(
        bad_stall.validate("agy"),
        Err(HrnError::Config(_))
    ));

    let mut bad_model = def;
    bad_model.handoff_model = "  ".to_owned();
    assert!(matches!(
        bad_model.validate("agy"),
        Err(HrnError::Config(_))
    ));
    Ok(())
}

#[test]
fn semver_and_no_proxy_helpers() {
    assert_eq!(parse_semver("1.5.18"), Some((1, 5, 18)));
    assert_eq!(parse_semver("agy v1.5.0-rc1"), Some((1, 5, 0)));
    assert_eq!(parse_semver("not-a-version"), None);

    assert!(!is_supported_version("1.4.9"));
    assert!(is_supported_version("1.5.0"));
    assert!(is_supported_version("1.5.18"));
    assert!(is_supported_version("agy 2.0.0"));

    let merged = merged_no_proxy_from(
        Some("corp.internal,127.0.0.1"),
        Some("LOCALHOST,staging.local"),
    );
    let upper = merged.get("NO_PROXY").map_or("", String::as_str);
    let lower = merged.get("no_proxy").map_or("", String::as_str);
    assert_eq!(upper, lower);
    assert!(upper.contains("corp.internal"));
    assert!(upper.contains("staging.local"));
    assert!(upper.contains("127.0.0.1"));
    assert!(upper.contains("::1"));
    assert_eq!(upper.matches("127.0.0.1").count(), 1);
}

#[test]
fn parse_live_agy_usage_json_including_epoch_zero() -> TestResult {
    let fixture = r#"{
      "response": "Usage data retrieved",
      "status": "SUCCESS",
      "command": {
        "name": "usage",
        "data": {
          "groups": [
            {
              "name": "Gemini Models",
              "buckets": [
                {
                  "name": "Weekly",
                  "window": "weekly",
                  "remaining_fraction": 0.884,
                  "reset_time": "2026-12-01T12:00:00Z"
                },
                {
                  "name": "Session",
                  "window": "5h",
                  "remaining_fraction": 1.0,
                  "reset_time": "1970-01-01T00:00:00Z"
                }
              ]
            }
          ]
        }
      }
    }"#;

    let now = 1_796_122_800_u64;
    let acct = parse_agy_usage_json(fixture, now)?;
    assert!(acct.problem.is_none());
    assert_eq!(acct.windows.len(), 2);

    let first = acct.windows.first().ok_or("missing first window")?;
    assert_eq!(first.label, "Gemini Models (weekly)");
    let used = first.used_percent.ok_or("missing used_percent")?;
    assert!((used - 11.6).abs() < 0.01);
    assert_eq!(first.resets_in_secs, Some(3600));

    let second = acct.windows.get(1).ok_or("missing second window")?;
    assert_eq!(second.label, "Gemini Models (5h)");
    assert_eq!(second.used_percent, Some(0.0));
    assert_eq!(second.resets_in_secs, None);

    let err_status =
        parse_agy_usage_json(r#"{"status":"ERROR","response":"Not authenticated"}"#, now)?;
    assert_eq!(err_status.problem.as_deref(), Some("Not authenticated"));
    assert!(err_status.windows.is_empty());

    assert!(parse_agy_usage_json("{not json", now).is_err());
    Ok(())
}

#[test]
fn ephemeral_agent_profile_lifecycle_and_orphan_sweep() -> TestResult {
    validate_builtin_tools(&[])?;
    validate_builtin_tools(&["read_file".to_owned()])?;
    let both = vec!["view_file".to_owned(), "read_file".to_owned()];
    assert!(matches!(
        validate_builtin_tools(&both),
        Err(HrnError::Config(_))
    ));

    let root = unique_temp_dir("agent_profile");
    std::fs::create_dir_all(&root)?;

    let md = render_agent_markdown(
        "hrn-tools-123-abc",
        "http://127.0.0.1:4321/mcp",
        "deadbeef",
        &["read".to_owned(), "hrn__bash".to_owned()],
        &[],
    );
    assert!(md.contains("tools: []"));
    assert!(md.contains("inheritCustomizations: false"));
    assert!(md.contains("inheritMcp: false"));
    assert!(md.contains("\"hrn__read\":"));
    assert!(md.contains("\"hrn__bash\":"));
    assert!(md.contains("x-hrn-bridge-token: \"deadbeef\""));

    let created_dir = {
        let prof = EphemeralAgentProfile::create_in(
            &root,
            "http://127.0.0.1:9999/mcp",
            "tok123",
            &["grep".to_owned()],
        )?;
        let dir = prof.dir().to_path_buf();
        assert!(dir.join("AGENT.md").is_file());
        assert_eq!(parse_agent_dir_pid(prof.name()), Some(std::process::id()));
        dir
    };
    assert!(
        !created_dir.exists(),
        "Drop must remove ephemeral agent dir"
    );

    let dead_dir = root.join("hrn-tools-999991-aaaa");
    let live_dir = root.join("hrn-tools-999992-bbbb");
    let mine_dir = root.join(format!("hrn-tools-{}-cccc", std::process::id()));
    let unrelated = root.join("custom-user-agent");
    for d in [&dead_dir, &live_dir, &mine_dir, &unrelated] {
        std::fs::create_dir_all(d)?;
    }

    let removed = cleanup_stale_with_checker(&root, |pid| pid == 999_992);
    assert_eq!(removed, 1);
    assert!(!dead_dir.exists());
    assert!(live_dir.exists());
    assert!(mine_dir.exists());
    assert!(unrelated.exists());

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn session_map_dag_routing_lru_and_invalidation() -> TestResult {
    let mut map = SessionMap::with_capacity(4);
    let m1 = vec![Msg::user("hello"), Msg::assistant("hi there")];
    let h1 = hash_messages_prefix(&m1);
    assert_eq!(h1, hash_messages_prefix(&m1));

    map.record(
        "s1",
        AgySessionEntry {
            agy_session_id: "agy-A".to_owned(),
            synced_prefix_hash: h1,
            synced_msg_count: m1.len(),
            model_id: "m1".to_owned(),
            effort: ThinkingLevel::Medium,
            last_used_at: Instant::now(),
        },
    );

    // Linear continuation.
    let mut m_lin = m1.clone();
    m_lin.push(Msg::user("next step"));
    assert!(matches!(
        map.route("s1", &m_lin),
        RouteDecision::ContinueLinear { delta_start: 2, .. }
    ));

    // Switch to branch B.
    let m_branch_b = vec![Msg::user("branch B"), Msg::assistant("reply B")];
    map.record(
        "s1",
        AgySessionEntry {
            agy_session_id: "agy-B".to_owned(),
            synced_prefix_hash: hash_messages_prefix(&m_branch_b),
            synced_msg_count: m_branch_b.len(),
            model_id: "m1".to_owned(),
            effort: ThinkingLevel::Medium,
            last_used_at: Instant::now() + Duration::from_secs(1),
        },
    );

    // Returning to branch A should hit ResumeBranch (session/resume) instead of ForkOrBootstrap.
    match map.route("s1", &m_lin) {
        RouteDecision::ResumeBranch { entry, delta_start } => {
            assert_eq!(entry.agy_session_id, "agy-A");
            assert_eq!(delta_start, 2);
        }
        other => return Err(format!("expected ResumeBranch, got {other:?}").into()),
    }

    // Rewind before synced prefix -> ForkOrBootstrap.
    let rewound = vec![Msg::user("rewound")];
    assert!(matches!(
        map.route("s1", &rewound),
        RouteDecision::ForkOrBootstrap { fork_from: Some(_) }
    ));

    // Invalidation removes agy-A cleanly.
    map.invalidate_agy_session("agy-A");
    assert!(matches!(
        map.route("s1", &m_lin),
        RouteDecision::ForkOrBootstrap { .. }
    ));

    // LRU eviction enforces capacity=4.
    for i in 0..6u32 {
        let msgs = vec![Msg::user(format!("k{i}"))];
        map.record(
            "s1",
            AgySessionEntry {
                agy_session_id: format!("agy-{i}"),
                synced_prefix_hash: hash_messages_prefix(&msgs),
                synced_msg_count: 1,
                model_id: "m1".to_owned(),
                effort: ThinkingLevel::Low,
                last_used_at: Instant::now() + Duration::from_millis(u64::from(i) * 10),
            },
        );
    }
    assert!(map.len() <= 4);
    Ok(())
}

struct MockSummarizer {
    calls: AtomicUsize,
    fail: bool,
}

impl HandoffSummarizer for MockSummarizer {
    fn summarize<'a>(
        &'a self,
        model: &'a str,
        _messages: &'a [Msg],
        _cancel: &'a CancelFlag,
    ) -> ScopedFuture<'a, HrnResult<String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let fail = self.fail;
        let model_owned = model.to_owned();
        Box::pin(async move {
            if fail {
                Err(HrnError::Provider {
                    message: "summarizer down".to_owned(),
                    retryable: true,
                    retry_after_secs: None,
                })
            } else {
                Ok(format!("CHECKPOINT_BY_{model_owned}"))
            }
        })
    }
}

#[tokio::test]
async fn two_tier_handoff_and_utf8_truncation() -> TestResult {
    let cyrillic = "Привет мир 🚀".repeat(20);
    let truncated = truncate_chars(&cyrillic, 15);
    assert!(truncated.contains("[truncated"));
    assert!(truncated.starts_with("Привет мир 🚀При"));

    let big_tool = Msg::tool_result("c1", "bash", "x".repeat(6_000));
    let ctx = build_restored_branch_context(&[Msg::user("run"), big_tool]);
    assert!(ctx.contains("<restored_branch_context>"));
    assert!(ctx.contains("[truncated"));

    let cancel = CancelFlag::new();
    let fresh = vec![Msg::system("sys rule"), Msg::user("hi")];
    let (prompt, strat) = prepare_bootstrap_prompt(&fresh, 12_000, "smol", None, &cancel).await?;
    assert_eq!(strat, BootstrapStrategy::Fresh);
    assert!(prompt.contains("sys rule"));
    assert!(prompt.contains("hi"));

    let small_hist = vec![
        Msg::user("step 1"),
        Msg::assistant("done 1"),
        Msg::user("step 2"),
    ];
    let sum = MockSummarizer {
        calls: AtomicUsize::new(0),
        fail: false,
    };
    let (p_dir, s_dir) =
        prepare_bootstrap_prompt(&small_hist, 12_000, "smol", Some(&sum), &cancel).await?;
    assert_eq!(s_dir, BootstrapStrategy::DirectTranscript);
    assert_eq!(sum.calls.load(Ordering::SeqCst), 0);
    assert!(p_dir.contains("<restored_branch_context>"));

    // Above threshold -> Smart Handoff invokes summarizer with `handoff_model`.
    let big_hist = vec![
        Msg::user("a".repeat(2_000)),
        Msg::assistant("b".repeat(2_000)),
        Msg::user("continue now"),
    ];
    assert!(estimate_messages_tokens(&big_hist) > 200);
    let (p_smart, s_smart) =
        prepare_bootstrap_prompt(&big_hist, 200, "smol", Some(&sum), &cancel).await?;
    assert_eq!(s_smart, BootstrapStrategy::SmartHandoff);
    assert_eq!(sum.calls.load(Ordering::SeqCst), 1);
    assert!(p_smart.contains("CHECKPOINT_BY_smol"));
    assert!(p_smart.contains("continue now"));

    // Summarizer failure degrades gracefully to DirectTranscript.
    let failing = MockSummarizer {
        calls: AtomicUsize::new(0),
        fail: true,
    };
    let (_, s_fallback) =
        prepare_bootstrap_prompt(&big_hist, 200, "smol", Some(&failing), &cancel).await?;
    assert_eq!(s_fallback, BootstrapStrategy::DirectTranscript);
    Ok(())
}

#[tokio::test]
async fn wire_framing_and_permission_isolation() -> TestResult {
    let mut reader = tokio::io::BufReader::new(&b"{\"ok\":true}\n"[..]);
    assert_eq!(
        read_bounded_line(&mut reader, 64).await?,
        Some("{\"ok\":true}".to_owned())
    );
    assert_eq!(read_bounded_line(&mut reader, 64).await?, None);

    let oversize = format!("{}\n", "a".repeat(128));
    let mut reader_big = tokio::io::BufReader::new(oversize.as_bytes());
    assert!(read_bounded_line(&mut reader_big, 32).await.is_err());

    let bridge_perm = AcpPermissionRequest {
        rpc_id: json!(1),
        session_id: "s".to_owned(),
        tool_name: "hrn__read".to_owned(),
        server_name: Some("hrn-bridge".to_owned()),
        allow_option_id: Some("allow_once".to_owned()),
        reject_option_id: Some("reject_once".to_owned()),
    };
    assert!(is_hrn_bridge_permission(&bridge_perm));
    let allow_reply = build_permission_reply(&bridge_perm, AgyToolMode::Hrn);
    assert_eq!(
        allow_reply
            .pointer("/result/outcome/optionId")
            .and_then(serde_json::Value::as_str),
        Some("allow_once")
    );

    let native_perm = AcpPermissionRequest {
        rpc_id: json!(2),
        session_id: "s".to_owned(),
        tool_name: "run_command".to_owned(),
        server_name: None,
        allow_option_id: Some("allow_once".to_owned()),
        reject_option_id: Some("reject_once".to_owned()),
    };
    assert!(!is_hrn_bridge_permission(&native_perm));
    let spoofed_shell = AcpPermissionRequest {
        rpc_id: json!(3),
        session_id: "s".to_owned(),
        tool_name: "Run echo hrn-bridge".to_owned(),
        server_name: None,
        allow_option_id: Some("allow_once".to_owned()),
        reject_option_id: Some("reject_once".to_owned()),
    };
    assert!(!is_hrn_bridge_permission(&spoofed_shell));
    let eager_mcp = AcpPermissionRequest {
        rpc_id: json!(4),
        session_id: "s".to_owned(),
        tool_name: "mcp_hrn-bridge_hrn__read".to_owned(),
        server_name: None,
        allow_option_id: Some("allow_once".to_owned()),
        reject_option_id: Some("reject_once".to_owned()),
    };
    assert!(is_hrn_bridge_permission(&eager_mcp));
    let deny_reply = build_permission_reply(&native_perm, AgyToolMode::Hrn);
    assert_eq!(
        deny_reply
            .pointer("/result/outcome/optionId")
            .and_then(serde_json::Value::as_str),
        Some("reject_once")
    );
    let native_allow = build_permission_reply(&native_perm, AgyToolMode::Native);
    assert_eq!(
        native_allow
            .pointer("/result/outcome/optionId")
            .and_then(serde_json::Value::as_str),
        Some("allow_once")
    );
    Ok(())
}

#[test]
fn error_quota_classification_and_wire_parsers() -> TestResult {
    // Auth & quota classification.
    assert!(matches!(
        classify_jsonrpc_error(-32000, "Authentication required", None),
        HrnError::Auth(_)
    ));
    let quota_rpc = classify_jsonrpc_error(
        -32001,
        "RESOURCE_EXHAUSTED: 429 too many requests",
        Some(&json!({ "retry_after_secs": 45 })),
    );
    assert!(quota_rpc.is_retryable());
    assert_eq!(quota_rpc.retry_after_secs(), Some(45));

    // Plain-text Usage Limit Reached + end_turn + 0 tokens quirk.
    let now = 1_796_119_200_u64;
    let res = AcpPromptResult {
        stop_reason: "end_turn".to_owned(),
        usage: Some(Usage::default()),
        quota: Some(InlineQuota {
            reset_time: Some("2026-12-01T10:05:00Z".to_owned()),
            remaining_fraction: Some(0.0),
            retry_after_secs: None,
        }),
    };
    let detected = detect_quota_exhaustion(
        &res,
        "Usage Limit Reached. You have reached your current quota.",
        now,
    )
    .ok_or("expected quota detection")?;
    assert!(detected.is_retryable());
    assert_eq!(detected.retry_after_secs(), Some(300));
    assert_eq!(
        retry_after_from_quota(
            &InlineQuota {
                reset_time: Some("1970-01-01T00:00:00Z".to_owned()),
                remaining_fraction: None,
                retry_after_secs: None,
            },
            now
        ),
        None
    );

    assert_eq!(thinking_level_to_acp(ThinkingLevel::Off), "low");
    assert_eq!(thinking_level_to_acp(ThinkingLevel::Medium), "medium");
    assert_eq!(thinking_level_to_acp(ThinkingLevel::High), "high");
    assert_eq!(thinking_level_to_acp(ThinkingLevel::Max), "xhigh");

    let caps = parse_capabilities(&json!({
        "protocolVersion": 1,
        "agentInfo": { "name": "Antigravity", "version": "1.5.18" },
        "agentCapabilities": {
            "loadSession": true,
            "sessionCapabilities": { "resume": {}, "close": {}, "fork": {} },
            "promptCapabilities": { "image": true }
        }
    }));
    assert!(
        caps.supports_resume && caps.supports_close && caps.supports_fork && caps.supports_image
    );

    let sn = parse_session_new(&json!({
        "sessionId": "sid-1",
        "configOptions": [{
            "category": "model",
            "currentValue": "gemini-3.8-flash",
            "options": [{ "value": "gemini-3.8-flash", "name": "Gemini 3.8 Flash" }]
        }],
        "_meta": { "antigravity": { "tier": { "name": "Google AI Ultra" } } }
    }))
    .ok_or("session/new parse failed")?;
    assert_eq!(sn.session_id, "sid-1");
    assert_eq!(sn.tier_name.as_deref(), Some("Google AI Ultra"));
    assert_eq!(sn.available_models.len(), 1);
    Ok(())
}

fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("hrn-agy-unit-{label}-{}-{n}", std::process::id()))
}
