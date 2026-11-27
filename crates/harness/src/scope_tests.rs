use super::*;
use crate::testutil::hook_ctx;
use hrn_kernel::PathKind;
use serde_json::json;

#[test]
fn glob_and_prefix_matching() {
    assert!(path_matches_pattern(
        "crates/front/tui/src/render.rs",
        "crates/front/tui/**"
    ));
    assert!(path_matches_pattern(
        "crates/front/tui/src/render.rs",
        "crates/front/tui"
    ));
    assert!(path_matches_pattern(
        "crates/bins/harness/src/chat.rs",
        "crates/bins/harness/src/chat.rs"
    ));
    assert!(!path_matches_pattern(
        "crates/edge/http/src/guard.rs",
        "crates/front/tui/**"
    ));
    assert!(path_matches_pattern("src/a.rs", "src/*.rs"));
    assert!(!path_matches_pattern("src/sub/a.rs", "src/*.rs"));
}

fn poll_hook(hook: &ScopeGuardHook, tool: &str, args: &Value) -> HookVerdict {
    let fut = hook.before_tool(&hook_ctx(), tool, args);
    let mut fut = std::pin::pin!(fut);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => v.expect("hook ok"),
        std::task::Poll::Pending => panic!("hook future must be ready"),
    }
}

#[test]
fn guard_blocks_out_of_scope_denied_and_traversal_paths() {
    let cwd = PathBuf::from("/repo");
    let hook = ScopeGuardHook::new(
        cwd,
        vec![
            "crates/front/tui/**".to_owned(),
            "crates/bins/harness/src/chat.rs".to_owned(),
        ],
        vec!["crates/front/tui/src/secret.rs".to_owned()],
    );

    let ok = poll_hook(
        &hook,
        "edit",
        &json!({"path": "crates/front/tui/src/app.rs"}),
    );
    assert!(matches!(ok, HookVerdict::Continue));

    let ok_read = poll_hook(&hook, "read", &json!({"path": "Cargo.toml"}));
    assert!(matches!(ok_read, HookVerdict::Continue));

    let blocked = poll_hook(&hook, "write", &json!({"path": "Cargo.toml"}));
    assert!(matches!(blocked, HookVerdict::ShortCircuit(out) if out.is_error));

    let denied = poll_hook(
        &hook,
        "edit",
        &json!({"path": "crates/front/tui/src/secret.rs"}),
    );
    assert!(matches!(denied, HookVerdict::ShortCircuit(out) if out.is_error));

    let escape = poll_hook(&hook, "edit", &json!({"path": "../../etc/passwd"}));
    assert!(matches!(escape, HookVerdict::ShortCircuit(out) if out.is_error));
}

#[test]
fn guard_blocks_apply_patch_ast_edit_resolve_single_quoted_mv_and_symlinks() {
    struct LinkFs;
    impl WorkspaceFs for LinkFs {
        fn read_to_string(&self, _: &Path) -> Result<Option<String>> {
            Ok(None)
        }
        fn list_dir(&self, _: &Path) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
        fn kind(&self, _: &Path) -> PathKind {
            PathKind::Missing
        }
        fn home_dir(&self) -> Option<PathBuf> {
            None
        }
        fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
            let s = path.to_string_lossy().replace('\\', "/");
            if s == "/repo/src/escape" {
                Some(PathBuf::from("/outside"))
            } else {
                None
            }
        }
    }

    let hook = ScopeGuardHook::new(
        PathBuf::from("/repo"),
        vec!["src/**".to_owned()],
        vec!["src/secret.rs".to_owned()],
    )
    .with_fs(Arc::new(LinkFs));

    // `apply_patch` targeting denied or out-of-scope file is blocked.
    let patch_denied = poll_hook(
        &hook,
        "apply_patch",
        &json!({"patch": "*** Begin Patch\n*** Update File: Cargo.toml\n@@\n*** End Patch"}),
    );
    assert!(matches!(patch_denied, HookVerdict::ShortCircuit(out) if out.is_error));

    // `ast_edit` with `;`-delimited paths blocks out-of-scope secondary target.
    let ast_blocked = poll_hook(&hook, "ast_edit", &json!({"path": "src/lib.rs;Cargo.toml"}));
    assert!(matches!(ast_blocked, HookVerdict::ShortCircuit(out) if out.is_error));

    // `resolve` targeting denied file is blocked.
    let resolve_blocked = poll_hook(&hook, "resolve", &json!({"file": "src/secret.rs"}));
    assert!(matches!(resolve_blocked, HookVerdict::ShortCircuit(out) if out.is_error));

    // Single-quoted `MV '/outside/pwn.txt'` in hashline edit is blocked.
    let mv_blocked = poll_hook(
        &hook,
        "edit",
        &json!({"input": "[src/lib.rs#abcd]\nMV '/outside/pwn.txt'\n"}),
    );
    assert!(matches!(mv_blocked, HookVerdict::ShortCircuit(out) if out.is_error));

    // Symlink ancestor `/repo/src/escape -> /outside` is canonicalized and blocked.
    let link_blocked = poll_hook(&hook, "write", &json!({"path": "src/escape/pwn.rs"}));
    assert!(matches!(link_blocked, HookVerdict::ShortCircuit(out) if out.is_error));
}

#[test]
fn denied_paths_only_blocks_ast_edit_globs_dirs_and_pathless_resolve() {
    let hook = ScopeGuardHook::new(
        PathBuf::from("/repo"),
        Vec::new(),
        vec!["src/secret.rs".to_owned()],
    );

    // Wildcards and directories that could match `src/secret.rs` MUST be blocked even when
    // `allowed_paths` is empty.
    for overlapping in ["src/*.rs", "sec*", "src/[s]ecret.rs", "**/*.rs", "src", "."] {
        let blocked = poll_hook(&hook, "ast_edit", &json!({"paths": [overlapping]}));
        assert!(
            matches!(blocked, HookVerdict::ShortCircuit(out) if out.is_error),
            "expected denied_paths to block ast_edit `{overlapping}`"
        );
    }

    // Pathless `resolve {}` fails closed when `denied_paths` is active.
    let resolve_empty = poll_hook(&hook, "resolve", &json!({}));
    assert!(matches!(resolve_empty, HookVerdict::ShortCircuit(out) if out.is_error));

    // Unrelated concrete file and disjoint glob remain allowed.
    let ok_file = poll_hook(&hook, "ast_edit", &json!({"paths": ["src/lib.rs"]}));
    assert!(matches!(ok_file, HookVerdict::Continue));
    let ok_glob = poll_hook(&hook, "ast_edit", &json!({"paths": ["docs/*.rs"]}));
    assert!(matches!(ok_glob, HookVerdict::Continue));
}

#[test]
fn scope_policy_with_paths_removes_eval_alongside_bash() {
    use crate::registry::ToolRegistry;
    use crate::runtime::HarnessConfig;
    use crate::testutil::{MemFs, MemStore, ScriptedLlm};
    use hrn_kernel::{Tool, ToolEffect, ToolSpec};

    struct StubTool(&'static str, ToolEffect);
    impl Tool for StubTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.to_owned(),
                description: String::new(),
                parameters: json!({ "type": "object" }),
            }
        }
        fn effect(&self) -> ToolEffect {
            self.1
        }
        fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
            Box::pin(async { Ok(ToolOutput::ok("ok")) })
        }
    }

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(StubTool("read", ToolEffect::ReadOnly)));
    tools.register(Arc::new(StubTool("write", ToolEffect::Mutating)));
    tools.register(Arc::new(StubTool("bash", ToolEffect::Mutating)));
    tools.register(Arc::new(StubTool("eval", ToolEffect::Mutating)));
    let runtime = AgentRuntime::new(
        Arc::new(ScriptedLlm::default()),
        Arc::new(MemStore::default()),
        tools,
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );

    let allowed = ScopePolicy {
        allowed_paths: vec!["src/**".to_owned()],
        ..ScopePolicy::default()
    }
    .apply_to_runtime(&runtime, PathBuf::from("/repo"));
    assert!(allowed.tools().get("bash").is_none());
    assert!(allowed.tools().get("eval").is_none());
    assert!(allowed.tools().get("read").is_some());
    assert!(allowed.tools().get("write").is_some());

    let denied = ScopePolicy {
        denied_paths: vec!["secrets/**".to_owned()],
        ..ScopePolicy::default()
    }
    .apply_to_runtime(&runtime, PathBuf::from("/repo"));
    assert!(denied.tools().get("bash").is_none());
    assert!(denied.tools().get("eval").is_none());

    let opted_in = ScopePolicy {
        allowed_paths: vec!["src/**".to_owned()],
        allow_bash_with_paths: true,
        ..ScopePolicy::default()
    }
    .apply_to_runtime(&runtime, PathBuf::from("/repo"));
    assert!(opted_in.tools().get("bash").is_some());
    assert!(opted_in.tools().get("eval").is_some());
}

#[test]
fn edit_apply_patch_mode_enforces_scope_and_restricted_folders() {
    use crate::approvals::{extract_targets, matching_restricted_folder_for_tool};

    let patch = "*** Begin Patch\n*** Update File: secrets/key.pem\n@@\n-a\n+b\n\
                 *** Update File: src/ok.rs\n*** Move to: secrets/moved.rs\n@@\n-x\n+y\n*** End Patch";
    let args = json!({ "patch": patch });

    // `ScopeGuardHook` blocks `edit` in `apply_patch` mode touching denied or outside-allowed paths.
    let hook = ScopeGuardHook::new(
        PathBuf::from("/repo"),
        vec!["src/**".to_owned()],
        vec!["secrets/**".to_owned()],
    );
    let verdict = poll_hook(&hook, "edit", &args);
    assert!(
        matches!(verdict, HookVerdict::ShortCircuit(out) if out.is_error),
        "edit(patch) must be blocked by ScopeGuardHook"
    );

    // `approvals_path::extract_targets` and `restricted_folders` match `edit` patch targets too.
    let targets = extract_targets("edit", &args);
    assert!(
        targets.iter().any(|t| t == "secrets/key.pem"),
        "{targets:?}"
    );
    assert!(
        targets.iter().any(|t| t == "secrets/moved.rs"),
        "{targets:?}"
    );
    let hit = matching_restricted_folder_for_tool(
        "edit",
        &["secrets".to_owned()],
        &targets,
        Path::new("/repo"),
        None,
        None,
    );
    assert_eq!(hit.as_deref(), Some("secrets"));
}

#[test]
fn lsp_format_enforces_scope_while_read_only_lsp_continues() {
    let hook = ScopeGuardHook::new(
        PathBuf::from("/repo"),
        vec!["src/**".to_owned()],
        vec!["src/secret.rs".to_owned()],
    );

    let hover_outside = poll_hook(
        &hook,
        "lsp",
        &json!({"action": "hover", "file": "Cargo.toml", "line": 1, "col": 1}),
    );
    assert!(matches!(hover_outside, HookVerdict::Continue));

    let fmt_allowed = poll_hook(
        &hook,
        "lsp",
        &json!({"action": "format", "file": "src/lib.rs"}),
    );
    assert!(matches!(fmt_allowed, HookVerdict::Continue));

    let fmt_outside = poll_hook(
        &hook,
        "lsp",
        &json!({"action": "format", "file": "Cargo.toml"}),
    );
    assert!(matches!(fmt_outside, HookVerdict::ShortCircuit(out) if out.is_error));

    let fmt_denied = poll_hook(
        &hook,
        "lsp",
        &json!({"action": "format", "file": "src/secret.rs"}),
    );
    assert!(matches!(fmt_denied, HookVerdict::ShortCircuit(out) if out.is_error));
}
