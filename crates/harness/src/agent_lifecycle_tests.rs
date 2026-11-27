//! Wave 9c `AgentLifecycle` acceptance tests: idle follow-up, TTL parking + artifact retention,
//! revival from parked & aborted, nested `agent://<parent>.<child>`, JSON pointer extraction,
//! `agent://all` subagent refusal, `?progress` notes, and startup resurrection.

use crate::agents::AgentScheme;
use crate::agents::{AgentRegistry, JobStatus, Spawn};
use crate::artifacts::ArtifactStore;
use crate::proc_urls::ProcScheme;
use crate::registry::ToolRegistry;
use crate::runtime::{AgentRuntime, HarnessConfig};
use crate::subagent::SubagentTool;
use crate::task_config::TaskConfig;
use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents, block_on};
use crate::think_yield::YieldTool;
use hrn_kernel::{
    BoxFuture, CancelFlag, Clock, FinishReason, LlmResponse, PermissionMode, Result, SessionMeta,
    ThinkingLevel, Tool, ToolCall, ToolContext, UrlBody, UrlScheme, Usage,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct FakeClock(AtomicI64);
impl FakeClock {
    fn new(start_ms: i64) -> Self {
        Self(AtomicI64::new(start_ms))
    }
    fn advance(&self, delta_ms: i64) {
        self.0.fetch_add(delta_ms, Ordering::SeqCst);
    }
}
impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
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

fn text_reply(text: &str) -> LlmResponse {
    LlmResponse {
        model_used: "m".into(),
        fallback_from: None,
        content: text.into(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        usage: Some(Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cost_micros: Some(250),
            ..Usage::default()
        }),
        finish_reason: FinishReason::Stop,
    }
}

fn tool_reply(call_id: &str, name: &str, args: &Value) -> LlmResponse {
    LlmResponse {
        model_used: "m".into(),
        fallback_from: None,
        content: String::new(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            id: call_id.into(),
            name: name.into(),
            arguments: args.to_string(),
            signature: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 40,
            completion_tokens: 20,
            total_tokens: 60,
            cost_micros: Some(100),
            ..Usage::default()
        }),
        finish_reason: FinishReason::ToolCalls,
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hrn-lc-{tag}-{}-{n}", std::process::id()));
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

struct Harness {
    rt: AgentRuntime,
    llm: Arc<ScriptedLlm>,
    store: Arc<MemStore>,
    spawned: Arc<ManualSpawn>,
    clock: Arc<FakeClock>,
    artifacts: Arc<ArtifactStore>,
    parent: SessionMeta,
    _tmp: TempDir,
}

impl Harness {
    fn new(tag: &str, responses: Vec<LlmResponse>, task: TaskConfig) -> Self {
        let tmp = TempDir::new(tag);
        let clock = Arc::new(FakeClock::new(1_000_000));
        let artifacts = Arc::new(ArtifactStore::new(tmp.0.clone()));
        let llm = Arc::new(ScriptedLlm::default());
        llm.responses
            .lock()
            .unwrap()
            .extend(responses.into_iter().map(Ok));
        let store = Arc::new(MemStore::default());
        let spawned = Arc::new(ManualSpawn::default());
        let registry = Arc::new(
            AgentRegistry::new(spawned.clone())
                .with_clock(clock.clone())
                .with_idle_ttl_ms(task.agent_idle_ttl_ms)
                .with_artifacts(artifacts.clone()),
        );
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(YieldTool));
        let rt = AgentRuntime::new(
            llm.clone(),
            store.clone(),
            tools,
            Arc::new(MemFs::default()),
            HarnessConfig {
                task,
                ..HarnessConfig::default()
            },
        )
        .with_clock(clock.clone())
        .with_agents(registry);
        let parent = rt
            .create_session(
                "root session",
                Path::new("/ws"),
                None,
                ThinkingLevel::Off,
                PermissionMode::Yolo,
            )
            .unwrap();
        Self {
            rt,
            llm,
            store,
            spawned,
            clock,
            artifacts,
            parent,
            _tmp: tmp,
        }
    }

    fn main_ctx(&self) -> ToolContext {
        ToolContext {
            session_id: self.parent.id.clone(),
            turn_id: "t".into(),
            call_id: "c".into(),
            cwd: "/ws".into(),
            permission_mode: PermissionMode::Yolo,
            cancel: CancelFlag::new(),
            events: Arc::new(VecEvents::default()),
            ask: None,
            approval: None,
            urls: Arc::default(),
            root_session_id: self.parent.id.clone(),
            inbox: Arc::clone(self.rt.agents().inbox()),
            caller: None,
            todos: Arc::clone(self.rt.todos()),
            yield_data: Arc::default(),
            is_subagent: false,
        }
    }

    fn spawn(&self, args: &Value) {
        let out =
            block_on(SubagentTool::new(self.rt.clone()).execute(&self.main_ctx(), args.clone()))
                .unwrap();
        assert!(!out.is_error, "spawn failed: {}", out.content);
    }

    fn run_all(&self) {
        loop {
            let jobs = std::mem::take(&mut *self.spawned.0.lock().unwrap());
            if jobs.is_empty() {
                break;
            }
            for job in jobs {
                block_on(job);
            }
        }
    }

    fn agent_scheme(&self) -> AgentScheme {
        AgentScheme::new(Arc::clone(self.rt.agents()))
    }
}

#[test]
fn idle_agent_accepts_followup() {
    let h = Harness::new(
        "idle-followup",
        vec![
            text_reply("initial findings"),
            text_reply("added tests too"),
        ],
        TaskConfig::default(),
    );
    h.spawn(&json!({"context": "ctx", "tasks": [{"name": "alpha", "task": "inspect code", "solutionSpace": "scan"}]}));
    h.run_all();

    let rec = h.rt.agents().get("alpha").unwrap();
    let snap1 = rec.snapshot();
    assert_eq!(
        (snap1.status, snap1.generation, snap1.requests),
        (JobStatus::Idle, 1, 1)
    );
    assert_eq!((snap1.input_tokens, snap1.output_tokens), (100, 50));
    assert!(rec.has_live_runtime(), "idle agent keeps live runtime");

    let items1 = h.rt.agents().inbox().take(&h.parent.id);
    assert_eq!(items1.len(), 1);
    assert!(items1[0].text.contains("initial findings"));

    h.agent_scheme()
        .write(&h.main_ctx(), "alpha", "also add tests")
        .unwrap();
    assert_eq!(
        (rec.snapshot().status, rec.snapshot().generation),
        (JobStatus::Running, 2)
    );

    h.run_all();
    let snap2 = rec.snapshot();
    assert_eq!(
        (snap2.status, snap2.generation, snap2.requests),
        (JobStatus::Idle, 2, 2)
    );
    assert_eq!((snap2.input_tokens, snap2.output_tokens), (200, 100));
    assert_eq!(snap2.output, "added tests too");

    let reqs = h.llm.requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    assert!(
        reqs[1]
            .messages
            .iter()
            .any(|m| m.content.contains("initial findings"))
    );
    assert!(
        reqs[1]
            .messages
            .last()
            .unwrap()
            .content
            .contains("also add tests")
    );
    let items2 = h.rt.agents().inbox().take(&h.parent.id);
    assert_eq!(items2.len(), 1);
    assert!(
        items2[0].text.contains("added tests too") && items2[0].text.contains("generation=\"2\"")
    );
}

#[test]
fn park_after_ttl_keeps_artifact() {
    let ttl = 420_000u64;
    let h = Harness::new(
        "park-ttl",
        vec![text_reply("durable markdown report")],
        TaskConfig {
            agent_idle_ttl_ms: ttl,
            ..TaskConfig::default()
        },
    );
    let main = h.rt.agents().register_main(&h.parent.id, "main session");
    h.spawn(&json!({"context": "ctx", "tasks": [{"name": "worker", "task": "write report", "solutionSpace": "s"}]}));
    h.run_all();

    let rec = h.rt.agents().get("worker").unwrap();
    assert_eq!(rec.snapshot().status, JobStatus::Idle);
    assert!(rec.has_live_runtime());
    assert!(rec.snapshot().artifact_path.expect("artifact").is_file());

    h.clock.advance(i64::try_from(ttl).unwrap() - 1);
    assert!(h.rt.agents().sweep_idle().is_empty());
    assert_eq!(rec.snapshot().status, JobStatus::Idle);

    h.clock.advance(1);
    assert_eq!(h.rt.agents().sweep_idle(), vec!["worker"]);
    assert_eq!(rec.snapshot().status, JobStatus::Parked);
    assert!(!rec.has_live_runtime(), "parked agent drops live runtime");
    assert_eq!(
        main.snapshot().status,
        JobStatus::Idle,
        "Main is never parked"
    );

    assert_eq!(
        h.artifacts
            .read_md(&h.parent.id, "worker")
            .unwrap()
            .as_deref(),
        Some("durable markdown report")
    );
    let UrlBody::Text(detail) = h.agent_scheme().read(&h.main_ctx(), "worker").unwrap() else {
        panic!("text expected");
    };
    assert!(detail.contains("status: parked") && detail.contains("durable markdown report"));
}

#[test]
fn revive_parked_to_idle() {
    let h = Harness::new(
        "revive-parked",
        vec![
            text_reply("gen 1 answer"),
            text_reply("gen 2 revived answer"),
        ],
        TaskConfig {
            agent_idle_ttl_ms: 10_000,
            ..TaskConfig::default()
        },
    );
    h.spawn(&json!({"context": "ctx", "tasks": [{"name": "analyst", "task": "analyze", "solutionSpace": "s"}]}));
    h.run_all();
    h.clock.advance(15_000);
    assert_eq!(h.rt.agents().sweep_idle(), vec!["analyst"]);

    let rec = h.rt.agents().get("analyst").unwrap();
    assert_eq!(rec.snapshot().status, JobStatus::Parked);
    assert!(!rec.has_live_runtime());

    h.agent_scheme()
        .write(&h.main_ctx(), "analyst", "dig deeper")
        .unwrap();
    assert_eq!(
        (rec.snapshot().status, rec.snapshot().generation),
        (JobStatus::Running, 2)
    );
    assert!(rec.has_live_runtime());

    h.run_all();
    let snap = rec.snapshot();
    assert_eq!((snap.status, snap.generation), (JobStatus::Idle, 2));
    assert_eq!(snap.output, "gen 2 revived answer");
    assert_eq!(
        h.artifacts
            .read_md(&h.parent.id, "analyst")
            .unwrap()
            .as_deref(),
        Some("gen 2 revived answer")
    );
    let reqs = h.llm.requests.lock().unwrap();
    assert!(
        reqs[1]
            .messages
            .iter()
            .any(|m| m.content.contains("gen 1 answer"))
    );
}

#[test]
fn kill_marks_aborted_and_revive_works() {
    let h = Harness::new(
        "kill-revive",
        vec![text_reply("back from tombstone")],
        TaskConfig::default(),
    );
    h.spawn(&json!({"context": "ctx", "tasks": [{"name": "runner", "task": "long job", "solutionSpace": "s"}]}));
    let procs = ProcScheme::new(Arc::clone(h.rt.procs()));
    procs.write(&h.main_ctx(), "runner/kill", "").unwrap();

    let rec = h.rt.agents().get("runner").unwrap();
    assert_eq!(rec.snapshot().status, JobStatus::Aborted);
    assert!(!rec.has_live_runtime());
    h.run_all();
    assert_eq!(rec.snapshot().status, JobStatus::Aborted);
    assert!(h.llm.requests.lock().unwrap().is_empty());

    h.agent_scheme()
        .write(&h.main_ctx(), "runner", "resume cleanly")
        .unwrap();
    assert_eq!(
        (rec.snapshot().status, rec.snapshot().generation),
        (JobStatus::Running, 2)
    );
    assert!(!rec.cancel_flag().is_cancelled());

    h.run_all();
    let snap = rec.snapshot();
    assert_eq!((snap.status, snap.generation), (JobStatus::Idle, 2));
    assert_eq!(snap.output, "back from tombstone");
}

#[test]
fn nested_agent_url_resolves() {
    let h = Harness::new(
        "nested-url",
        vec![
            tool_reply(
                "call_1",
                "task",
                &json!({"context": "nested", "tasks": [{"name": "worker", "agent": "explore", "task": "scan", "solutionSpace": "s"}]}),
            ),
            text_reply("waiting for worker"),
            text_reply("nested leaf payload"),
            text_reply("lead summary complete"),
        ],
        TaskConfig::default(),
    );
    h.spawn(&json!({"context": "ctx", "tasks": [{"name": "lead", "agent": "task", "task": "coordinate", "solutionSpace": "s"}]}));

    let mut lead_fut = h.spawned.0.lock().unwrap().remove(0);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(lead_fut.as_mut().poll(&mut cx).is_pending());
    assert!(h.rt.agents().get("lead.worker").is_some());
    h.run_all();
    block_on(lead_fut);

    let child = h.rt.agents().get("lead.worker").unwrap();
    assert_eq!(child.parent.as_deref(), Some("lead"));
    assert_eq!(child.snapshot().status, JobStatus::Idle);
    assert_eq!(child.snapshot().output, "nested leaf payload");

    let UrlBody::Text(body) = h.agent_scheme().read(&h.main_ctx(), "lead.worker").unwrap() else {
        panic!("text expected");
    };
    assert!(body.contains("agent lead.worker") && body.contains("parent: lead"));
    assert!(body.contains("nested leaf payload"));
}

#[test]
fn agent_json_path_extracts() {
    let payload = json!({
        "result": {
            "summary": "all green",
            "reports": [
                {"id": 1, "data": "first-finding"},
                {"id": 2, "data": "second-finding"}
            ]
        }
    });
    let h = Harness::new(
        "json-path",
        vec![tool_reply("c1", "yield", &payload), text_reply("done")],
        TaskConfig {
            agent_idle_ttl_ms: 5_000,
            ..TaskConfig::default()
        },
    );
    h.spawn(&json!({"context": "ctx", "tasks": [{"name": "auditor", "task": "audit", "solutionSpace": "s"}]}));
    h.run_all();
    assert!(
        h.artifacts
            .read_json(&h.parent.id, "auditor")
            .unwrap()
            .is_some()
    );

    let scheme = h.agent_scheme();
    let UrlBody::Text(v1) = scheme
        .read(&h.main_ctx(), "auditor/reports/0/data")
        .unwrap()
    else {
        panic!("text expected");
    };
    assert_eq!(v1, "first-finding");

    // Still extracts from on-disk `<id>.json` after parking.
    h.clock.advance(10_000);
    assert_eq!(h.rt.agents().sweep_idle(), vec!["auditor"]);
    let UrlBody::Text(v2) = scheme
        .read(&h.main_ctx(), "auditor/reports/1/data")
        .unwrap()
    else {
        panic!("text expected");
    };
    assert_eq!(v2, "second-finding");
    assert!(
        scheme
            .read(&h.main_ctx(), "auditor/reports/99/data")
            .is_err()
    );
}

#[test]
fn agent_all_refused_for_subagents() {
    let h = Harness::new("all-refused", vec![], TaskConfig::default());
    h.spawn(&json!({"context": "ctx", "tasks": [{"name": "a1", "task": "t1", "solutionSpace": "s"}, {"name": "a2", "task": "t2", "solutionSpace": "s"}]}));
    let scheme = h.agent_scheme();

    let mut sub_ctx = h.main_ctx();
    sub_ctx.is_subagent = true;
    sub_ctx.session_id = "child-sess".into();
    let err = scheme
        .write(&sub_ctx, "all", "stop everything")
        .expect_err("subagent must not broadcast");
    assert!(err.to_string().contains("main agent") || err.to_string().contains("subagent"));
    assert!(scheme.read(&sub_ctx, "all").is_err());

    scheme
        .write(&h.main_ctx(), "all", "focus on tests")
        .unwrap();
    assert_eq!(
        h.rt.agents().get("a1").unwrap().control.drain_steers(),
        ["focus on tests"]
    );
    assert_eq!(
        h.rt.agents().get("a2").unwrap().control.drain_steers(),
        ["focus on tests"]
    );
}

#[test]
fn progress_notes_and_resurrection_preserve_lineage() {
    let h = Harness::new(
        "resurrect",
        vec![
            text_reply("report body"),
            text_reply("revived after restart"),
        ],
        TaskConfig::default(),
    );
    h.spawn(&json!({"context": "ctx", "tasks": [{"name": "lead", "task": "build", "solutionSpace": "s"}]}));
    let scheme = h.agent_scheme();
    scheme
        .write(&h.main_ctx(), "lead?progress", "step 1: compiling")
        .unwrap();
    let UrlBody::Text(notes) = scheme.read(&h.main_ctx(), "lead?progress").unwrap() else {
        panic!("text expected");
    };
    assert_eq!(notes, "step 1: compiling");
    h.run_all();

    h.artifacts
        .write_output(&h.parent.id, "lead.sub", "sub output", None)
        .unwrap();

    let fresh_spawn = Arc::new(ManualSpawn::default());
    let fresh_reg = Arc::new(
        AgentRegistry::new(fresh_spawn.clone())
            .with_clock(h.clock.clone())
            .with_artifacts(h.artifacts.clone()),
    );
    let _fresh_rt = AgentRuntime::new(
        h.llm.clone(),
        h.store.clone(),
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    )
    .with_agents(fresh_reg.clone());

    assert_eq!(
        fresh_reg
            .resurrect_tree(&h.parent.id, h.store.as_ref())
            .unwrap(),
        2
    );
    let lead = fresh_reg.get_in_tree(&h.parent.id, "lead").unwrap();
    let sub = fresh_reg.get_in_tree(&h.parent.id, "lead.sub").unwrap();
    assert_eq!(lead.snapshot().status, JobStatus::Parked);
    assert_eq!(lead.snapshot().progress_notes, vec!["step 1: compiling"]);
    assert_eq!(
        (sub.snapshot().status, sub.parent.as_deref()),
        (JobStatus::Parked, Some("lead"))
    );

    assert_eq!(
        fresh_reg
            .revive(&h.parent.id, "lead", "continue work")
            .unwrap(),
        2
    );
    for job in std::mem::take(&mut *fresh_spawn.0.lock().unwrap()) {
        block_on(job);
    }
    assert_eq!(lead.snapshot().status, JobStatus::Idle);
    assert_eq!(lead.snapshot().output, "revived after restart");
}
