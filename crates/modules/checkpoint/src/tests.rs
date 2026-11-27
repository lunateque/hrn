//! Module tests on a real temporary directory: the tools' behaviour and the automatic snapshot
//! the hook takes.

use crate::{Config, DEFAULTS, Overrides, Shared, hook::AutoSnapshot, tools};
use hrn_edge_tools::Kind;
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::testing::{TempDir, tool_context as test_ctx};
use hrn_kernel::{
    HarnessHook, HrnError, Result, RewindTo, SessionEffect, Tool, ToolContext, ToolOutput,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn shared(dir: &Path) -> Arc<Shared> {
    let config: Config = serde_json::from_str(DEFAULTS).expect("defaults parse");
    Arc::new(Shared {
        config,
        root: dir.join("shadow"),
        busy: Mutex::new(()),
    })
}

fn ctx(dir: &Path) -> ToolContext {
    let mut ctx = test_ctx(dir.to_path_buf());
    ctx.session_id = "sess_t1".to_owned();
    ctx.root_session_id = ctx.session_id.clone();
    ctx
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
}

/// One tool call on a runtime of its own.
fn call(tool: &Arc<dyn Tool>, ctx: &ToolContext, args: Value) -> ToolOutput {
    runtime().block_on(tool.execute(ctx, args)).expect("call")
}

fn run_hook(hook: &AutoSnapshot, ctx: &ToolContext, tool: &str) -> Result {
    runtime().block_on(hook.before_mutating_tool(ctx, tool))
}

/// The tool's working directory, created.
fn work(dir: &TempDir) -> PathBuf {
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).expect("work dir");
    work
}

fn repo(shared: &Shared, work: &Path) -> hrn_edge_tools::Checkpoints {
    shared.writer(work).expect("shadow repo")
}

fn ids(shared: &Shared, work: &Path) -> Vec<String> {
    repo(shared, work)
        .list(None)
        .expect("list")
        .into_iter()
        .map(|snap| snap.id)
        .collect()
}

#[test]
fn a_rewind_restores_the_files_and_the_undo_takes_that_back() {
    let dir = TempDir::new("checkpoint");
    let work = work(&dir);
    let shared = shared(dir.path());
    std::fs::write(work.join("a.txt"), "first").expect("write");
    let ctx = ctx(&work);
    let checkpoint: Arc<dyn Tool> = Arc::new(tools::CheckpointTool(Arc::clone(&shared)));
    let rewind: Arc<dyn Tool> = Arc::new(tools::RewindTool(Arc::clone(&shared)));

    let made = call(&checkpoint, &ctx, json!({"goal": "try a rename"}));
    assert!(!made.is_error, "{}", made.content);
    let id = repo(&shared, &work)
        .active("sess_t1")
        .expect("active")
        .expect("a checkpoint is active");
    assert!(
        made.session_effect
            == Some(SessionEffect::Mark {
                label: format!("checkpoint:{id}")
            }),
        "{made:?}"
    );
    // A second checkpoint waits for the first one to be rewound.
    let twice = call(&checkpoint, &ctx, json!({"goal": "another"}));
    assert!(
        twice.is_error && twice.content.contains("already active"),
        "{twice:?}"
    );

    // A different length as well: a filesystem timestamp cannot see a same-size rewrite.
    std::fs::write(work.join("a.txt"), "second!").expect("write");
    std::fs::write(work.join("b.txt"), "new").expect("write");
    let back = call(
        &rewind,
        &ctx,
        json!({"report": "the rename is not worth it"}),
    );
    assert!(!back.is_error, "{}", back.content);
    assert_eq!(
        std::fs::read_to_string(work.join("a.txt")).expect("read"),
        "first"
    );
    assert!(!work.join("b.txt").exists());
    let rewound = match back.session_effect {
        Some(SessionEffect::Rewind { to, report }) => (to, report),
        other => panic!("no rewind effect: {other:?}"),
    };
    assert_eq!(rewound.0, RewindTo::Mark(format!("checkpoint:{id}")));
    assert!(
        rewound.1.contains("the rename is not worth it"),
        "{}",
        rewound.1
    );
    assert!(
        rewound.1.contains("Files restored to checkpoint"),
        "{}",
        rewound.1
    );
    assert!(
        repo(&shared, &work)
            .active("sess_t1")
            .expect("active")
            .is_none(),
        "the checkpoint is spent"
    );
    // The state the restore replaced was snapshotted first, so the restore is undoable.
    let (undo, _) = (
        ids(&shared, &work).first().cloned().expect("undo"),
        ids(&shared, &work).last().cloned().expect("checkpoint"),
    );
    repo(&shared, &work)
        .restore(&undo, None, None)
        .expect("undo");
    assert_eq!(
        std::fs::read_to_string(work.join("a.txt")).expect("read"),
        "second!"
    );
    assert!(work.join("b.txt").exists());
    assert_eq!(
        repo(&shared, &work)
            .list(Some(1))
            .expect("list")
            .first()
            .map(|s| s.kind),
        Some(Kind::Restore)
    );
}

#[test]
fn reports_and_targets_are_checked_before_anything_moves() {
    let dir = TempDir::new("checkpoint");
    let work = work(&dir);
    let shared = shared(dir.path());
    let ctx = ctx(&work);
    let rewind: Arc<dyn Tool> = Arc::new(tools::RewindTool(Arc::clone(&shared)));
    let out = call(&rewind, &ctx, json!({"report": "   "}));
    assert!(out.is_error && out.content.contains("empty"), "{out:?}");
    let out = call(&rewind, &ctx, json!({"report": "done"}));
    assert!(out.is_error && out.content.contains("No active"), "{out:?}");
    let out = call(
        &rewind,
        &ctx,
        json!({"report": "done", "checkpoint": "ffff"}),
    );
    assert!(
        out.is_error && out.content.contains("No checkpoint"),
        "{out:?}"
    );
    let out = call(
        &rewind,
        &ctx,
        json!({"report": "done", "checkpoint": "main"}),
    );
    assert!(
        out.is_error && out.content.contains("No checkpoint"),
        "{out:?}"
    );
    // A restore point records a state, not a place in the conversation.
    std::fs::write(work.join("a.txt"), "one").expect("write");
    let snapshot = repo(&shared, &work)
        .snapshot(Kind::Restore, "x", None, None)
        .expect("snapshot");
    let out = call(
        &rewind,
        &ctx,
        json!({"report": "done", "checkpoint": snapshot}),
    );
    assert!(out.is_error && out.content.contains("restore"), "{out:?}");
}

#[test]
fn the_hook_snapshots_once_per_turn_before_an_editing_tool() {
    let dir = TempDir::new("checkpoint");
    let work = work(&dir);
    let shared = shared(dir.path());
    let hook = AutoSnapshot::new(Arc::clone(&shared));
    let mut ctx = ctx(&work);
    std::fs::write(work.join("a.txt"), "one").expect("write");
    run_hook(&hook, &ctx, "edit").expect("hook");
    std::fs::write(work.join("a.txt"), "two").expect("write");
    run_hook(&hook, &ctx, "write").expect("hook");
    assert_eq!(ids(&shared, &work).len(), 1, "one snapshot per turn");
    // The next turn snapshots again; a tool outside `tools` never does.
    ctx.turn_id = "turn_2".to_owned();
    run_hook(&hook, &ctx, "todo").expect("hook");
    assert_eq!(ids(&shared, &work).len(), 1);
    std::fs::write(work.join("a.txt"), "three").expect("write");
    ctx.turn_id = "turn_3".to_owned();
    run_hook(&hook, &ctx, "bash").expect("hook");
    let labels: Vec<String> = repo(&shared, &work)
        .list(None)
        .expect("list")
        .into_iter()
        .map(|snap| snap.label)
        .collect();
    assert_eq!(labels, ["before `bash`", "before `edit`"]);
}

#[test]
fn a_directory_that_cannot_be_snapshotted_leaves_the_turn_running() {
    let dir = TempDir::new("checkpoint");
    let shared = shared(dir.path());
    let hook = AutoSnapshot::new(Arc::clone(&shared));
    // A work tree that does not exist: the edit still runs, and reading it reports the reason.
    let ctx = ctx(&dir.path().join("gone"));
    run_hook(&hook, &ctx, "edit").expect("the edit still runs");
    assert!(matches!(shared.reader(&ctx.cwd), Err(HrnError::Io(_))));
}

#[test]
fn config_defaults_and_overrides() {
    let base = Config::resolve(Overrides::default()).expect("defaults parse");
    assert!(base.auto && base.timeout_secs > 0 && !base.tools.is_empty());
    let off = Config::resolve(Overrides {
        auto: Some(false),
        ..Overrides::default()
    })
    .expect("override parses");
    assert!(!off.auto && off.tools == base.tools);
    let bad = serde_json::from_str::<Overrides>(&json!({"autoo": true}).to_string());
    assert!(bad.is_err(), "an unknown key is refused");
}
