use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempFixture {
    root: PathBuf,
    omp: PathBuf,
    hrn: PathBuf,
    cfg: PathBuf,
}

impl TempFixture {
    fn new(tag: &str) -> Self {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("hrn-omp-import-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let omp = root.join("omp-agent");
        let hrn = root.join("hrn-agent");
        let cfg = hrn.join("config.toml");
        std::fs::create_dir_all(&omp).expect("create omp dir");
        std::fs::create_dir_all(&hrn).expect("create hrn dir");
        Self {
            root,
            omp,
            hrn,
            cfg,
        }
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const SAMPLE_MODELS_YML: &str = "\
providers:
  overrides_only:
    modelOverrides:
      x: { compactionModel: y }
  gw:
    baseUrl: https://gw.example/v1
    apiKey: '!powershell -Command \"(Get-Content -Raw ''C:/Users/alice/.omp/agent/auth-gateway-tokens/gw.token'').Trim()\"'
    api: openai-completions
    models:
      - id: example/model-pro
        name: Example Pro
        contextWindow: 131072
        cost: { input: 1.5, output: 3 }
  envprov:
    baseUrl: https://api.example.com/v1
    apiKey: ${EXAMPLE_API_KEY}
    api: openai-chat
    models:
      - id: m1
  rawprov:
    baseUrl: https://raw.example/v1
    apiKey: sk-super-secret-plaintext-value
    api: openai-responses
    models:
      - id: r1
  claude_sub:
    api: anthropic-messages
    auth: oauth
    models:
      - id: claude-sonnet-4
  blocked_google:
    api: google-gemini-cli
    auth: oauth
    models:
      - id: gemini-2.5-pro
  blocked_ag:
    api: google-antigravity
    auth: oauth
    models:
      - id: gemini-2.5-pro
modelTags:
  default:
    name: Daily
    color: cyan
";

const SAMPLE_CONFIG_YML: &str = "\
defaultThinkingLevel: high
modelRoles:
  default: envprov/m1
  fast: gw/example/model-pro
cycleOrder:
  - default
  - fast
retry:
  fallbackChains:
    default: [fast, rawprov/r1]
modelPresets:
  coding:
    modelRoles:
      default: envprov/m1
    defaultThinkingLevel: xhigh
compaction:
  thresholdPercent: 75
";

#[test]
fn dry_run_writes_zero_files_and_reports_planned_changes() {
    let fx = TempFixture::new("dry");
    std::fs::create_dir_all(fx.omp.join("auth-gateway-tokens")).expect("tok dir");
    std::fs::write(fx.omp.join("auth-gateway-tokens/gw.token"), "tok-gw-123\n").expect("tok");
    std::fs::write(fx.omp.join(".env"), "EXAMPLE_API_KEY=env-secret-999\n").expect("env");
    std::fs::write(fx.omp.join("models.yml"), SAMPLE_MODELS_YML).expect("models");
    std::fs::write(fx.omp.join("config.yml"), SAMPLE_CONFIG_YML).expect("config");

    let report = import_from_dir(&fx.omp, &fx.hrn, &fx.cfg, true, false).expect("dry run");
    assert!(report.dry_run);
    assert_eq!(
        report.providers_imported,
        ["gw", "envprov", "rawprov", "claude_sub"]
    );
    assert_eq!(report.oauth_providers, ["claude_sub"]);
    assert_eq!(report.tokens_imported, ["gw.token"]);
    assert_eq!(report.env_imported, ["EXAMPLE_API_KEY", "RAWPROV_API_KEY"]);
    assert!(
        report
            .providers_skipped
            .iter()
            .any(|(n, _)| n == "blocked_google")
    );
    assert!(
        report
            .providers_skipped
            .iter()
            .any(|(n, _)| n == "blocked_ag")
    );
    assert!(
        report
            .providers_skipped
            .iter()
            .any(|(n, _)| n == "overrides_only")
    );
    assert!(!fx.hrn.join("models.yml").exists());
    assert!(!fx.hrn.join(".env").exists());
    assert!(!fx.hrn.join("auth-gateway-tokens/gw.token").exists());
    assert!(!fx.cfg.exists());
}

#[test]
fn import_merges_files_strips_plaintext_keys_and_preserves_comments() {
    let fx = TempFixture::new("merge");
    std::fs::create_dir_all(fx.omp.join("auth-gateway-tokens")).expect("tok dir");
    std::fs::write(fx.omp.join("auth-gateway-tokens/gw.token"), "tok-gw-123\n").expect("tok");
    std::fs::write(fx.omp.join(".env"), "EXAMPLE_API_KEY=env-secret-999\n").expect("env");
    std::fs::write(fx.omp.join("models.yml"), SAMPLE_MODELS_YML).expect("models");
    std::fs::write(fx.omp.join("config.yml"), SAMPLE_CONFIG_YML).expect("config");
    std::fs::write(
        &fx.cfg,
        "# Keep my comment\nthinking = \"medium\"\n\n[roles]\nfast = \"keep/fast\"\n",
    )
    .expect("seed config");

    let report = import_from_dir(&fx.omp, &fx.hrn, &fx.cfg, false, false).expect("import");
    assert_eq!(
        report.providers_imported,
        ["gw", "envprov", "rawprov", "claude_sub"]
    );

    let models_out = std::fs::read_to_string(fx.hrn.join("models.yml")).expect("models out");
    assert!(
        !models_out.contains("sk-super-secret-plaintext-value"),
        "plaintext key leaked into models.yml:\n{models_out}"
    );
    assert!(models_out.contains("RAWPROV_API_KEY"), "{models_out}");
    assert!(models_out.contains("EXAMPLE_API_KEY"), "{models_out}");
    assert!(
        models_out.contains("!cat 'auth-gateway-tokens/gw.token'"),
        "{models_out}"
    );
    assert!(!models_out.contains("blocked_google"), "{models_out}");
    assert!(!models_out.contains("blocked_ag"), "{models_out}");
    // Must round-trip cleanly through hrn's YAML parser.
    let reparsed = parse_yaml(&models_out).expect("reparse emitted models.yml");
    assert_eq!(
        reparsed
            .get("providers")
            .and_then(|p| p.get("gw"))
            .and_then(|p| p.get("apiKey"))
            .and_then(Yaml::str),
        Some("!cat 'auth-gateway-tokens/gw.token'")
    );
    assert_eq!(
        reparsed
            .get("providers")
            .and_then(|p| p.get("rawprov"))
            .and_then(|p| p.get("apiKey"))
            .and_then(Yaml::str),
        Some("RAWPROV_API_KEY")
    );
    assert_eq!(
        reparsed
            .get("modelTags")
            .and_then(|t| t.get("default"))
            .and_then(|t| t.get("name"))
            .and_then(Yaml::str),
        Some("Daily")
    );

    let env_out = std::fs::read_to_string(fx.hrn.join(".env")).expect("env out");
    let env_map = parse_dotenv_map(&env_out);
    assert_eq!(
        env_map.get("RAWPROV_API_KEY").map(String::as_str),
        Some("sk-super-secret-plaintext-value")
    );
    assert_eq!(
        env_map.get("EXAMPLE_API_KEY").map(String::as_str),
        Some("env-secret-999")
    );

    let tok_out =
        std::fs::read_to_string(fx.hrn.join("auth-gateway-tokens/gw.token")).expect("tok out");
    assert_eq!(tok_out, "tok-gw-123\n");

    let cfg_out = std::fs::read_to_string(&fx.cfg).expect("cfg out");
    assert!(cfg_out.contains("# Keep my comment"), "{cfg_out}");
    assert!(cfg_out.contains("thinking = \"medium\""), "{cfg_out}");
    assert!(cfg_out.contains("fast = \"keep/fast\""), "{cfg_out}");
    assert!(cfg_out.contains("default = \"envprov/m1\""), "{cfg_out}");
    assert!(
        cfg_out.contains("cycle = [\"default\", \"fast\"]"),
        "{cfg_out}"
    );
    assert!(cfg_out.contains("[modelPresets.coding"), "{cfg_out}");
    assert!(
        !cfg_out.contains("sk-super-secret-plaintext-value"),
        "plaintext key leaked into config.toml"
    );

    // Re-running with --force overwrites previously preserved keys.
    let forced = import_from_dir(&fx.omp, &fx.hrn, &fx.cfg, false, true).expect("force import");
    assert!(forced.config_imported.contains(&"thinking".to_owned()));
    assert!(forced.config_imported.contains(&"roles.fast".to_owned()));
    let cfg_forced = std::fs::read_to_string(&fx.cfg).expect("cfg forced");
    assert!(cfg_forced.contains("thinking = \"high\""), "{cfg_forced}");
    assert!(
        cfg_forced.contains("fast = \"gw/example/model-pro\""),
        "{cfg_forced}"
    );
}

#[test]
fn refuses_forbidden_consumer_cli_roots() {
    let fx = TempFixture::new("forbid");
    for name in [".gemini", ".claude", ".codex"] {
        let dir = fx.root.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let err = import_from_dir(&dir, &fx.hrn, &fx.cfg, true, false)
            .expect_err("forbidden root must fail");
        assert!(
            err.to_string().contains("forbidden"),
            "unexpected error for {name}: {err}"
        );
    }
}
