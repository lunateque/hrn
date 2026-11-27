use crate::approvals::{
    ApprovalResolution, GrantCache, UserApprovalPolicy, normalized_call_targets, resolve,
    resolve_for_call,
};
use hrn_kernel::{PermissionMode, ToolApproval};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

#[test]
fn restricted_folder_catches_redirections_subshells_ansi_c_and_inline_cd() {
    let policy = UserApprovalPolicy {
        restricted_folders: vec![
            "/vault".to_owned(),
            "/repo/secrets".to_owned(),
            "~/.ssh".to_owned(),
        ],
        tool_policies: BTreeMap::new(),
    };
    let grants = GrantCache::memory();
    let cwd = Path::new("/repo");
    let home = Path::new("/home/alice");

    for cmd in [
        "cat>/vault/secret",
        "cat $(</vault/secret)",
        "sh -c 'cat /vault/secret'",
        r#"bash -lc "cat /vault/secret""#,
        r#"powershell -Command "Get-Content /vault/secret""#,
        "cat $'/vault/secret'",
        r"cat $'\x2fvault/secret'",
        r"cat $'\057vault/secret'",
        r"cat $'\57vault/secret'",
        r"cat $'\u002fvault/secret'",
        "cd /var/tmp && cat ../../vault/secret",
        "cd /repo",
        "cd",
        "cd -",
    ] {
        let targets = normalized_call_targets(
            "bash",
            &json!({"command": cmd}),
            cwd,
            None,
            Some(home),
            None,
        );
        let res = resolve_for_call(
            "s1",
            "bash",
            &ToolApproval::exec(),
            &policy,
            PermissionMode::Yolo,
            &targets,
            cwd,
            Some(home),
            None,
            true,
            &grants,
        );
        assert!(
            matches!(
                res,
                ApprovalResolution::Prompt {
                    restricted: true,
                    ..
                }
            ),
            "expected restricted prompt for `{cmd}`, got {res:?} (targets={targets:?})"
        );
    }

    for safe_cmd in ["echo hello", "cd /repo/src && cargo test"] {
        let targets = normalized_call_targets(
            "bash",
            &json!({"command": safe_cmd}),
            cwd,
            None,
            Some(home),
            None,
        );
        let res = resolve_for_call(
            "s1",
            "bash",
            &ToolApproval::exec(),
            &policy,
            PermissionMode::Yolo,
            &targets,
            cwd,
            Some(home),
            None,
            true,
            &grants,
        );
        assert_eq!(res, ApprovalResolution::Allow, "safe `{safe_cmd}`");
    }
}

#[test]
fn restricted_folder_catches_windows_cmd_and_persistent_cd() {
    let policy = UserApprovalPolicy {
        restricted_folders: vec!["/vault".to_owned()],
        tool_policies: BTreeMap::new(),
    };
    let grants = GrantCache::memory();
    let cwd = Path::new("/repo");
    let home = Path::new("/home/alice");

    let win_policy = UserApprovalPolicy {
        restricted_folders: vec![r"C:\vault".to_owned()],
        tool_policies: BTreeMap::new(),
    };
    let win_targets = normalized_call_targets(
        "bash",
        &json!({"command": r#"cmd /c "type C:\vault\secret""#}),
        Path::new(r"C:\repo"),
        None,
        None,
        None,
    );
    assert!(matches!(
        resolve_for_call(
            "s1",
            "bash",
            &ToolApproval::exec(),
            &win_policy,
            PermissionMode::Yolo,
            &win_targets,
            Path::new(r"C:\repo"),
            None,
            None,
            true,
            &grants,
        ),
        ApprovalResolution::Prompt {
            restricted: true,
            ..
        }
    ));

    // Persistent `bash` `cd` across calls: after `cd /vault` runs in `s1`, a follow-up relative
    // `cat secret` still triggers the restricted prompt for `/vault`.
    let cd_call = normalized_call_targets(
        "bash",
        &json!({"command": "cd /vault"}),
        cwd,
        None,
        Some(home),
        None,
    );
    let next_dir = cd_call.bash_next_cwd.expect("tracked cd target");
    grants.set_session_bash_cwd("s1", next_dir);
    let follow_up = normalized_call_targets(
        "bash",
        &json!({"command": "cat secret"}),
        cwd,
        grants.session_bash_cwd("s1").as_deref(),
        Some(home),
        None,
    );
    assert!(matches!(
        resolve_for_call(
            "s1",
            "bash",
            &ToolApproval::exec(),
            &policy,
            PermissionMode::Yolo,
            &follow_up,
            cwd,
            Some(home),
            None,
            true,
            &grants,
        ),
        ApprovalResolution::Prompt {
            restricted: true,
            ..
        }
    ));
}

#[test]
fn restricted_folder_catches_ast_edit_globs_and_pathless_resolve() {
    let policy = UserApprovalPolicy {
        restricted_folders: vec!["/repo/secrets".to_owned()],
        tool_policies: BTreeMap::new(),
    };
    let grants = GrantCache::memory();
    let cwd = Path::new("/repo");

    // `ast_edit` with wildcard `sec*` whose static prefix `<cwd>` is an ancestor of `/repo/secrets`
    // triggers restricted prompt even in `yolo`.
    for glob_pat in ["sec*", "secrets/*.rs", "**/key.rs"] {
        let targets = normalized_call_targets(
            "ast_edit",
            &json!({"paths": [glob_pat]}),
            cwd,
            None,
            None,
            None,
        );
        let res = resolve_for_call(
            "s1",
            "ast_edit",
            &ToolApproval::write(),
            &policy,
            PermissionMode::Yolo,
            &targets,
            cwd,
            None,
            None,
            true,
            &grants,
        );
        assert!(
            matches!(
                res,
                ApprovalResolution::Prompt {
                    restricted: true,
                    ..
                }
            ),
            "expected restricted prompt for ast_edit glob `{glob_pat}`, got {res:?}"
        );
    }

    // Disjoint `ast_edit` glob `src/*.rs` (static prefix `/repo/src`) stays allowed in `yolo`.
    let safe_glob = normalized_call_targets(
        "ast_edit",
        &json!({"paths": ["src/*.rs"]}),
        cwd,
        None,
        None,
        None,
    );
    assert_eq!(
        resolve_for_call(
            "s1",
            "ast_edit",
            &ToolApproval::write(),
            &policy,
            PermissionMode::Yolo,
            &safe_glob,
            cwd,
            None,
            None,
            true,
            &grants,
        ),
        ApprovalResolution::Allow
    );

    // Pathless `resolve {}` in `/repo` overlaps child restricted folder `/repo/secrets` -> prompt!
    assert!(matches!(
        resolve(
            "resolve",
            &ToolApproval::write(),
            &policy,
            PermissionMode::Yolo,
            &[],
            cwd,
            None,
            None,
            true,
            &grants,
        ),
        ApprovalResolution::Prompt {
            restricted: true,
            ..
        }
    ));
}
