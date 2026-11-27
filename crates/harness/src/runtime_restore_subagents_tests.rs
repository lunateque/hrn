//! Acceptance test for Wave 10b subagent crash restore (`restore_revives_mid_turn_subagents_first`)
//! and omp-style restore flash formatting.

use crate::agents::{AgentRegistry, JobStatus, Spawn};
use crate::artifacts::{ArtifactRecord, ArtifactStore};
use crate::control::SessionControl;
use crate::registry::ToolRegistry;
use crate::runtime::{AgentRuntime, HarnessConfig, TurnIo};
use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents, block_on};
use crate::{AgentUsage, TreeRestoreSummary};
use hrn_kernel::{
    AgentStatus, BoxFuture, CancelFlag, DagNode, DagPayload, ExitReason, FinishReason, LlmResponse,
    Msg, PermissionMode, Result, SessionStore, ThinkingLevel, Tool, ToolApproval, ToolCall,
    ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct RecordingTool {
    runs: Arc<AtomicUsize>,
}

impl Tool for RecordingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "probe".into(),
            description: "probe tool".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }
    fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
        let runs = Arc::clone(&self.runs);
        Box::pin(async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::ok("probe-ok"))
        })
    }
}

#[derive(Default)]
struct ManualSpawn(Mutex<Vec<BoxFuture<()>>>);
impl Spawn for ManualSpawn {
    fn spawn(&self, task: BoxFuture<()>) -> Result {
        self.0.lock().unwrap().push(task);
        Ok(())
    }
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("hrn-sub-restore-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn append_node(store: &MemStore, sid: &str, id: &str, parent: Option<&str>, payload: DagPayload) {
    store
        .append_node(&DagNode {
            id: id.into(),
            session_id: sid.into(),
            parent_id: parent.map(Into::into),
            created_at_ms: 100,
            payload,
        })
        .unwrap();
    let mut meta = store.get_session(sid).unwrap().unwrap();
    meta.head_node_id = Some(id.into());
    store.update_session(&meta).unwrap();
}

fn text_resp(text: &str) -> LlmResponse {
    LlmResponse {
        content: text.into(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        usage: None,
        model_used: "mock".into(),
        fallback_from: None,
        finish_reason: FinishReason::Stop,
    }
}

fn seed_root_and_crashed_child(
    runtime: &AgentRuntime,
    store: &MemStore,
) -> hrn_kernel::SessionMeta {
    let mut root = runtime
        .create_session(
            "root",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .unwrap();
    root.status = AgentStatus::Working;
    store.update_session(&root).unwrap();
    append_node(
        store,
        &root.id,
        "r1",
        None,
        DagPayload::Message {
            turn_id: "t1".into(),
            msg: Msg::user("coordinate"),
        },
    );

    // Child 1 (`scout`): left mid-turn with an unanswered tool call (must be cold-revived!).
    let mut child_crashed = runtime
        .create_child_session(&root, None, "subagent:scout", None, PermissionMode::Yolo)
        .unwrap();
    child_crashed.status = AgentStatus::Working;
    store.update_session(&child_crashed).unwrap();
    append_node(
        store,
        &child_crashed.id,
        "c1",
        None,
        DagPayload::Message {
            turn_id: "ct1".into(),
            msg: Msg::user("scan files"),
        },
    );
    append_node(
        store,
        &child_crashed.id,
        "c2",
        Some("c1"),
        DagPayload::Message {
            turn_id: "ct1".into(),
            msg: Msg::assistant_tool_calls(
                String::new(),
                None,
                vec![ToolCall {
                    id: "tc1".into(),
                    name: "probe".into(),
                    arguments: "{}".into(),
                    signature: None,
                }],
            ),
        },
    );
    root
}

fn seed_skipped_children(
    runtime: &AgentRuntime,
    store: &MemStore,
    artifacts: &ArtifactStore,
    root: &hrn_kernel::SessionMeta,
) {
    // Child 2 (`esc-aborted`): interrupted by user Esc (`ExitReason::Interrupted`) -> MUST be skipped.
    let mut child_esc = runtime
        .create_child_session(root, None, "subagent:plan", None, PermissionMode::Yolo)
        .unwrap();
    child_esc.status = AgentStatus::Idle;
    store.update_session(&child_esc).unwrap();
    append_node(
        store,
        &child_esc.id,
        "e1",
        None,
        DagPayload::Message {
            turn_id: "et1".into(),
            msg: Msg::user("draft plan"),
        },
    );
    append_node(
        store,
        &child_esc.id,
        "e2",
        Some("e1"),
        DagPayload::SessionExit {
            reason: ExitReason::Interrupted,
            pending: Vec::new(),
        },
    );

    // Child 3 (`tombstoned`): tombstoned as `Aborted` in `.meta` -> MUST be skipped.
    let mut child_tomb = runtime
        .create_child_session(root, None, "subagent:reviewer", None, PermissionMode::Yolo)
        .unwrap();
    child_tomb.status = AgentStatus::Working;
    store.update_session(&child_tomb).unwrap();
    append_node(
        store,
        &child_tomb.id,
        "k1",
        None,
        DagPayload::Message {
            turn_id: "kt1".into(),
            msg: Msg::user("audit"),
        },
    );
    artifacts
        .write_meta(
            &root.id,
            &ArtifactRecord {
                id: "reviewer".into(),
                name: "reviewer".into(),
                profile: "reviewer".into(),
                owner: root.id.clone(),
                root: root.id.clone(),
                parent: None,
                task: "audit".into(),
                session_id: Some(child_tomb.id.clone()),
                status: JobStatus::Aborted,
                started_ms: 100,
                finished_ms: 200,
                generation: 1,
                usage: AgentUsage::default(),
                progress_notes: Vec::new(),
                patch_path: None,
                isolated_branch: None,
            },
        )
        .unwrap();
}

fn seed_crashed_tree(
    runtime: &AgentRuntime,
    store: &MemStore,
    artifacts: &ArtifactStore,
) -> hrn_kernel::SessionMeta {
    let root = seed_root_and_crashed_child(runtime, store);
    seed_skipped_children(runtime, store, artifacts, &root);
    root
}

fn assert_flash_messages() {
    assert_eq!(
        TreeRestoreSummary {
            main_interrupted: true,
            subagent_count: 2,
        }
        .flash_message(),
        "restored: going on with the interrupted turn and 2 subagents."
    );
    assert_eq!(
        TreeRestoreSummary {
            main_interrupted: false,
            subagent_count: 2,
        }
        .flash_message(),
        "restored: 2 subagents going on with their interrupted turns."
    );
    assert_eq!(
        TreeRestoreSummary {
            main_interrupted: true,
            subagent_count: 0,
        }
        .flash_message(),
        "restored: going on with the interrupted turn…"
    );
    assert_eq!(
        TreeRestoreSummary {
            main_interrupted: false,
            subagent_count: 0,
        }
        .flash_message(),
        "restored: nothing was left unfinished"
    );
}

#[test]
fn restore_revives_mid_turn_subagents_first() {
    let tmp = TempDir::new("first");
    let artifacts = Arc::new(ArtifactStore::new(tmp.0.clone()));
    let store = Arc::new(MemStore::default());
    let probe_runs = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(RecordingTool {
        runs: Arc::clone(&probe_runs),
    }));

    let llm = Arc::new(ScriptedLlm {
        responses: Mutex::new(VecDeque::from(vec![
            Ok(text_resp("main resumed")),
            Ok(text_resp("scout resumed after crash")),
        ])),
        ..ScriptedLlm::default()
    });
    let spawned = Arc::new(ManualSpawn::default());
    let registry = Arc::new(AgentRegistry::new(spawned.clone()).with_artifacts(artifacts.clone()));
    let runtime = AgentRuntime::new(
        llm,
        store.clone(),
        tools,
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    )
    .with_agents(registry.clone());

    let root = seed_crashed_tree(&runtime, &store, &artifacts);

    assert_eq!(
        registry.resurrect_tree(&root.id, store.as_ref()).unwrap(),
        3
    );
    assert_eq!(runtime.count_interrupted_subagents(&root.id).unwrap(), 1);

    let io = TurnIo {
        cancel: CancelFlag::new(),
        events: Arc::new(VecEvents::default()),
        approval: None,
        ask: None,
    };
    let (revived_ids, main_done) =
        block_on(runtime.restore_tree(&root.id, &SessionControl::default(), io)).unwrap();
    assert_eq!(revived_ids, vec!["scout"]);
    assert_eq!(main_done.expect("main restored").final_text, "main resumed");

    let jobs = std::mem::take(&mut *spawned.0.lock().unwrap());
    assert_eq!(
        jobs.len(),
        1,
        "only the non-tombstoned crashed child spawned"
    );
    for job in jobs {
        block_on(job);
    }
    assert_eq!(
        probe_runs.load(Ordering::SeqCst),
        1,
        "child replayed cut-off tool"
    );
    let scout_rec = registry.get_in_tree(&root.id, "scout").unwrap();
    assert_eq!(scout_rec.snapshot().status, JobStatus::Idle);
    assert_eq!(scout_rec.snapshot().output, "scout resumed after crash");
    let deliveries = registry.inbox().take(&root.id);
    assert_eq!(deliveries.len(), 1);
    assert!(deliveries[0].text.contains("scout resumed after crash"));

    assert_flash_messages();
}
