use super::*;
use crate::testutil::MemStore;
use hrn_kernel::{PathKind, Result, SessionStore, SystemClock};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn resolve_ladder_user_deny_beats_yolo() {
    let mut policy = UserApprovalPolicy::default();
    policy
        .tool_policies
        .insert("bash".to_owned(), ToolApprovalPolicy::Deny);
    policy
        .tool_policies
        .insert("mcp__srv_drop".to_owned(), ToolApprovalPolicy::Deny);
    policy
        .tool_policies
        .insert("edit".to_owned(), ToolApprovalPolicy::Prompt);
    let grants = GrantCache::memory();

    let bash_res = resolve(
        "bash",
        &ToolApproval::exec(),
        &policy,
        PermissionMode::Yolo,
        &[],
        Path::new("/repo"),
        None,
        None,
        true,
        &grants,
    );
    assert!(matches!(bash_res, ApprovalResolution::Deny { .. }));

    // MCP tool qualified name `mcp__<server>_<tool>` is matched by `[tools.approval]`.
    let mcp_res = resolve(
        "mcp__srv_drop",
        &ToolApproval::exec(),
        &policy,
        PermissionMode::Yolo,
        &[],
        Path::new("/repo"),
        None,
        None,
        true,
        &grants,
    );
    assert!(matches!(mcp_res, ApprovalResolution::Deny { .. }));

    // Explicit `prompt` beats `yolo` too.
    let edit_res = resolve(
        "edit",
        &ToolApproval::write(),
        &policy,
        PermissionMode::Yolo,
        &["src/lib.rs".to_owned()],
        Path::new("/repo"),
        None,
        None,
        true,
        &grants,
    );
    assert!(matches!(
        edit_res,
        ApprovalResolution::Prompt {
            restricted: false,
            ..
        }
    ));
}

#[test]
fn restricted_folder_prompts_in_yolo() {
    let policy = UserApprovalPolicy {
        restricted_folders: vec!["/secrets".to_owned()],
        tool_policies: BTreeMap::from([("read".to_owned(), ToolApprovalPolicy::Allow)]),
    };
    let grants = GrantCache::memory();
    grants.grant_tool("read", ToolTier::Write);
    grants.grant_file("/secrets/key.pem", ToolTier::Write);

    // Path tool targeting a restricted folder (with trailing line selector) forces plain prompt
    // even in `yolo` and even with an existing tool/file grant and user Allow override.
    let path_res = resolve(
        "read",
        &ToolApproval::read(),
        &policy,
        PermissionMode::Yolo,
        &["/secrets/key.pem:1-10".to_owned()],
        Path::new("/repo"),
        None,
        None,
        true,
        &grants,
    );
    assert_eq!(
        path_res,
        ApprovalResolution::Prompt {
            reason: Some("target is inside restricted folder `/secrets`".to_owned()),
            choices: vec![ApprovalGrantScope::Once],
            restricted: true,
        }
    );

    // Pathless tool matches by cwd when cwd is inside the restricted folder.
    let pathless_res = resolve(
        "bash",
        &ToolApproval::exec(),
        &policy,
        PermissionMode::Yolo,
        &[],
        Path::new("/secrets/sub"),
        None,
        None,
        true,
        &grants,
    );
    assert!(matches!(
        pathless_res,
        ApprovalResolution::Prompt {
            restricted: true,
            choices,
            ..
        } if choices == vec![ApprovalGrantScope::Once]
    ));

    // Outside the restricted folder, yolo still auto-allows.
    let ok_res = resolve(
        "read",
        &ToolApproval::read(),
        &policy,
        PermissionMode::Yolo,
        &["/repo/src/main.rs".to_owned()],
        Path::new("/repo"),
        None,
        None,
        true,
        &grants,
    );
    assert_eq!(ok_res, ApprovalResolution::Allow);
}

#[test]
fn headless_fails_closed() {
    let grants = GrantCache::memory();
    let empty = UserApprovalPolicy::default();

    // Default mode Write tool with no UI -> Deny.
    let write_headless = resolve(
        "write",
        &ToolApproval::write(),
        &empty,
        PermissionMode::Default,
        &["/repo/a.txt".to_owned()],
        Path::new("/repo"),
        None,
        None,
        false,
        &grants,
    );
    assert!(matches!(write_headless, ApprovalResolution::Deny { .. }));

    // AskCached Read tool with no UI -> Deny.
    let cached_headless = resolve(
        "read",
        &ToolApproval::read(),
        &empty,
        PermissionMode::AskCached,
        &["/repo/a.txt".to_owned()],
        Path::new("/repo"),
        None,
        None,
        false,
        &grants,
    );
    assert!(matches!(cached_headless, ApprovalResolution::Deny { .. }));

    // Restricted folder in Yolo mode with no UI -> Deny (fails closed!).
    let restricted_policy = UserApprovalPolicy {
        restricted_folders: vec!["/vault".to_owned()],
        tool_policies: BTreeMap::new(),
    };
    let restricted_headless = resolve(
        "read",
        &ToolApproval::read(),
        &restricted_policy,
        PermissionMode::Yolo,
        &["/vault/token.txt".to_owned()],
        Path::new("/repo"),
        None,
        None,
        false,
        &grants,
    );
    assert!(matches!(
        restricted_headless,
        ApprovalResolution::Deny { .. }
    ));
}

#[test]
fn cached_directory_grant_direct_children_only() {
    let grants = GrantCache::memory();
    let policy = UserApprovalPolicy::default();
    let cwd = Path::new("/ws");

    // Record a Write Directory grant via a file inside `/ws/src`.
    let norm_a = normalize_target(cwd, "src/a.rs:10-20", None, None);
    grants
        .record(
            ApprovalGrantScope::Directory,
            "edit",
            ToolTier::Write,
            &[norm_a],
        )
        .expect("record");

    // Direct child `/ws/src/b.rs` with Write -> covered!
    let direct_write = resolve(
        "edit",
        &ToolApproval::write(),
        &policy,
        PermissionMode::AskCached,
        &["src/b.rs".to_owned()],
        cwd,
        None,
        None,
        true,
        &grants,
    );
    assert_eq!(direct_write, ApprovalResolution::Allow);

    // Direct child `/ws/src/c.rs#L12` with Read -> covered (Write dominates Read)!
    let direct_read = resolve(
        "read",
        &ToolApproval::read(),
        &policy,
        PermissionMode::AskCached,
        &["src/c.rs#L12".to_owned()],
        cwd,
        None,
        None,
        true,
        &grants,
    );
    assert_eq!(direct_read, ApprovalResolution::Allow);

    // Nested subdirectory child `/ws/src/nested/d.rs` -> NOT covered (direct children only)!
    let nested_read = resolve(
        "read",
        &ToolApproval::read(),
        &policy,
        PermissionMode::AskCached,
        &["src/nested/d.rs".to_owned()],
        cwd,
        None,
        None,
        true,
        &grants,
    );
    assert!(matches!(nested_read, ApprovalResolution::Prompt { .. }));

    // Read grant NEVER covers Write or Exec.
    let read_only = GrantCache::memory();
    let norm_x = normalize_target(cwd, "docs/readme.md", None, None);
    read_only
        .record(ApprovalGrantScope::File, "read", ToolTier::Read, &[norm_x])
        .expect("record");
    assert_eq!(
        resolve(
            "read",
            &ToolApproval::read(),
            &policy,
            PermissionMode::AskCached,
            &["docs/readme.md:1-5".to_owned()],
            cwd,
            None,
            None,
            true,
            &read_only,
        ),
        ApprovalResolution::Allow
    );
    assert!(matches!(
        resolve(
            "edit",
            &ToolApproval::write(),
            &policy,
            PermissionMode::AskCached,
            &["docs/readme.md".to_owned()],
            cwd,
            None,
            None,
            true,
            &read_only,
        ),
        ApprovalResolution::Prompt { .. }
    ));
}

#[test]
fn persistent_grants_survive_new_cache_and_clear_forgets_both() {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::default());
    let cache1 = GrantCache::with_store(Arc::clone(&store), Arc::new(SystemClock));
    let cwd = Path::new("/ws");
    let target = normalize_target(cwd, "notes.txt:raw", None, None);

    cache1
        .record(
            ApprovalGrantScope::Persistent,
            "write",
            ToolTier::Write,
            std::slice::from_ref(&target),
        )
        .expect("persist");

    // A fresh cache over the same store inherits the persisted grant.
    let cache2 = GrantCache::with_store(Arc::clone(&store), Arc::new(SystemClock));
    assert!(cache2.covers("read", ToolTier::Read, std::slice::from_ref(&target)));
    assert!(cache2.covers("write", ToolTier::Write, std::slice::from_ref(&target)));

    // `/forget-approvals` clears both session memory and persistent store.
    cache2.clear().expect("clear");
    assert!(!cache2.covers("read", ToolTier::Read, std::slice::from_ref(&target)));
    assert!(store.list_approval_grants().expect("list").is_empty());
}

#[test]
fn selector_peeling_and_symlink_ancestor_canonicalization() {
    struct SymlinkFs;
    impl WorkspaceFs for SymlinkFs {
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
            Some(PathBuf::from("/home/alice"))
        }
        fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
            let s = path.to_string_lossy().replace('\\', "/");
            if s == "/repo/link" {
                Some(PathBuf::from("/vault"))
            } else {
                None
            }
        }
    }

    assert_eq!(peel_selector("src/lib.rs:10-20"), "src/lib.rs");
    assert_eq!(peel_selector("src/lib.rs:10-20:raw"), "src/lib.rs");
    assert_eq!(peel_selector("src/lib.rs#L42"), "src/lib.rs");
    assert_eq!(peel_selector("src/lib.rs#L10-L25"), "src/lib.rs");
    assert_eq!(peel_selector(r"C:\src\lib.rs:5"), r"C:\src\lib.rs");
    assert_eq!(peel_selector(r"C:\src\lib.rs"), r"C:\src\lib.rs");

    // Creating a new file `/repo/link/new.txt` inside a symlink to `/vault` still matches `/vault`.
    let norm = normalize_target(
        Path::new("/repo"),
        "link/new.txt",
        Some(Path::new("/home/alice")),
        Some(&SymlinkFs),
    );
    assert_eq!(norm, "/vault/new.txt");
    let matched = matching_restricted_folder(
        &["/vault".to_owned()],
        &["link/new.txt".to_owned()],
        Path::new("/repo"),
        Some(Path::new("/home/alice")),
        Some(&SymlinkFs),
    );
    assert_eq!(matched.as_deref(), Some("/vault"));
}

#[test]
fn restricted_folder_catches_semicolon_lists_dir_scans_bash_paths_and_windows_paths() {
    use serde_json::json;

    // 1. `;`-delimited path list in `grep` catches secondary restricted folder.
    let grep_targets = extract_targets("grep", &json!({"path": ".;/vault/secrets"}));
    assert_eq!(grep_targets, vec![".", "/vault/secrets"]);
    let policy = UserApprovalPolicy {
        restricted_folders: vec!["/vault".to_owned(), "/repo/secrets".to_owned()],
        tool_policies: BTreeMap::new(),
    };
    let grants = GrantCache::memory();
    assert!(matches!(
        resolve(
            "grep",
            &ToolApproval::read(),
            &policy,
            PermissionMode::Yolo,
            &grep_targets,
            Path::new("/repo"),
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

    // 2. Directory-scanning tool (`grep { path: "." }`) overlapping child restricted folder
    //    `/repo/secrets` triggers restricted prompt even in `yolo`!
    assert!(matches!(
        resolve(
            "grep",
            &ToolApproval::read(),
            &policy,
            PermissionMode::Yolo,
            &[".".to_owned()],
            Path::new("/repo"),
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

    // 3. `program`, `file`, and `add_dirs` are extracted as path targets.
    assert_eq!(
        extract_targets(
            "agy_run",
            &json!({"program": "/vault/app", "add_dirs": ["/repo/secrets"]})
        ),
        vec!["/vault/app", "/repo/secrets"]
    );
    assert_eq!(
        extract_targets("debug", &json!({"file": "/vault/main.rs"})),
        vec!["/vault/main.rs"]
    );

    // 4. `bash` command path inspection catches `cat /vault/secret`, `cd /vault`, `cat ~/.ssh/id`.
    let (_, bash_targets) = normalized_targets_for_call(
        "bash",
        &json!({"command": "cat /vault/secret && echo done"}),
        Path::new("/repo"),
        Some(Path::new("/home/alice")),
        None,
    );
    assert!(matches!(
        resolve(
            "bash",
            &ToolApproval::exec(),
            &policy,
            PermissionMode::Yolo,
            &bash_targets,
            Path::new("/repo"),
            Some(Path::new("/home/alice")),
            None,
            true,
            &grants,
        ),
        ApprovalResolution::Prompt {
            restricted: true,
            ..
        }
    ));

    // 5. Windows drive-relative (`C:secrets\k`) and drive-root-relative (`\vault\k`) resolve
    //    against `cwd`'s drive/path.
    let win_cwd = Path::new(r"C:\repo");
    assert_eq!(
        normalize_target(win_cwd, r"\vault\k", None, None),
        "c:/vault/k"
    );
    assert_eq!(
        normalize_target(win_cwd, r"C:secrets\k", None, None),
        "c:/repo/secrets/k"
    );
    assert_eq!(
        normalize_target(win_cwd, r"D:other\k", None, None),
        "d:/other/k"
    );
}

#[test]
fn session_grants_are_isolated_per_session() {
    let grants = GrantCache::memory();
    let policy = UserApprovalPolicy::default();
    let target = vec!["/repo/src/a.rs".to_owned()];
    grants
        .record_for_session(
            "sess-a",
            ApprovalGrantScope::ToolSession,
            "edit",
            ToolTier::Write,
            &target,
        )
        .expect("record");

    // Covered in sess-a, NOT in sess-b!
    assert_eq!(
        resolve_for_session(
            "sess-a",
            "edit",
            &ToolApproval::write(),
            &policy,
            PermissionMode::Default,
            &target,
            Path::new("/repo"),
            None,
            None,
            true,
            &grants,
        ),
        ApprovalResolution::Allow
    );
    assert!(matches!(
        resolve_for_session(
            "sess-b",
            "edit",
            &ToolApproval::write(),
            &policy,
            PermissionMode::Default,
            &target,
            Path::new("/repo"),
            None,
            None,
            true,
            &grants,
        ),
        ApprovalResolution::Prompt { .. }
    ));

    grants.clear_session("sess-a");
    assert!(matches!(
        resolve_for_session(
            "sess-a",
            "edit",
            &ToolApproval::write(),
            &policy,
            PermissionMode::Default,
            &target,
            Path::new("/repo"),
            None,
            None,
            true,
            &grants,
        ),
        ApprovalResolution::Prompt { .. }
    ));
}

#[test]
fn eval_load_magic_extracts_target_and_triggers_restricted_folder() {
    let args = serde_json::json!({
        "language": "py",
        "code": "%load /secret/foo.py"
    });
    assert_eq!(extract_targets("eval", &args), vec!["/secret/foo.py"]);

    let quoted = serde_json::json!({
        "language": "py",
        "code": "x = 1\n%load '/secret/bar.py'"
    });
    assert_eq!(extract_targets("eval", &quoted), vec!["/secret/bar.py"]);

    let (_, restricted) =
        normalized_targets_for_call("eval", &args, Path::new("/repo"), None, None);
    let hit = matching_restricted_folder_for_tool(
        "eval",
        &["/secret".to_owned()],
        &restricted,
        Path::new("/repo"),
        None,
        None,
    );
    assert_eq!(hit.as_deref(), Some("/secret"));
}
