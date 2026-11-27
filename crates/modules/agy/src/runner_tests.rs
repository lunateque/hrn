//! Unit tests for `hrn-mod-agy` CLI argument builder, stdout parser, and `/agy` slash parser.

use super::*;

#[test]
fn builds_official_agy_cli_flags_in_order() {
    let cfg = AgyConfig {
        default_model: Some("gemini-pro".to_owned()),
        default_effort: Some("high".to_owned()),
        default_mode: Some("plan".to_owned()),
        ..AgyConfig::default()
    };
    let req = AgyRequest {
        prompt: "audit crate".to_owned(),
        cwd: std::env::temp_dir(),
        model: None,
        effort: Some("medium".to_owned()),
        mode: None,
        resume: Some("conv-42".to_owned()),
        add_dirs: vec!["/extra".to_owned()],
        skip_permissions: true,
        timeout_secs: Some(90),
        ..AgyRequest::default()
    };
    let args = build_args(&cfg, &req, 90);
    assert_eq!(
        args,
        vec![
            "-p",
            "--output-format",
            "json",
            "--print-timeout",
            "90s",
            "--model",
            "gemini-pro",
            "--effort",
            "medium",
            "--mode",
            "plan",
            "--resume",
            "conv-42",
            "--add-dir",
            "/extra",
            "--dangerously-skip-permissions",
            "audit crate",
        ]
    );
}

#[test]
fn parses_json_and_plain_text_stdout() {
    let json = r#"{"result":"done!","conversation_id":"c-123"}"#;
    let (out, id) = parse_agy_stdout(json);
    assert_eq!(out, "done!");
    assert_eq!(id.as_deref(), Some("c-123"));

    let (plain, no_id) = parse_agy_stdout("  hello world  \n");
    assert_eq!(plain, "hello world");
    assert!(no_id.is_none());
}

#[test]
fn parses_slash_command_status_and_flags() {
    let cwd = std::env::temp_dir();
    assert!(parse_slash_command(None, cwd.clone()).unwrap().is_none());
    assert!(
        parse_slash_command(Some("status"), cwd.clone())
            .unwrap()
            .is_none()
    );
    let req = parse_slash_command(
        Some("--model gemini-pro --worktree --allow src/** audit auth flow"),
        cwd,
    )
    .unwrap()
    .expect("request");
    assert_eq!(req.model.as_deref(), Some("gemini-pro"));
    assert!(req.needs_worktree());
    assert_eq!(req.allowed_paths, vec!["src/**"]);
    assert_eq!(req.prompt, "audit auth flow");
}
