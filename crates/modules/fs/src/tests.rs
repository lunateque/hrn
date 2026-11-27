use super::*;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::testing::{TempDir, tool_context};
use hrn_kernel::{Host, Registered, Shared, Tool};
use std::collections::BTreeMap;

fn registered_with(config: Value, shared: Arc<Shared>) -> Registered {
    let table: BTreeMap<String, Value> = match config {
        Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    };
    let host = Host {
        cwd: ".".into(),
        data_dir: ".".into(),
    };
    Registry::new(host, table, shared)
        .module("fs", register)
        .and_then(Registry::finish)
        .expect("fs registers")
}

fn registered(config: Value) -> Registered {
    registered_with(config, Arc::new(Shared::new()))
}

fn names(registered: &Registered) -> Vec<String> {
    registered.tools.iter().map(|t| t.spec().name).collect()
}

fn tool<'a>(registered: &'a Registered, name: &str) -> &'a Arc<dyn Tool> {
    registered
        .tools
        .iter()
        .find(|t| t.spec().name == name)
        .expect("tool registered")
}

#[test]
fn fs_registers_file_tools_without_edit_and_keeps_legacy_edit_mode_ignored() {
    let default = registered(json!({}));
    assert_eq!(
        names(&default),
        ["read", "write", "apply_patch", "grep", "glob"]
    );
    // `local://` scratch and `conflict://` hunks.
    assert_eq!(default.schemes.len(), 2);

    // Legacy `[fs] edit_mode` is tolerated cleanly without crashing older configs.
    let legacy = registered(json!({"fs": {"edit_mode": "replace"}}));
    assert_eq!(names(&legacy), names(&default));
}

#[test]
fn an_unknown_fs_key_is_refused() {
    let host = Host {
        cwd: ".".into(),
        data_dir: ".".into(),
    };
    let table = BTreeMap::from([("fs".to_owned(), json!({"edit_modes": "replace"}))]);
    let err = Registry::new(host, table, Arc::new(Shared::new()))
        .module("fs", register)
        .err()
        .expect("typo refused")
        .to_string();
    assert!(err.contains("edit_modes"), "{err}");
}

#[tokio::test]
async fn read_and_grep_follow_shared_edit_mode_hint_and_share_tracker() {
    let dir = TempDir::new("fs-module");
    let shared = Arc::new(Shared::new());
    let hint = shared
        .get_or_init(|| Ok(EditModeHint::new()))
        .expect("hint");
    let reg = registered_with(json!({}), Arc::clone(&shared));
    let ctx = tool_context(dir.path().to_path_buf());
    tool(&reg, "write")
        .execute(&ctx, json!({"path": "a.txt", "content": "hello world\n"}))
        .await
        .expect("write");

    let with_anchors = tool(&reg, "read")
        .execute(&ctx, json!({"path": "a.txt"}))
        .await
        .expect("read");
    assert!(
        with_anchors.content.contains("[a.txt#"),
        "{}",
        with_anchors.content
    );

    hint.set_hashline_anchors(false);
    let without_anchors = tool(&reg, "read")
        .execute(&ctx, json!({"path": "a.txt"}))
        .await
        .expect("read no anchors");
    assert!(
        !without_anchors.content.contains("[a.txt#"),
        "{}",
        without_anchors.content
    );
    let grep_plain = tool(&reg, "grep")
        .execute(&ctx, json!({"pattern": "world", "path": "a.txt"}))
        .await
        .expect("grep");
    assert!(
        grep_plain.content.contains("*1|hello world"),
        "{}",
        grep_plain.content
    );
}

#[tokio::test]
async fn write_and_apply_patch_enforce_shared_edit_guards() {
    let dir = TempDir::new("fs-edit-guards");
    std::fs::write(dir.path().join("Cargo.lock"), "version = 3\n").expect("lock");
    std::fs::write(dir.path().join("src.rs"), "line 1\nline 2\nline 3\n").expect("src");
    let shared = Arc::new(Shared::new());
    let hint = shared
        .get_or_init(|| Ok(EditModeHint::new()))
        .expect("hint");
    hint.set_edit_policy(true, vec!["Cargo.lock".to_owned()], true, 0.95);
    let reg = registered_with(json!({}), Arc::clone(&shared));
    let ctx = tool_context(dir.path().to_path_buf());

    // `write` refuses auto-generated files via shared EditModeHint.
    let write_err = tool(&reg, "write")
        .execute(
            &ctx,
            json!({"path": "Cargo.lock", "content": "version = 4\n"}),
        )
        .await
        .expect_err("write must refuse generated file")
        .to_string();
    assert!(write_err.contains("Cargo.lock"), "{write_err}");

    // `apply_patch` refuses both auto-generated files and unread files.
    let patch_gen = "*** Begin Patch\n*** Update File: Cargo.lock\n@@\n-version = 3\n+version = 4\n*** End Patch";
    let err_gen = tool(&reg, "apply_patch")
        .execute(&ctx, json!({"patch": patch_gen}))
        .await
        .expect_err("apply_patch must refuse generated file")
        .to_string();
    assert!(err_gen.contains("Cargo.lock"), "{err_gen}");

    let patch_unread =
        "*** Begin Patch\n*** Update File: src.rs\n@@\n-line 2\n+LINE 2\n*** End Patch";
    let err_unread = tool(&reg, "apply_patch")
        .execute(&ctx, json!({"patch": patch_unread}))
        .await
        .expect_err("apply_patch must refuse unread file")
        .to_string();
    assert!(err_unread.contains("unseen"), "{err_unread}");

    // After reading `src.rs`, `apply_patch` succeeds.
    tool(&reg, "read")
        .execute(&ctx, json!({"path": "src.rs"}))
        .await
        .expect("read");
    tool(&reg, "apply_patch")
        .execute(&ctx, json!({"patch": patch_unread}))
        .await
        .expect("apply_patch after read");
    assert!(
        std::fs::read_to_string(dir.path().join("src.rs"))
            .expect("read back")
            .contains("LINE 2")
    );
}

#[tokio::test]
async fn write_unknown_scheme_refuses_without_touching_disk() {
    let dir = TempDir::new("fs-unknown-scheme");
    let reg = registered(json!({}));
    let mut ctx = tool_context(dir.path().to_path_buf());
    let mut urls = hrn_kernel::UrlRouter::new();
    for scheme in &reg.schemes {
        urls = urls.try_with_module(Arc::clone(scheme)).expect("scheme");
    }
    ctx.urls = Arc::new(urls);

    let err = tool(&reg, "write")
        .execute(&ctx, json!({"path": "foo://bar", "content": "nope"}))
        .await
        .expect_err("unknown scheme must fail")
        .to_string();
    assert!(err.contains("unknown URL scheme `foo://`"), "{err}");
    assert!(err.contains("known: conflict, local"), "{err}");
    assert!(!err.contains("web_fetch"), "{err}");
    assert!(
        std::fs::read_dir(dir.path())
            .expect("read_dir")
            .next()
            .is_none(),
        "must not create files on disk for unknown URL schemes"
    );
}

#[test]
fn write_out_scheme_uses_read_approval() {
    let reg = registered(json!({}));
    let write = tool(&reg, "write");
    assert_eq!(
        write.approval(&json!({"path": "out://note", "content": "hi"})),
        hrn_kernel::ToolApproval::read()
    );
    assert_eq!(
        write.approval(&json!({"path": "plain.txt", "content": "hi"})),
        hrn_kernel::ToolApproval::write()
    );
}
