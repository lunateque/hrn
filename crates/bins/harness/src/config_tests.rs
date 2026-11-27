use super::*;

const GOOD: &str = r#"
thinking = "high"

[[providers]]
name = "gateway"
kind = "openai_chat"
base_url = "https://api.example.com/v1"
api_key_file = "C:/tokens/gateway.token"

[models.flash]
provider = "gateway"
id = "example-flash"
context_window = 262144

[roles]
default = "flash"
"#;

#[test]
fn parses_a_complete_file() {
    let cfg = Config::parse(GOOD).expect("valid");
    assert_eq!(cfg.thinking, ThinkingLevel::High);
    let preset = cfg.llm.presets.get("flash").expect("flash");
    assert_eq!(preset.id, "example-flash");
    assert_eq!(preset.context_window, Some(262_144));
    assert_eq!(
        cfg.llm.roles.get("default").map(String::as_str),
        Some("flash")
    );
    assert!(!cfg.llm.roles.contains_key("fast"));
}

#[test]
fn parses_oauth_roles_fallback_and_cycle() {
    let cfg = Config::parse(
        r#"
cycle = ["primary", "fast_m"]

[[providers]]
name = "oidc"
kind = "openai_responses"
base_url = "https://api.openai.com/v1"
strip_temperature = true
[providers.oauth]
authorize_url = "https://auth.example.com/authorize"
token_url = "https://auth.example.com/oauth/token"
client_id = "my-custom-app"
scope = "openid offline_access"

[models.primary]
provider = "oidc"
id = "gpt-5"
context_window = 200000
thinking = "high"

[models.fast_m]
provider = "oidc"
id = "gpt-5-mini"
context_window = 128000
thinking = "low"

[roles]
default = "primary"
fast = "fast_m"
plan = "primary"
reviewer = "primary"

[fallback]
primary = ["fast_m"]
"#,
    )
    .expect("valid oauth config");
    let prov = cfg.llm.providers.get("oidc").expect("oidc");
    assert_eq!(prov.auth_mode, AuthMode::OauthJson);
    assert!(prov.strip_temperature);
    assert!(cfg.oauth_logins.contains_key("oidc"));
    assert_eq!(
        cfg.llm.fallback_chains.get("primary"),
        Some(&vec!["fast_m".to_owned()])
    );
    assert_eq!(cfg.llm.cycle_order, vec!["primary", "fast_m"]);
}

#[test]
fn oauth_client_guard_allows_custom_clients_and_blocks_google() {
    let with_client = |client_id: &str| {
        format!(
            r#"
[[providers]]
name = "p"
kind = "anthropic"
[providers.oauth]
authorize_url = "https://auth.example.com/authorize"
token_url = "https://auth.example.com/oauth/token"
client_id = "{client_id}"

[models.m]
provider = "p"
id = "claude-opus-5-5"
context_window = 200000

[roles]
default = "m"
"#
        )
    };
    for allowed in ["my-own-oidc-client", "custom-enterprise-app"] {
        let parsed = Config::parse(&with_client(allowed));
        assert!(parsed.is_ok(), "{allowed}: {:?}", parsed.err());
    }
    for blocked in [
        "681255809395-x.apps.googleusercontent.com",
        "1071006060591-y.apps.googleusercontent.com",
    ] {
        let err = Config::parse(&with_client(blocked)).expect_err("Google client must fail");
        assert!(err.to_string().contains("ToS guard"), "{err}");
    }
}

#[test]
fn rejects_unknown_fields_and_dangling_references() {
    let err = |text: &str| Config::parse(text).expect_err("must fail").to_string();
    assert!(err(&GOOD.replace("kind =", "knd =")).contains("knd"));
    assert!(err(&GOOD.replace("default = \"flash\"", "default = \"nope\"")).contains("nope"));
    assert!(err(&GOOD.replace("provider = \"gateway\"", "provider = \"x\"")).contains('x'));
    assert!(err(&GOOD.replace("high", "extreme")).contains("extreme"));
    assert!(err(&GOOD.replace("[roles]\ndefault = \"flash\"", "")).contains("[roles]"));
    assert!(err(&GOOD.replace("262144", "0")).contains("context_window"));
}

#[test]
fn key_sources_are_validated() {
    let both = GOOD.replace("api_key_file", "api_key_env = \"K\"\napi_key_file");
    assert!(Config::parse(&both).is_err());
    let hosted_without_key = GOOD
        .replace("openai_chat", "anthropic")
        .replace("api_key_file = \"C:/tokens/gateway.token\"", "");
    assert!(Config::parse(&hosted_without_key).is_err());
    let keyless_local = GOOD.replace("api_key_file = \"C:/tokens/gateway.token\"", "");
    assert!(Config::parse(&keyless_local).is_ok());
}

#[test]
fn parses_mcp_stdio_and_http_entries() {
    let toml = format!(
        "{GOOD}\n\
[[mcp]]\n\
name = \"local\"\n\
command = \"npx\"\n\
args = [\"-y\", \"demo-mcp\"]\n\
read_only = true\n\
\n\
[[mcp]]\n\
name = \"remote\"\n\
url = \"https://mcp.example.com\"\n\
auto_approve = false\n"
    );
    let cfg = Config::parse(&toml).expect("valid mcp");
    assert_eq!(cfg.mcp_servers.len(), 2);
    let local = cfg.mcp_servers.first().expect("local");
    assert!(local.auto_approve);
    assert!(local.read_only);
    let remote = cfg.mcp_servers.get(1).expect("remote");
    assert!(!remote.auto_approve);
}

#[test]
fn module_sections_are_kept_raw_for_their_module() {
    let toml = format!("{GOOD}\n[web]\nchain = [\"exa\"]\n[fs]\nedit_mode = \"replace\"\n");
    let cfg = Config::parse(&toml).expect("module sections parse");
    assert_eq!(
        cfg.module_tables
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["fs", "web"]
    );
    assert!(Config::parse(GOOD).expect("none").module_tables.is_empty());
}

#[test]
fn ask_timeout_is_off_by_default_and_typed() {
    assert_eq!(Config::parse(GOOD).unwrap().ask_timeout, None);
    let toml = format!("{GOOD}\n[tools]\nask_timeout_secs = 45\n");
    assert_eq!(
        Config::parse(&toml).unwrap().ask_timeout,
        Some(std::time::Duration::from_secs(45))
    );
    let toml = format!("{GOOD}\n[tools]\nask_timeout_secs = \"soon\"\n");
    assert!(Config::parse(&toml).is_err());
}

#[test]
fn the_tui_theme_is_optional_and_typed() {
    let default_cfg = Config::parse(GOOD).unwrap();
    assert_eq!(default_cfg.appearance.theme.dark, "titanium");
    assert_eq!(default_cfg.appearance.theme.light, "titanium-light");
    assert_eq!(
        default_cfg.appearance.theme.current,
        hrn_tui::ThemeMode::Auto
    );
    assert_eq!(
        default_cfg.appearance.symbol_preset,
        hrn_tui::SymbolPreset::Unicode
    );
    assert!(!default_cfg.appearance.color_blind_mode);

    // Legacy `[tui] theme = "x"` maps cleanly to both dark and light slots.
    let legacy = format!("{GOOD}\n[tui]\ntheme = \"titanium-vivid\"\n");
    let legacy_cfg = Config::parse(&legacy).unwrap();
    assert_eq!(legacy_cfg.appearance.theme.dark, "titanium-vivid");
    assert_eq!(legacy_cfg.appearance.theme.light, "titanium-vivid");

    // Structured `[tui.theme]` and `[tui.statusline]` parse cleanly.
    let structured = format!(
        "{GOOD}\n\
         [tui]\n\
         symbol_preset = \"ascii\"\n\
         color_blind_mode = true\n\
         vim_mode = true\n\
         vim_mode_display = \"icon\"\n\
         recall_cleared_drafts = false\n\
         [tui.theme]\n\
         dark = \"titanium-vivid\"\n\
         light = \"titanium-light\"\n\
         current = \"light\"\n\
         [tui.statusline]\n\
         preset = \"custom\"\n\
         separator = \"pipe\"\n\
         left_segments = [\"brand\", \"model\"]\n\
         right_segments = [\"title\"]\n\
         session_accent = false\n\
         transparent = true\n\
         show_hook_status = true\n"
    );
    let s_cfg = Config::parse(&structured).expect("structured tui config");
    assert_eq!(s_cfg.appearance.theme.dark, "titanium-vivid");
    assert_eq!(s_cfg.appearance.theme.light, "titanium-light");
    assert_eq!(s_cfg.appearance.theme.current, hrn_tui::ThemeMode::Light);
    assert_eq!(s_cfg.appearance.symbol_preset, hrn_tui::SymbolPreset::Ascii);
    assert!(s_cfg.appearance.color_blind_mode);
    assert_eq!(
        s_cfg.appearance.statusline.preset,
        hrn_tui::StatuslinePreset::Custom
    );
    assert_eq!(
        s_cfg.appearance.statusline.separator,
        hrn_tui::SeparatorStyle::Pipe
    );
    assert_eq!(
        s_cfg.appearance.statusline.left_segments,
        ["brand", "model"]
    );
    assert_eq!(s_cfg.appearance.statusline.right_segments, ["title"]);
    assert!(!s_cfg.appearance.statusline.session_accent);
    assert!(s_cfg.appearance.statusline.transparent);
    assert!(s_cfg.appearance.statusline.show_hook_status);
    assert!(s_cfg.appearance.vim_mode);
    assert_eq!(
        s_cfg.appearance.vim_mode_display,
        hrn_tui::VimModeDisplay::Icon
    );
    assert!(!s_cfg.appearance.recall_cleared_drafts);

    let toml = format!("{GOOD}\n[tui]\nvim_mode_display = \"bogus\"\n");
    assert!(Config::parse(&toml).is_err());

    let toml = format!("{GOOD}\n[tui]\ntheme = 3\n");
    assert!(Config::parse(&toml).is_err());
    let toml = format!("{GOOD}\n[tui]\ncolors = \"x\"\n");
    assert!(Config::parse(&toml).is_err());
    let toml = format!("{GOOD}\n[tui]\nsymbol_preset = \"emoji\"\n");
    assert!(Config::parse(&toml).is_err());
    let toml = format!("{GOOD}\n[tui.theme]\ncurrent = \"midnight\"\n");
    assert!(Config::parse(&toml).is_err());
    let toml = format!("{GOOD}\n[tui.statusline]\npreset = \"bogus\"\n");
    assert!(Config::parse(&toml).is_err());
    let toml = format!("{GOOD}\n[tui.statusline]\nseparator = \"bogus\"\n");
    assert!(Config::parse(&toml).is_err());
}

#[test]
fn mcp_tool_budget_defaults_and_is_typed() {
    assert_eq!(Config::parse(GOOD).expect("default").mcp_tool_budget, 4_000);
    let toml = format!("{GOOD}\n[mcp_tools]\nbudget_tokens = 0\n");
    assert_eq!(Config::parse(&toml).expect("zero").mcp_tool_budget, 0);
    let toml = format!("{GOOD}\n[mcp_tools]\nbudget = 5\n");
    assert!(Config::parse(&toml).is_err(), "unknown fields are rejected");
}

#[test]
fn model_prices_are_typed_and_validated() {
    let toml = GOOD.replace(
        "context_window = 262144",
        "context_window = 262144\nprice = { input = 3, output = 15, cache_read = 0.3 }",
    );
    let cfg = Config::parse(&toml).expect("priced");
    let price = cfg.llm.presets["flash"].price.expect("price");
    assert_eq!(
        (price.input, price.cache_read, price.cache_write),
        (3.0, Some(0.3), None)
    );
    assert!(
        Config::parse(GOOD).expect("plain").llm.presets["flash"]
            .price
            .is_none()
    );

    let negative = GOOD.replace(
        "context_window = 262144",
        "context_window = 262144\nprice = { input = -1, output = 1 }",
    );
    assert!(Config::parse(&negative).is_err(), "negative price");
    let unknown = GOOD.replace(
        "context_window = 262144",
        "context_window = 262144\nprice = { input = 1, output = 1, tip = 2 }",
    );
    assert!(Config::parse(&unknown).is_err(), "unknown price field");
}

#[test]
fn a_subscription_login_takes_no_price() {
    let toml = r#"
[[providers]]
name = "sub"
kind = "anthropic"
auth_mode = "claude"

[models.opus]
provider = "sub"
id = "claude-opus-4-6"
context_window = 200000
price = { input = 15, output = 75 }

[roles]
default = "opus"
"#;
    let err = Config::parse(toml).expect_err("rejected").to_string();
    assert!(err.contains("no per-token billing"), "{err}");
}

#[test]
fn approval_mode_and_policy_parse() {
    let default_cfg = Config::parse(GOOD).expect("default");
    assert_eq!(default_cfg.approval_mode, hrn_kernel::PermissionMode::Yolo);
    assert!(default_cfg.approval_policy.restricted_folders.is_empty());
    assert!(default_cfg.approval_policy.tool_policies.is_empty());

    let toml = format!(
        "{GOOD}\n\
[tools]\n\
approval_mode = \"always-ask-cached\"\n\
restricted_folders = [\"~/.ssh\", \"/etc\"]\n\
\n\
[tools.approval]\n\
bash = \"deny\"\n\
mcp__gh_create_issue = \"prompt\"\n\
read = \"allow\"\n"
    );
    let cfg = Config::parse(&toml).expect("custom approval config");
    assert_eq!(cfg.approval_mode, hrn_kernel::PermissionMode::AskCached);
    assert_eq!(cfg.approval_policy.restricted_folders, ["~/.ssh", "/etc"]);
    assert_eq!(
        cfg.approval_policy.tool_policies.get("bash"),
        Some(&hrn_kernel::ToolApprovalPolicy::Deny)
    );
    assert_eq!(
        cfg.approval_policy
            .tool_policies
            .get("mcp__gh_create_issue"),
        Some(&hrn_kernel::ToolApprovalPolicy::Prompt)
    );
    assert_eq!(
        cfg.approval_policy.tool_policies.get("read"),
        Some(&hrn_kernel::ToolApprovalPolicy::Allow)
    );
}

#[test]
fn example_toml_parses_and_matches_builtin_catalog() {
    let example = include_str!("../../../../hrn.example.toml");
    let cfg = Config::parse(example).expect("hrn.example.toml must parse cleanly");
    assert_eq!(cfg.llm.presets["opus"].id, "claude-opus-4-6");
    assert_eq!(cfg.llm.presets["opus"].context_window, Some(1_000_000));
    assert_eq!(cfg.llm.presets["codex"].id, "gpt-6.2-sol");
    assert_eq!(cfg.llm.presets["codex"].context_window, Some(500_000));
    assert_eq!(cfg.llm.presets["flash"].id, "gemini-2.5-flash");

    let auth = std::env::temp_dir().join(format!("hrn-example-cat-{}", std::process::id()));
    let home = auth.join("home");
    std::fs::create_dir_all(&auth).expect("dir");
    std::fs::write(
        auth.join(hrn_edge_oauth::Subscription::Claude.file_name()),
        "{}",
    )
    .expect("claude");
    std::fs::write(
        auth.join(hrn_edge_oauth::Subscription::Chatgpt.file_name()),
        "{}",
    )
    .expect("chatgpt");
    let no_env = |_: &str| None;
    let catalogs = hrn_edge_llm::standard_catalogs_with_env(&home, &auth, Some(&no_env));
    let mut builtin = LlmConfig::default();
    builtin.absorb_all(catalogs);
    std::fs::remove_dir_all(&auth).ok();

    let opus_builtin = builtin
        .presets
        .get("anthropic/claude-opus-4-6")
        .expect("opus example id must exist in built-in models.yml");
    assert_eq!(
        cfg.llm.presets["opus"].context_window,
        opus_builtin.context_window
    );
    let codex_builtin = builtin
        .presets
        .get("openai-codex/gpt-6.2-sol")
        .expect("codex example id must exist in built-in models.yml");
    assert_eq!(
        cfg.llm.presets["codex"].context_window,
        codex_builtin.context_window
    );
}
