//! Wave 10a `TaskParity` acceptance tests (part 1: shapes, `solutionSpace`, `steps[]`,
//! `outputSchema`, worktree isolation, sync mode, per-call `model`, unknown agent).

#[path = "task_parity_extra_tests.rs"]
mod extra;

use crate::agents::AgentScheme;
use crate::agents::{AgentRegistry, JobStatus, Spawn};
use crate::artifacts::ArtifactStore;
use crate::isolation::{IsolatedWorktree, IsolationFinalize, IsolationOutcome, WorktreeIsolator};
use crate::registry::ToolRegistry;
use crate::runtime::{AgentRuntime, HarnessConfig};
use crate::subagent::SubagentTool;
use crate::subagent::bucket_steps;
use crate::task_config::{TaskConfig, TaskIsolationConfig};
use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents, block_on};
use crate::think_yield::YieldTool;
use hrn_kernel::{
    BoxFuture, CancelFlag, Clock, FinishReason, LlmResponse, ModelInfo, PermissionMode, Result,
    SessionMeta, SessionStore, ThinkingLevel, Tool, ToolCall, ToolContext, ToolEffect, ToolOutput,
    ToolSpec, UrlBody, UrlScheme,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(super) struct ManualSpawn(pub(super) Mutex<Vec<BoxFuture<()>>>);
impl Spawn for ManualSpawn {
    fn spawn(&self, task: BoxFuture<()>) -> Result {
        self.0.lock().unwrap().push(task);
        Ok(())
    }
}

pub(super) struct FakeClock(AtomicI64);
impl FakeClock {
    pub(super) fn new(start: i64) -> Self {
        Self(AtomicI64::new(start))
    }
    pub(super) fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}
impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
pub(super) struct TempDir(pub(super) PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hrn-10a-{tag}-{}-{n}", std::process::id()));
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

pub(super) fn reply(text: &str) -> LlmResponse {
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

pub(super) fn tool_call(id: &str, name: &str, args: &Value) -> LlmResponse {
    LlmResponse {
        model_used: "m".into(),
        fallback_from: None,
        content: String::new(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
            signature: None,
        }],
        usage: None,
        finish_reason: FinishReason::ToolCalls,
    }
}

pub(super) struct DummyTool(pub(super) &'static str, pub(super) ToolEffect);
impl Tool for DummyTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.to_owned(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        }
    }
    fn effect(&self) -> ToolEffect {
        self.1
    }
    fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
        Box::pin(async { Ok(ToolOutput::ok("ok")) })
    }
}

pub(super) struct StubIsolator {
    wt_root: PathBuf,
    patch_body: String,
    pub(super) fail_apply: AtomicBool,
    pub(super) applied_patches: Mutex<Vec<(PathBuf, String)>>,
}

impl StubIsolator {
    pub(super) fn new(wt_root: PathBuf, patch_body: &str) -> Self {
        Self {
            wt_root,
            patch_body: patch_body.to_owned(),
            fail_apply: AtomicBool::new(false),
            applied_patches: Mutex::new(Vec::new()),
        }
    }
}

impl WorktreeIsolator for StubIsolator {
    fn prepare(&self, repo_cwd: &Path, agent_id: &str) -> BoxFuture<Result<IsolatedWorktree>> {
        let wt_path = self
            .wt_root
            .join(".harness")
            .join("wt")
            .join(format!("task-{agent_id}"));
        let _ = std::fs::create_dir_all(&wt_path);
        let wt = IsolatedWorktree {
            path: wt_path,
            branch: format!("hrn/task/{agent_id}"),
            repo_cwd: repo_cwd.to_path_buf(),
        };
        Box::pin(async move { Ok(wt) })
    }

    fn finalize(
        &self,
        worktree: &IsolatedWorktree,
        req: IsolationFinalize,
    ) -> BoxFuture<Result<IsolationOutcome>> {
        let patch = self.patch_body.clone();
        let fail_apply = self.fail_apply.load(Ordering::SeqCst);
        let mut outcome = IsolationOutcome::default();
        if let Some(store) = req.artifacts {
            let p = store
                .write_patch(&req.root_session_id, &req.agent_id, &patch)
                .unwrap();
            outcome.patch_path = Some(p);
        }
        if req.config.merge == "branch" && req.child_succeeded {
            outcome.branch = Some(worktree.branch.clone());
        }
        if req.config.apply && req.child_succeeded {
            if fail_apply {
                outcome
                    .notes
                    .push("patch apply failed (conflict); patch kept".to_owned());
            } else {
                self.applied_patches
                    .lock()
                    .unwrap()
                    .push((worktree.repo_cwd.clone(), patch));
                outcome.applied = true;
            }
        }
        Box::pin(async move { Ok(outcome) })
    }

    fn cleanup(&self, _worktree: &IsolatedWorktree) -> BoxFuture<Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

pub(super) struct Env {
    pub(super) rt: AgentRuntime,
    pub(super) llm: Arc<ScriptedLlm>,
    pub(super) store: Arc<MemStore>,
    pub(super) spawned: Arc<ManualSpawn>,
    pub(super) clock: Arc<FakeClock>,
    pub(super) artifacts: Arc<ArtifactStore>,
    pub(super) parent: SessionMeta,
    pub(super) tmp: TempDir,
}

impl Env {
    pub(super) fn build(
        tag: &str,
        responses: Vec<LlmResponse>,
        task: TaskConfig,
        fs: MemFs,
        models: Vec<ModelInfo>,
    ) -> Self {
        let tmp = TempDir::new(tag);
        let clock = Arc::new(FakeClock::new(10_000));
        let artifacts = Arc::new(ArtifactStore::new(tmp.0.clone()));
        let llm = ScriptedLlm {
            models,
            ..ScriptedLlm::default()
        };
        llm.responses
            .lock()
            .unwrap()
            .extend(responses.into_iter().map(Ok));
        let llm = Arc::new(llm);
        let store = Arc::new(MemStore::default());
        let spawned = Arc::new(ManualSpawn::default());
        let reg = Arc::new(
            AgentRegistry::new(spawned.clone())
                .with_clock(clock.clone())
                .with_artifacts(artifacts.clone()),
        );
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(YieldTool));
        tools.register(Arc::new(DummyTool("read", ToolEffect::ReadOnly)));
        tools.register(Arc::new(DummyTool("edit", ToolEffect::Mutating)));
        let rt = AgentRuntime::new(
            llm.clone(),
            store.clone(),
            tools,
            Arc::new(fs),
            HarnessConfig {
                task,
                ..HarnessConfig::default()
            },
        )
        .with_clock(clock.clone())
        .with_agents(reg);
        let parent = rt
            .create_session(
                "root",
                &tmp.0,
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
            tmp,
        }
    }

    pub(super) fn ctx(&self, mode: PermissionMode) -> ToolContext {
        ToolContext {
            session_id: self.parent.id.clone(),
            turn_id: "t".into(),
            call_id: "c".into(),
            cwd: PathBuf::from(&self.parent.cwd),
            permission_mode: mode,
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

    pub(super) fn call(&self, args: &Value) -> Result<ToolOutput> {
        block_on(
            SubagentTool::new(self.rt.clone())
                .execute(&self.ctx(PermissionMode::Yolo), args.clone()),
        )
    }

    pub(super) fn run_all(&self) {
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
}

#[test]
fn task_flat_shape_accepted() {
    let env = Env::build(
        "flat",
        vec![reply("flat answer")],
        TaskConfig::default(),
        MemFs::default(),
        vec![],
    );
    let out = env
        .call(&json!({"name": "solo", "agent": "explore", "task": "check files", "solutionSpace": "single file"}))
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("`solo` [scout]"), "{}", out.content);
    env.run_all();
    assert_eq!(
        env.rt.agents().get("solo").unwrap().snapshot().output,
        "flat answer"
    );
    assert!(
        env.call(&json!({
            "context": "ctx",
            "task": "top",
            "tasks": [{"task": "inner", "solutionSpace": "small"}]
        }))
        .is_err()
    );
}

#[test]
fn task_missing_context_errors() {
    let env = Env::build(
        "no-ctx",
        vec![],
        TaskConfig::default(),
        MemFs::default(),
        vec![],
    );
    let err = env
        .call(&json!({"tasks": [{"task": "do work", "solutionSpace": "small"}]}))
        .expect_err("missing context")
        .to_string();
    assert!(err.contains("context"), "{err}");
    assert!(
        env.call(
            &json!({"context": "  ", "tasks": [{"task": "do work", "solutionSpace": "small"}]})
        )
        .is_err()
    );
}

#[test]
fn task_solution_space_blank_warns() {
    let env = Env::build(
        "blank-ss",
        vec![reply("done")],
        TaskConfig::default(),
        MemFs::default(),
        vec![],
    );
    let out = env
        .call(&json!({
            "context": "repo ctx",
            "tasks": [{"name": "a", "task": "inspect parser", "solutionSpace": "   "}]
        }))
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("solutionSpace"), "{}", out.content);
    env.run_all();
    let reqs = env.llm.requests.lock().unwrap();
    let last_msg = &reqs[0].messages.last().unwrap().content;
    assert!(
        last_msg.contains("[solution-space: inspect parser]"),
        "{last_msg}"
    );
}

#[test]
fn task_steps_chain() {
    let five: Vec<String> = (1..=5).map(|n| format!("item {n}")).collect();
    let buckets = bucket_steps(&five);
    assert_eq!(buckets.len(), 4);
    assert!(buckets[0].contains("item 1") && buckets[0].contains("item 2"));
    assert_eq!(
        (
            buckets[1].as_str(),
            buckets[2].as_str(),
            buckets[3].as_str()
        ),
        ("item 3", "item 4", "item 5")
    );

    let env = Env::build(
        "steps",
        vec![reply("step 1 findings"), reply("step 2 final answer")],
        TaskConfig {
            steps: true,
            ..TaskConfig::default()
        },
        MemFs::default(),
        vec![],
    );
    let out = env
        .call(&json!({
            "context": "shared",
            "tasks": [{"name": "chain", "steps": ["analyze module", "write summary"], "solutionSpace": "two steps"}]
        }))
        .unwrap();
    assert!(!out.is_error);
    env.run_all();
    let snap = env.rt.agents().get("chain").unwrap().snapshot();
    assert_eq!(
        (snap.status, snap.output.as_str()),
        (JobStatus::Idle, "step 2 final answer")
    );
    assert_eq!(snap.progress_notes.len(), 1);
    assert!(snap.progress_notes[0].contains("step 1 findings"));
    let reqs = env.llm.requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].session_id, reqs[1].session_id);
}

#[test]
fn task_output_schema_strict_rejects() {
    let schema = json!({
        "type": "object",
        "required": ["ok", "score"],
        "properties": {"ok": {"type": "boolean"}, "score": {"type": "integer", "minimum": 0}},
        "additionalProperties": false
    });
    let bad = json!({"result": {"ok": "nope"}});
    let env = Env::build(
        "schema-strict",
        vec![
            tool_call("c0", "yield", &bad),
            reply("d0"),
            tool_call("c1", "yield", &bad),
            reply("d1"),
            tool_call("c2", "yield", &bad),
            reply("d2"),
            tool_call("c3", "yield", &bad),
            reply("d3"),
        ],
        TaskConfig {
            structured: true,
            ..TaskConfig::default()
        },
        MemFs::default(),
        vec![],
    );
    env.call(&json!({
        "context": "audit",
        "tasks": [{"name": "strict-kid", "task": "score", "solutionSpace": "s", "outputSchema": schema, "schemaMode": "strict"}]
    }))
    .unwrap();
    env.run_all();
    let snap = env.rt.agents().get("strict-kid").unwrap().snapshot();
    assert_eq!(snap.status, JobStatus::Failed);
    assert!(
        snap.output.contains("schema validation failed") && snap.output.contains("$.ok"),
        "{}",
        snap.output
    );
}

#[test]
fn task_isolation_captures_patch() {
    let mut env = Env::build(
        "iso-patch",
        vec![reply("edited file")],
        TaskConfig {
            isolation: TaskIsolationConfig {
                enabled: true,
                merge: "patch".into(),
                apply: false,
            },
            ..TaskConfig::default()
        },
        MemFs::default(),
        vec![],
    );
    let iso = Arc::new(StubIsolator::new(
        env.tmp.0.clone(),
        "diff --git a/x.txt b/x.txt\n+hi\n",
    ));
    env.rt = env.rt.with_isolator(iso.clone());
    env.call(&json!({
        "context": "edit",
        "tasks": [{"name": "iso1", "agent": "task", "task": "edit x", "solutionSpace": "s", "isolated": true}]
    }))
    .unwrap();
    env.run_all();
    let snap = env.rt.agents().get("iso1").unwrap().snapshot();
    let child_sess = env
        .store
        .get_session(snap.session_id.as_deref().unwrap())
        .unwrap()
        .unwrap();
    assert!(child_sess.cwd.contains("task-iso1"), "{}", child_sess.cwd);
    assert!(snap.patch_path.as_ref().is_some_and(|p| p.is_file()));
    assert_eq!(
        env.artifacts
            .read_patch(&env.parent.id, "iso1")
            .unwrap()
            .as_deref(),
        Some("diff --git a/x.txt b/x.txt\n+hi\n")
    );
    let UrlBody::Text(detail) = AgentScheme::new(Arc::clone(env.rt.agents()))
        .read(&env.ctx(PermissionMode::Yolo), "iso1")
        .unwrap()
    else {
        panic!("text expected");
    };
    assert!(detail.contains("patch:"), "{detail}");
    assert!(iso.applied_patches.lock().unwrap().is_empty());
}

#[test]
fn task_isolation_apply() {
    let mut env = Env::build(
        "iso-apply",
        vec![reply("applied"), reply("conflict")],
        TaskConfig {
            isolation: TaskIsolationConfig {
                enabled: true,
                merge: "branch".into(),
                apply: true,
            },
            ..TaskConfig::default()
        },
        MemFs::default(),
        vec![],
    );
    let iso = Arc::new(StubIsolator::new(
        env.tmp.0.clone(),
        "diff --git a/a.rs b/a.rs\n+ok\n",
    ));
    env.rt = env.rt.with_isolator(iso.clone());
    env.call(&json!({
        "context": "apply",
        "tasks": [{"name": "applier", "agent": "task", "task": "edit a", "solutionSpace": "s", "isolated": true}]
    }))
    .unwrap();
    env.run_all();
    assert_eq!(iso.applied_patches.lock().unwrap().len(), 1);
    let snap = env.rt.agents().get("applier").unwrap().snapshot();
    assert_eq!(snap.isolated_branch.as_deref(), Some("hrn/task/applier"));

    iso.fail_apply.store(true, Ordering::SeqCst);
    env.call(&json!({
        "context": "conflict",
        "tasks": [{"name": "conflict", "agent": "task", "task": "edit a", "solutionSpace": "s", "isolated": true}]
    }))
    .unwrap();
    env.run_all();
    let csnap = env.rt.agents().get("conflict").unwrap().snapshot();
    assert!(csnap.patch_path.as_ref().is_some_and(|p| p.is_file()));
    assert!(
        csnap.output.contains("patch apply failed"),
        "{}",
        csnap.output
    );
}
