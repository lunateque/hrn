use super::*;
use hrn_edge_tools::{FileReadTracker, read_sync};
use hrn_kernel::{
    AgentEvent, CancelFlag, EventSink, HelpPages, PermissionMode, ToolContext, UrlBody,
};
use std::path::PathBuf;

struct NoopSink;
impl EventSink for NoopSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

fn test_ctx(cwd: PathBuf, urls: Arc<UrlRouter>) -> ToolContext {
    ToolContext {
        session_id: "sess-1".to_owned(),
        turn_id: "t1".to_owned(),
        call_id: "c1".to_owned(),
        cwd,
        permission_mode: PermissionMode::Default,
        cancel: CancelFlag::new(),
        events: Arc::new(NoopSink),
        ask: None,
        approval: None,
        urls,
        root_session_id: "sess-1".to_owned(),
        inbox: Arc::default(),
        caller: None,
        todos: Arc::default(),
        yield_data: Arc::default(),
        is_subagent: false,
    }
}

struct ShadowAttempt(&'static str);

impl UrlScheme for ShadowAttempt {
    fn scheme(&self) -> &'static str {
        self.0
    }

    fn about(&self) -> &'static str {
        "shadow attempt"
    }

    fn read(&self, _ctx: &ToolContext, _rest: &str) -> Result<UrlBody> {
        Ok(UrlBody::Text("shadowed".to_owned()))
    }
}

fn test_urls(
    data_dir: PathBuf,
    module_schemes: Vec<Arc<dyn UrlScheme>>,
    out_enabled: bool,
) -> Result<UrlRouter> {
    let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
    let bg = Background::new();
    let procs = Arc::new(ProcTable::new(Vec::new()));
    let grants = hrn_harness::GrantCache::memory();
    assemble_urls(
        store,
        data_dir,
        &bg,
        &procs,
        HelpPages::default(),
        module_schemes,
        &grants,
        out_enabled,
    )
}

#[test]
fn scheme_shadowing_rejected() {
    let dir = std::env::temp_dir().join(format!("hrn-shadow-test-{}", std::process::id()));
    for builtin in [
        "out",
        "OUT",
        "ssh",
        "mcp",
        "attachment",
        "history",
        "skill",
        "rule",
        "agent",
        "proc",
        "help",
    ] {
        let err = test_urls(dir.clone(), vec![Arc::new(ShadowAttempt(builtin))], true)
            .err()
            .unwrap_or_else(|| panic!("shadowing `{builtin}` must fail"))
            .to_string();
        assert!(err.contains("built-in"), "{builtin}: {err}");
    }
    // `out://` remains reserved even when `[output] out_enabled = false`.
    let err_disabled = test_urls(dir.clone(), vec![Arc::new(ShadowAttempt("out"))], false)
        .err()
        .expect("shadowing disabled built-in `out` must fail")
        .to_string();
    assert!(err_disabled.contains("built-in"), "{err_disabled}");
    // Non-built-in module schemes register cleanly.
    let ok = test_urls(dir, vec![Arc::new(ShadowAttempt("custom"))], false)
        .expect("non-builtin module scheme registers");
    assert!(ok.reference().unwrap_or_default().contains("`custom://`"));
}

#[test]
fn out_scheme_registered_only_when_enabled() {
    let dir = std::env::temp_dir().join(format!("hrn-out-flag-{}", std::process::id()));
    let disabled = test_urls(dir.clone(), Vec::new(), false).expect("disabled urls");
    assert!(
        !disabled
            .reference()
            .unwrap_or_default()
            .contains("`out://`")
    );
    let enabled = test_urls(dir, Vec::new(), true).expect("enabled urls");
    assert!(
        enabled
            .reference()
            .expect("reference present")
            .contains("`out://`")
    );
}

#[test]
fn attachment_scheme_end_to_end_read_and_query() {
    let dir = std::env::temp_dir().join(format!("hrn-att-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = DiskAttachmentStore::new(dir.clone());
    let key = store
        .store_bytes("sess-1", "data.json", br#"{"items":["alpha","beta"]}"#)
        .expect("store");
    assert_eq!(key, "1-data.json");

    let urls = Arc::new(test_urls(dir.clone(), Vec::new(), false).expect("urls"));
    let ctx = test_ctx(dir.clone(), urls);

    let tracker = FileReadTracker::new();
    let listed = read_sync(&tracker, &ctx, "attachment://").expect("list");
    assert!(
        listed.content.contains("attachment://1-data.json"),
        "{}",
        listed.content
    );

    let queried = read_sync(&tracker, &ctx, "attachment://1-data.json?q=.items[1]").expect("jq");
    assert!(queried.content.contains("beta"), "{}", queried.content);

    let err = read_sync(&tracker, &ctx, "attachment://../escape.txt")
        .expect_err("traversal refused")
        .to_string();
    assert!(
        err.contains("traversal") || err.contains("forbidden"),
        "{err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reapply_discovered_preserves_agy_and_respects_disabled_providers() {
    let discovered = hrn_edge_llm::agy_acp::DiscoveredAgyModels {
        models: vec![hrn_edge_llm::agy_acp::AcpModelOption {
            value: "claude-sonnet-4-6".to_owned(),
            name: "Claude Sonnet 4.6".to_owned(),
        }],
        current_model: Some("claude-sonnet-4-6".to_owned()),
        tier_name: None,
        agent_version: "1.5.18".to_owned(),
    };
    let cat =
        hrn_edge_llm::build_agy_catalog("agy", &hrn_edge_llm::default_agy_provider(), &discovered)
            .expect("agy catalog");

    let bg = Background::new();
    bg.set_discovered(vec![cat]);

    let base_toml = r#"
[[providers]]
name = "stub"
kind = "openai_chat"
base_url = "https://example.invalid/v1"

[models."stub/m1"]
provider = "stub"
id = "m1"
context_window = 128000

[roles]
default = "stub/m1"
"#;

    let mut cfg = Config::parse_with_home(base_toml, None).expect("base config");
    bg.reapply_discovered(&mut cfg);
    assert!(cfg.llm.providers.contains_key("agy"));
    assert!(cfg.llm.presets.contains_key("agy/claude-sonnet-4-6"));
    assert_eq!(
        cfg.llm.roles.get("agy").map(String::as_str),
        Some("agy/claude-sonnet-4-6")
    );
    assert_eq!(
        cfg.llm.roles.get("default").map(String::as_str),
        Some("stub/m1")
    );

    let mut disabled = Config::parse_with_home(base_toml, None).expect("disabled config");
    disabled.llm.disabled_providers.push("agy".to_owned());
    bg.reapply_discovered(&mut disabled);
    assert!(!disabled.llm.providers.contains_key("agy"));
    assert!(!disabled.llm.presets.contains_key("agy/claude-sonnet-4-6"));
    assert!(!disabled.llm.roles.contains_key("agy"));
}
