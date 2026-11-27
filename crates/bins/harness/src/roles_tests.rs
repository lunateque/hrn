use super::*;
use crate::config_layers::LayerInput;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

static DIR_SEQ: AtomicU64 = AtomicU64::new(1);

fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hrn-roles-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

const BASE_TOML: &str = r#"
thinking = "medium"

[[providers]]
name = "a"
kind = "anthropic"
api_key_env = "A_TEST_KEY"

[[providers]]
name = "b"
kind = "openai_chat"

[models.opus]
provider = "a"
id = "claude-opus-4-6"
context_window = 200000

[models.sonnet]
provider = "a"
id = "claude-sonnet-4-6"
context_window = 200000

[models.haiku]
provider = "a"
id = "claude-haiku-4-5"
context_window = 200000

[models.gpt]
provider = "b"
id = "gpt-5"
context_window = 200000

[roles]
default = "opus"
smol = "haiku"
slow = "opus"
plan = "opus"

[modelPresets.lean]
defaultThinkingLevel = "low"
[modelPresets.lean.modelRoles]
default = "sonnet"

[modelPresets.inherit_default]
defaultThinkingLevel = "max"
[modelPresets.inherit_default.modelRoles]
default = "sonnet:inherit"

[modelPresets.broken_provider]
[modelPresets.broken_provider.modelRoles]
default = "gpt"
"#;

#[test]
fn preset_switch_clears_omitted_roles() {
    let dir = temp_dir("clear");
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, BASE_TOML).expect("write");
    let mut cfg = Config::parse(BASE_TOML).expect("parse");

    let (alias, level, report) =
        apply_preset_switch(&mut cfg, &cfg_path, "lean").expect("switch lean");
    assert_eq!(alias, "sonnet");
    assert_eq!(level, ThinkingLevel::Low);
    assert!(report.contains("cleared:"), "{report}");
    assert_eq!(
        cfg.llm.roles.get("default").map(String::as_str),
        Some("sonnet")
    );
    assert!(!cfg.llm.roles.contains_key("smol"));
    assert!(!cfg.llm.roles.contains_key("slow"));
    assert!(!cfg.llm.roles.contains_key("plan"));

    let reloaded = std::fs::read_to_string(&cfg_path).expect("read");
    assert!(reloaded.contains("default = \"sonnet\""), "{reloaded}");
    assert!(!reloaded.contains("smol ="), "{reloaded}");
    assert!(!reloaded.contains("slow ="), "{reloaded}");
    assert!(!reloaded.contains("plan ="), "{reloaded}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preset_unavailable_default_is_noop() {
    let dir = temp_dir("noop");
    let cfg_path = dir.join("config.toml");
    let toml_with_disabled = format!("disabled_providers = [\"b\"]\n{BASE_TOML}");
    std::fs::write(&cfg_path, &toml_with_disabled).expect("write");
    let mut cfg = Config::parse(&toml_with_disabled).expect("parse");
    let before_roles = cfg.llm.roles.clone();
    let before_thinking = cfg.thinking;

    let err = apply_preset_switch(&mut cfg, &cfg_path, "broken_provider")
        .expect_err("disabled default must fail");
    assert!(err.to_string().contains("default unavailable"), "{err}");
    assert_eq!(cfg.llm.roles, before_roles);
    assert_eq!(cfg.thinking, before_thinking);

    let disk = std::fs::read_to_string(&cfg_path).expect("read");
    assert_eq!(disk, toml_with_disabled);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preset_delete_removes_table() {
    let dir = temp_dir("delete");
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, BASE_TOML).expect("write");
    let mut cfg = Config::parse(BASE_TOML).expect("parse");

    assert!(cfg.llm.role_presets.contains_key("lean"));
    delete_preset(&mut cfg, &cfg_path, "lean").expect("delete lean");
    assert!(!cfg.llm.role_presets.contains_key("lean"));

    let disk = std::fs::read_to_string(&cfg_path).expect("read");
    assert!(!disk.contains("[modelPresets.lean]"), "{disk}");
    assert!(disk.contains("[modelPresets.inherit_default]"), "{disk}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preset_inherit_keeps_level() {
    let dir = temp_dir("inherit");
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, BASE_TOML).expect("write");
    let mut cfg = Config::parse(BASE_TOML).expect("parse");
    cfg.thinking = ThinkingLevel::High;

    let (alias, level, _) =
        apply_preset_switch(&mut cfg, &cfg_path, "inherit_default").expect("switch inherit");
    assert_eq!(alias, "sonnet");
    assert_eq!(level, ThinkingLevel::High);
    assert_eq!(cfg.thinking, ThinkingLevel::High);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn assign_role_persists_and_config_get_reads_it() {
    let dir = temp_dir("assign");
    let global_path = dir.join("global.toml");
    let project_cwd = dir.join("repo");
    std::fs::create_dir_all(&project_cwd).expect("mkdir repo");
    std::fs::write(&global_path, BASE_TOML).expect("write global");

    let mut cfg = Config::parse(BASE_TOML).expect("parse");
    let global_target = role_storage_path(&cfg, &global_path, &project_cwd);
    assert_eq!(global_target, global_path);

    assign_role(&mut cfg, &global_target, "plan", "sonnet:high").expect("assign plan");

    // Verify `config_layers` (what `hrn config get roles.plan` reads) observes the new value.
    let input = LayerInput {
        global_path: Some(global_path.clone()),
        project_path: None,
        overlays: Vec::new(),
        cwd: project_cwd.clone(),
        home: Some(dir.clone()),
        env: BTreeMap::new(),
        runtime: toml::Table::new(),
    };
    let (_, merged) = Config::load_layered(&input).expect("reload");
    assert_eq!(
        merged.get("roles.plan").and_then(toml::Value::as_str),
        Some("sonnet:high")
    );

    // Project storage writes to `<cwd>/.harness/config.toml`.
    cfg.llm.role_storage = RoleStorage::Project;
    let project_target = role_storage_path(&cfg, &global_path, &project_cwd);
    assert_eq!(
        project_target,
        project_cwd.join(".harness").join("config.toml")
    );
    assign_role(&mut cfg, &project_target, "advisor", "opus:low").expect("assign project role");
    let project_text = std::fs::read_to_string(&project_target).expect("read project");
    assert!(
        project_text.contains("advisor = \"opus:low\""),
        "{project_text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_preset_captures_active_model_override() {
    let dir = temp_dir("save");
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, BASE_TOML).expect("write");
    let mut cfg = Config::parse(BASE_TOML).expect("parse");

    save_preset(
        &mut cfg,
        &cfg_path,
        "custom",
        Some("haiku"),
        ThinkingLevel::Xhigh,
    )
    .expect("save preset");
    let saved = cfg.llm.role_presets.get("custom").expect("in-memory");
    assert_eq!(
        saved.model_roles.get("default").map(String::as_str),
        Some("haiku")
    );
    assert_eq!(saved.default_thinking_level, Some(ThinkingLevel::Xhigh));

    let disk = std::fs::read_to_string(&cfg_path).expect("read");
    assert!(disk.contains("default = \"haiku\""), "{disk}");
    assert!(disk.contains("defaultThinkingLevel = \"xhigh\""), "{disk}");
    let _ = std::fs::remove_dir_all(&dir);
}
