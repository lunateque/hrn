use super::*;

fn tbl(raw: &str) -> toml::Table {
    toml::from_str(raw).expect("valid toml")
}

#[test]
fn merge_rules() {
    let layers = vec![
        (
            LayerSource::Default,
            tbl(r#"
thinking = "medium"
[tui]
notify = true
notify_after_secs = 30
theme = "titanium"
"#),
        ),
        (
            LayerSource::Global,
            tbl(r#"
[tui]
theme = "global-theme"
"#),
        ),
        (
            LayerSource::Project,
            tbl(r"
[tui]
notify_after_secs = 10
"),
        ),
        (
            LayerSource::Overlay(PathBuf::from("a.toml")),
            tbl(r#"
[tui]
theme = "overlay-a"
"#),
        ),
        (
            LayerSource::Overlay(PathBuf::from("b.toml")),
            tbl(r#"
[tui]
theme = "overlay-b"
"#),
        ),
    ];
    let merged = merge_layer_list(&layers, Path::new("/repo"), None);
    // Deep merge keeps sibling keys across layers; later overlay wins.
    assert_eq!(
        merged.get("tui.theme").and_then(toml::Value::as_str),
        Some("overlay-b")
    );
    assert_eq!(
        merged
            .get("tui.notify_after_secs")
            .and_then(toml::Value::as_integer),
        Some(10)
    );
    assert_eq!(
        merged.get("tui.notify").and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        merged.source_of("tui.theme"),
        LayerSource::Overlay(PathBuf::from("b.toml"))
    );
    assert_eq!(
        merged.source_of("tui.notify_after_secs"),
        LayerSource::Project
    );
}

#[test]
fn prepend_lists() {
    let layers = vec![
        (
            LayerSource::Global,
            tbl(r#"
[shell.interceptor]
patterns = [{ pattern = "^cat ", tool = "read", message = "use read" }]
"#),
        ),
        (
            LayerSource::Project,
            tbl(r#"
[shell.interceptor]
patterns = [{ pattern = "^rg ", tool = "grep", message = "use grep" }]
"#),
        ),
    ];
    let merged = merge_layer_list(&layers, Path::new("/repo"), None);
    let arr = merged
        .get("shell.interceptor.patterns")
        .and_then(toml::Value::as_array)
        .expect("patterns array");
    assert_eq!(arr.len(), 2);
    // Project prepends ahead of global.
    assert_eq!(
        arr[0].get("tool").and_then(toml::Value::as_str),
        Some("grep")
    );
    assert_eq!(
        arr[1].get("tool").and_then(toml::Value::as_str),
        Some("read")
    );
}

#[test]
fn project_cannot_loosen_operator_shell_patterns() {
    let layers = vec![
        (
            LayerSource::Global,
            tbl(r#"
[shell]
patterns = [{ match = "rm -rf *", approval = "deny" }]
"#),
        ),
        (
            LayerSource::Project,
            tbl(r#"
[shell]
patterns = [
    { match = "rm -rf *", approval = "allow" },
    { match = "cargo test*", approval = "allow" },
]
"#),
        ),
    ];
    let merged = merge_layer_list(&layers, Path::new("/repo"), None);
    let arr = merged
        .get("shell.patterns")
        .and_then(toml::Value::as_array)
        .expect("shell.patterns");
    assert_eq!(arr.len(), 2);
    assert_eq!(
        arr[0].get("approval").and_then(toml::Value::as_str),
        Some("deny")
    );
    assert_eq!(
        arr[1].get("match").and_then(toml::Value::as_str),
        Some("cargo test*")
    );
}

#[test]
fn path_scoped() {
    let layers = vec![(
        LayerSource::Global,
        tbl(r#"
enabled_models = [
    "flash",
    { path = "~/work/app", values = ["opus"] },
    { path_prefix = "~/other", values = ["nope"] },
]
"#),
    )];
    let home = Path::new("/home/alice");
    let merged = merge_layer_list(&layers, Path::new("/home/alice/work/app/sub"), Some(home));
    let vals: Vec<&str> = merged
        .get("enabled_models")
        .and_then(toml::Value::as_array)
        .expect("enabled_models")
        .iter()
        .filter_map(toml::Value::as_str)
        .collect();
    assert_eq!(vals, ["flash", "opus"]);
}

#[test]
fn env_flag_parsing() {
    let mut env = BTreeMap::new();
    env.insert("HRN_ADVISOR_ENABLED".to_owned(), "yes".to_owned());
    env.insert("HRN_TUI_NOTIFY".to_owned(), "0".to_owned());
    env.insert("HRN_TOOLS_PREMIUM_DEFER".to_owned(), "maybe".to_owned());
    env.insert("HRN_TUI_THEME".to_owned(), "titanium-vivid".to_owned());

    let input = LayerInput {
        cwd: PathBuf::from("/repo"),
        env,
        ..LayerInput::default()
    };
    let merged = load_layers(&input).expect("load");
    assert_eq!(
        merged.get("advisor.enabled").and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        merged.get("tui.notify").and_then(toml::Value::as_bool),
        Some(false)
    );
    // Unparseable env value ("maybe") counts as unset -> default `true` survives.
    assert_eq!(
        merged
            .get("tools.premium_defer")
            .and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        merged.source_of("tools.premium_defer"),
        LayerSource::Default
    );
    assert_eq!(
        merged.get("tui.theme").and_then(toml::Value::as_str),
        Some("titanium-vivid")
    );
    assert_eq!(
        merged.source_of("tui.theme"),
        LayerSource::Env("HRN_TUI_THEME".to_owned())
    );
}

#[test]
fn project_cannot_loosen_operator_tools_security_settings() {
    let layers = vec![
        (
            LayerSource::Global,
            tbl(r#"
[tools]
approval_mode = "always-ask"
restricted_folders = ["~/.ssh", "/etc"]

[tools.approval]
bash = "deny"
edit = "prompt"
"#),
        ),
        (
            LayerSource::Project,
            tbl(r#"
[tools]
approval_mode = "yolo"
restricted_folders = ["/repo/secrets"]

[tools.approval]
bash = "allow"
edit = "allow"
write = "deny"
"#),
        ),
    ];
    let merged = merge_layer_list(&layers, Path::new("/repo"), None);

    assert_eq!(
        merged
            .get("tools.approval_mode")
            .and_then(toml::Value::as_str),
        Some("always-ask")
    );
    let folders: Vec<&str> = merged
        .get("tools.restricted_folders")
        .and_then(toml::Value::as_array)
        .expect("restricted_folders")
        .iter()
        .filter_map(toml::Value::as_str)
        .collect();
    assert_eq!(folders, ["~/.ssh", "/etc", "/repo/secrets"]);
    assert_eq!(
        merged
            .get("tools.approval.bash")
            .and_then(toml::Value::as_str),
        Some("deny")
    );
    assert_eq!(
        merged
            .get("tools.approval.edit")
            .and_then(toml::Value::as_str),
        Some("prompt")
    );
    assert_eq!(
        merged
            .get("tools.approval.write")
            .and_then(toml::Value::as_str),
        Some("deny")
    );
}

#[test]
fn project_cannot_loosen_operator_shell_security_settings() {
    let layers = vec![
        (
            LayerSource::Global,
            tbl(r#"
[shell]
git_guard = true
allow_compound_commands = false
allowed_env_vars = ["CI", "RUST_LOG"]
patterns = [
    { match = "git status", approval = "allow" },
    { match = "git *", approval = "prompt" },
    { match = "rm -rf *", approval = "deny" },
]
"#),
        ),
        (
            LayerSource::Project,
            tbl(r#"
[shell]
git_guard = false
allow_compound_commands = true
allowed_env_vars = ["CI", "LD_PRELOAD", "BASH_ENV", "CUSTOM"]
patterns = [
    { match = "git push*", approval = "allow" },
    { match = "rm -rf *", approval = "allow" },
]
"#),
        ),
    ];
    let merged = merge_layer_list(&layers, Path::new("/repo"), None);

    assert_eq!(
        merged.get("shell.git_guard").and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        merged
            .get("shell.allow_compound_commands")
            .and_then(toml::Value::as_bool),
        Some(false)
    );
    let env_vars: Vec<&str> = merged
        .get("shell.allowed_env_vars")
        .and_then(toml::Value::as_array)
        .expect("allowed_env_vars")
        .iter()
        .filter_map(toml::Value::as_str)
        .collect();
    assert_eq!(env_vars, ["CI"]);
    let pats = merged
        .get("shell.patterns")
        .and_then(toml::Value::as_array)
        .expect("patterns");
    assert_eq!(
        pats[0].get("match").and_then(toml::Value::as_str),
        Some("git status")
    );
    assert_eq!(
        pats[0].get("approval").and_then(toml::Value::as_str),
        Some("allow")
    );
    assert_eq!(
        pats[1].get("match").and_then(toml::Value::as_str),
        Some("git *")
    );
    assert_eq!(
        pats[1].get("approval").and_then(toml::Value::as_str),
        Some("prompt")
    );
}

#[test]
fn unrelated_env_or_overlay_does_not_mask_global_approval_mode_and_project_cannot_seed_env_vars() {
    // Regression C2: unrelated `Env` / `Overlay` layers above `Global` must NOT mask Global's
    // `tools.approval_mode = "always-ask"` or `"plan"` when Project tries to loosen to `"yolo"`.
    // Also when operator sets NO `shell.allowed_env_vars` (default `[]`), Project cannot add any.
    for strict_mode in ["always-ask", "plan"] {
        let layers = vec![
            (
                LayerSource::Global,
                tbl(&format!(
                    r#"
[tools]
approval_mode = "{strict_mode}"
"#
                )),
            ),
            (
                LayerSource::Project,
                tbl(r#"
[tools]
approval_mode = "yolo"

[shell]
allowed_env_vars = ["GIT_SSH_COMMAND", "RUSTC_WRAPPER", "MAKEFLAGS", "HOME"]
"#),
            ),
            (
                LayerSource::Overlay(PathBuf::from("extra.toml")),
                tbl(r"
[tui]
notify = false
"),
            ),
            (
                LayerSource::Env("HRN_TUI_THEME".to_owned()),
                tbl(r#"
[tui]
theme = "titanium"
"#),
            ),
        ];
        let merged = merge_layer_list(&layers, Path::new("/repo"), None);
        assert_eq!(
            merged
                .get("tools.approval_mode")
                .and_then(toml::Value::as_str),
            Some(strict_mode),
            "project yolo must not override global {strict_mode} when unrelated env/overlay exist"
        );
        let env_vars: Vec<&str> = merged
            .get("shell.allowed_env_vars")
            .and_then(toml::Value::as_array)
            .expect("allowed_env_vars")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert!(
            env_vars.is_empty(),
            "project must not introduce allowed_env_vars when operator set none: {env_vars:?}"
        );
    }
}

#[test]
fn omp_layer_sits_between_default_and_global_with_provenance() {
    let dir = std::env::temp_dir().join(format!(
        "hrn-omp-layer-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let omp_dir = dir.join(".omp").join("agent");
    std::fs::create_dir_all(&omp_dir).expect("mkdir omp");
    std::fs::write(
        omp_dir.join("config.yml"),
        "\
defaultThinkingLevel: high
hideThinkingBlock: true
colorBlindMode: true
doubleEscapeAction: branch
",
    )
    .expect("write omp config");

    let global_path = dir.join("global.toml");
    std::fs::write(&global_path, "thinking = \"low\"\n").expect("write global");
    let project_path = dir.join("project.toml");
    std::fs::write(&project_path, "[tui]\ncolor_blind_mode = false\n").expect("write project");

    let input = LayerInput {
        global_path: Some(global_path),
        project_path: Some(project_path),
        cwd: dir.clone(),
        home: Some(dir.clone()),
        env: BTreeMap::new(),
        ..LayerInput::default()
    };
    let merged = load_layers(&input).expect("load layers");

    // Global overrides OMP.
    assert_eq!(
        merged.get("thinking").and_then(toml::Value::as_str),
        Some("low")
    );
    assert_eq!(merged.source_of("thinking"), LayerSource::Global);

    // OMP overrides Default.
    assert_eq!(
        merged
            .get("tui.hide_thinking")
            .and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(merged.source_of("tui.hide_thinking"), LayerSource::Omp);
    assert_eq!(
        LayerSource::Omp.label(),
        "omp config (~/.omp/agent/config.yml)"
    );
    assert_eq!(LayerSource::Omp.kind_tag(), "omp");
    assert_eq!(LayerSource::Omp.rank(), 1);
    assert_eq!(
        merged
            .get("interaction.double_escape_action")
            .and_then(toml::Value::as_str),
        Some("tree")
    );
    assert_eq!(
        merged.source_of("interaction.double_escape_action"),
        LayerSource::Omp
    );

    // Project overrides OMP.
    assert_eq!(
        merged
            .get("tui.color_blind_mode")
            .and_then(toml::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        merged.source_of("tui.color_blind_mode"),
        LayerSource::Project
    );

    // Default remains untouched where unset in OMP/Global/Project.
    assert_eq!(merged.source_of("tui.notify"), LayerSource::Default);

    let _ = std::fs::remove_dir_all(&dir);
}
