//! Background-agent flow: spawn, inbox delivery, `wait`, steering, cancellation, and config limits.

use super::*;
use crate::agents::AgentScheme;
use crate::agents::{AgentRegistry, JobStatus, Spawn};
use crate::control::SessionControl;
use crate::proc_urls::ProcScheme;
use crate::runtime::{HarnessConfig, TurnIo};
use crate::task_config::TaskConfig;
use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents, block_on};
use hrn_kernel::{EventSink, FinishReason, LlmResponse, ThinkingLevel, UrlBody, UrlScheme};
use serde_json::json;
use std::path::Path;

/// Collects spawned futures; the test decides when they run.
#[derive(Default)]
struct ManualSpawn(Mutex<Vec<BoxFuture<()>>>);

impl Spawn for ManualSpawn {
    fn spawn(&self, task: BoxFuture<()>) -> Result {
        self.0.lock().unwrap().push(task);
        Ok(())
    }
}

struct Fixture {
    rt: AgentRuntime,
    llm: Arc<ScriptedLlm>,
    spawned: Arc<ManualSpawn>,
    parent: SessionMeta,
}

fn reply(text: &str) -> LlmResponse {
    LlmResponse {
        model_used: "m".into(),
        fallback_from: None,
        content: text.into(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        usage: None,
        finish_reason: FinishReason::Stop,
    }
}

fn fixture_with_task(script: &[&str], task: TaskConfig) -> Fixture {
    let llm = Arc::new(ScriptedLlm::default());
    llm.responses
        .lock()
        .unwrap()
        .extend(script.iter().map(|t| Ok(reply(t))));
    let spawned = Arc::new(ManualSpawn::default());
    let rt = AgentRuntime::new(
        llm.clone(),
        Arc::new(MemStore::default()),
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            task,
            ..HarnessConfig::default()
        },
    )
    .with_agents(Arc::new(AgentRegistry::new(spawned.clone())));
    let parent = rt
        .create_session(
            "t",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .unwrap();
    Fixture {
        rt,
        llm,
        spawned,
        parent,
    }
}

fn fixture(script: &[&str]) -> Fixture {
    fixture_with_task(script, TaskConfig::default())
}

impl Fixture {
    fn ctx(&self) -> ToolContext {
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

    fn spawn(&self, args: &Value) -> Result<ToolOutput> {
        block_on(SubagentTool::new(self.rt.clone()).execute(&self.ctx(), args.clone()))
    }

    fn wait(&self) -> String {
        let wait = WaitTool::new(Arc::clone(self.rt.procs()));
        block_on(wait.execute(&self.ctx(), json!({})))
            .unwrap()
            .content
    }

    /// Run every future spawned so far.
    fn run_spawned(&self) {
        let jobs = std::mem::take(&mut *self.spawned.0.lock().unwrap());
        for job in jobs {
            block_on(job);
        }
    }
}

#[test]
fn spawn_returns_ids_at_once_and_the_result_arrives_through_the_inbox() {
    let f = fixture(&["child says hi"]);
    let out = f
        .spawn(&json!({"context": "shared", "tasks": [{"name": "alpha", "task": "look"}]}))
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("`alpha`"), "{}", out.content);
    let inbox = f.rt.agents().inbox();
    assert_eq!(
        (inbox.pending(&f.parent.id), inbox.queued(&f.parent.id)),
        (1, 0)
    );
    assert!(f.llm.requests.lock().unwrap().is_empty(), "nothing ran yet");

    f.run_spawned();
    let agent = f.rt.agents().get("alpha").unwrap().snapshot();
    assert_eq!(
        (agent.status, agent.output.as_str()),
        (JobStatus::Idle, "child says hi")
    );
    let prompt = f.llm.requests.lock().unwrap()[0]
        .messages
        .last()
        .unwrap()
        .content
        .clone();
    assert!(prompt.contains("Context:\nshared") && prompt.contains("Task:\nlook"));

    // `wait` hands the result over once, then has nothing left.
    let waited = f.wait();
    assert!(waited.contains("status=\"idle\"") && waited.contains("child says hi"));
    assert!(f.wait().starts_with("Nothing to wait for"));
    assert_eq!(inbox.queued(&f.parent.id), 0);
}

#[test]
fn wait_returns_at_once_when_nothing_is_pending() {
    let f = fixture(&[]);
    assert!(f.wait().starts_with("Nothing to wait for"));
}

#[test]
fn task_concurrency_from_config() {
    // Default concurrency comes from TaskConfig (32), not the old hardcoded 4.
    assert_eq!(TaskConfig::default().max_concurrency, 32);

    // Setting max_concurrency = 2 caps concurrent agents at 2.
    let f = fixture_with_task(
        &[],
        TaskConfig {
            max_concurrency: 2,
            ..TaskConfig::default()
        },
    );
    let tasks: Vec<Value> = (0..4)
        .map(|i| json!({"name": format!("w{i}"), "task": "work", "solutionSpace": "small"}))
        .collect();
    let out = f.spawn(&json!({"context": "ctx", "tasks": tasks})).unwrap();
    assert!(!out.is_error);
    assert_eq!(f.rt.procs().running_ids(&f.parent.id).len(), 2);
    assert!(out.content.contains("`w2` not started"), "{}", out.content);
    assert!(
        out.content.contains("max concurrent subagents (2)"),
        "{}",
        out.content
    );

    // Setting max_concurrency = 6 allows 6 agents concurrently (exceeding the old hardcoded 4).
    let f6 = fixture_with_task(
        &[],
        TaskConfig {
            max_concurrency: 6,
            ..TaskConfig::default()
        },
    );
    let tasks6: Vec<Value> = (0..6)
        .map(|i| json!({"name": format!("j{i}"), "task": "work", "solutionSpace": "small"}))
        .collect();
    let out6 = f6
        .spawn(&json!({"context": "ctx", "tasks": tasks6}))
        .unwrap();
    assert!(!out6.is_error);
    assert_eq!(f6.rt.procs().running_ids(&f6.parent.id).len(), 6);
}

#[test]
fn concurrency_is_limited_and_a_bad_item_spawns_nothing() {
    let f = fixture_with_task(
        &[],
        TaskConfig {
            max_concurrency: 4,
            ..TaskConfig::default()
        },
    );
    let tasks: Vec<Value> = (0..5)
        .map(|i| json!({"name": format!("a{i}"), "task": "t", "solutionSpace": "small"}))
        .collect();
    let out = f.spawn(&json!({"context": "ctx", "tasks": tasks})).unwrap();
    assert!(!out.is_error);
    assert_eq!(f.rt.procs().running_ids(&f.parent.id).len(), 4);
    assert!(out.content.contains("`a4` not started"), "{}", out.content);

    let before = f.rt.agents().in_tree(&f.parent.id).len();
    for args in [
        json!({"context": "ctx", "tasks": [{"name": "x", "task": "t"}, {"name": "X", "task": "t"}]}),
        json!({"context": "ctx", "tasks": [{"agent": "nope", "task": "t"}]}),
        json!({"context": "ctx", "tasks": [{"task": " "}]}),
        json!({"context": "ctx", "tasks": []}),
    ] {
        assert!(f.spawn(&args).is_err(), "{args}");
    }
    assert_eq!(f.rt.agents().in_tree(&f.parent.id).len(), before);
}

#[test]
fn a_settled_session_runs_its_agents_results_as_a_follow_up_turn() {
    let f = fixture(&["child done", "first parent turn", "parent saw it"]);
    let io = || TurnIo {
        cancel: CancelFlag::new(),
        events: Arc::new(VecEvents::default()) as Arc<dyn EventSink>,
        approval: None,
        ask: None,
    };
    f.spawn(&json!({"context": "ctx", "tasks": [{"name": "kid", "task": "work", "solutionSpace": "small"}]}))
        .unwrap();
    // The spawned agent settles before the parent's turn ends.
    f.run_spawned();
    let done =
        block_on(f.rt.run_turn_settled(&f.parent.id, "go", &SessionControl::default(), io()))
            .unwrap();
    assert_eq!(done.final_text, "parent saw it");
    let last = f
        .llm
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .messages
        .last()
        .unwrap()
        .content
        .clone();
    assert!(
        last.contains("<agent id=\"kid\"") && last.contains("child done"),
        "{last}"
    );
    assert_eq!(f.rt.agents().inbox().pending(&f.parent.id), 0);
}

#[test]
fn agent_and_proc_urls_steer_and_cancel_only_this_trees_agents() {
    let f = fixture(&[]);
    f.spawn(&json!({"context": "ctx", "tasks": [{"name": "kid", "task": "work", "solutionSpace": "small"}]}))
        .unwrap();
    let ctx = f.ctx();
    let agents = AgentScheme::new(Arc::clone(f.rt.agents()));
    let procs = ProcScheme::new(Arc::clone(f.rt.procs()));
    let UrlBody::Text(list) = agents.read(&ctx, "").unwrap() else {
        panic!("text expected");
    };
    assert!(list.contains("kid [scout] running"), "{list}");

    agents.write(&ctx, "kid", "also check x").unwrap();
    assert_eq!(
        f.rt.agents().get("kid").unwrap().control.drain_steers(),
        ["also check x"]
    );
    assert!(agents.write(&ctx, "kid", " ").is_err());
    assert!(procs.write(&ctx, "kid", "text").is_err());

    let mut stranger = ctx.clone();
    stranger.root_session_id = "elsewhere".into();
    assert!(agents.read(&stranger, "kid").is_err());
    assert!(procs.write(&stranger, "kid/kill", "").is_err());

    procs.write(&ctx, "kid/kill", "").unwrap();
    let rec = f.rt.agents().get("kid").unwrap();
    assert!(rec.cancel_flag().is_cancelled());
    assert_eq!(rec.snapshot().status, JobStatus::Aborted);
}

fn custom_profile_fixture(script: &[&str]) -> (AgentRuntime, Arc<ManualSpawn>, ToolContext) {
    let fs = MemFs::default()
        .with(
            "/ws/.harness/agents/orchestrator.md",
            "---\nspawns: [scout]\n---\nCoordinate scouts.",
        )
        .with(
            "/ws/.harness/agents/blocker.md",
            "---\nblocking: true\n---\nAlways run synchronously.",
        );
    let llm = Arc::new(ScriptedLlm::default());
    llm.responses
        .lock()
        .unwrap()
        .extend(script.iter().map(|t| Ok(reply(t))));
    let spawned = Arc::new(ManualSpawn::default());
    let rt = AgentRuntime::new(
        llm,
        Arc::new(MemStore::default()),
        ToolRegistry::new(),
        Arc::new(fs),
        HarnessConfig {
            task: TaskConfig {
                enable_effort: true,
                ..TaskConfig::default()
            },
            ..HarnessConfig::default()
        },
    )
    .with_agents(Arc::new(AgentRegistry::new(spawned.clone())));
    let parent = rt
        .create_session(
            "root",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .unwrap();
    let ctx = ToolContext {
        session_id: parent.id.clone(),
        turn_id: "t".into(),
        call_id: "c".into(),
        cwd: "/ws".into(),
        permission_mode: PermissionMode::Yolo,
        cancel: CancelFlag::new(),
        events: Arc::new(VecEvents::default()),
        ask: None,
        approval: None,
        urls: Arc::default(),
        root_session_id: parent.id.clone(),
        inbox: Arc::clone(rt.agents().inbox()),
        caller: None,
        todos: Arc::clone(rt.todos()),
        yield_data: Arc::default(),
        is_subagent: false,
    };
    (rt, spawned, ctx)
}

#[test]
fn blocking_profile_and_effort_override_work() {
    let (rt, spawned, ctx) = custom_profile_fixture(&["blocked sync reply", "effort reply"]);
    let out = block_on(SubagentTool::new(rt.clone()).execute(
        &ctx,
        json!({"context": "c", "tasks": [{"name": "b1", "agent": "blocker", "task": "run", "solutionSpace": "s"}]}),
    ))
    .unwrap();
    assert!(!out.is_error);
    assert!(
        out.content.contains("blocked sync reply"),
        "{}",
        out.content
    );
    assert!(spawned.0.lock().unwrap().is_empty());

    let eff_out = block_on(SubagentTool::new(rt.clone()).execute(
        &ctx,
        json!({"context": "c", "tasks": [{"name": "e1", "agent": "task", "task": "think", "solutionSpace": "s", "effort": "hi"}]}),
    ))
    .unwrap();
    assert!(!eff_out.is_error);
    let sid = rt
        .agents()
        .get("e1")
        .unwrap()
        .snapshot()
        .session_id
        .unwrap();
    assert_eq!(
        rt.store.get_session(&sid).unwrap().unwrap().thinking,
        ThinkingLevel::High
    );
}

#[test]
fn spawns_allowlist_and_self_recursion_guard() {
    let (rt, _, ctx) = custom_profile_fixture(&[]);
    let profiles = load_subagent_profiles(rt.fs.as_ref(), Path::new("/ws")).unwrap();
    let orch_prof = profiles.get("orchestrator").unwrap().clone();
    let nested = SubagentTool::nested(rt.clone(), 1, orch_prof);
    let denied = block_on(nested.execute(
        &ctx,
        json!({"context": "c", "tasks": [{"agent": "reviewer", "task": "audit", "solutionSpace": "s"}]}),
    ))
    .expect_err("unlisted child must be refused")
    .to_string();
    assert!(denied.contains("cannot spawn `reviewer`"), "{denied}");

    let self_rec = block_on(nested.execute(
        &ctx,
        json!({"context": "c", "tasks": [{"agent": "orchestrator", "task": "loop", "solutionSpace": "s"}]}),
    ))
    .expect_err("self-recursion must be refused")
    .to_string();
    assert!(self_rec.contains("cannot spawn"), "{self_rec}");

    let task_prof = profiles.get("task").unwrap().clone();
    let nested_task = SubagentTool::nested(rt, 1, task_prof);
    let self_blocked = block_on(nested_task.execute(
        &ctx,
        json!({"context": "c", "tasks": [{"agent": "task", "task": "loop", "solutionSpace": "s"}]}),
    ))
    .expect_err("self-recursion blocked")
    .to_string();
    assert!(self_blocked.contains("blocked_agent"), "{self_blocked}");
}
