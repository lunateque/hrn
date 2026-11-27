//! Unit tests for parallel read batching and `PermissionMode` x `ToolApproval` gating.

use super::*;
use crate::testutil::{MemFs, VecEvents, block_on};
use hrn_kernel::{BoxFuture, Tool, ToolSpec};
use serde_json::json;

struct FakeTool {
    name: &'static str,
    effect: ToolEffect,
    approval: ToolApproval,
}

impl Tool for FakeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.to_owned(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        }
    }

    fn effect(&self) -> ToolEffect {
        self.effect
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        self.approval.clone()
    }

    fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
        let name = self.name;
        Box::pin(async move { Ok(ToolOutput::ok(format!("ran {name}"))) })
    }
}

struct Gate(ApprovalDecision);

impl ApprovalGate for Gate {
    fn await_decision(
        &self,
        _r: ApprovalRequest,
        _c: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>> {
        let answer = self.0.clone();
        Box::pin(async move { Ok(answer) })
    }
}

/// Approves the first ask and switches the live mode to `yolo`, as a user would from the
/// front-end; any later ask is refused.
struct LoosenOnFirst(SessionControl);

impl ApprovalGate for LoosenOnFirst {
    fn await_decision(
        &self,
        _r: ApprovalRequest,
        _c: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>> {
        let first = self.0.permission_mode().is_none();
        self.0.set_permission_mode(PermissionMode::Yolo);
        Box::pin(async move { Ok(ApprovalDecision::from_bool(first)) })
    }
}

fn registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    for (name, effect, approval) in [
        ("read_a", ToolEffect::ReadOnly, ToolApproval::read()),
        ("read_b", ToolEffect::ReadOnly, ToolApproval::read()),
        ("edit", ToolEffect::Mutating, ToolApproval::write()),
        ("bash", ToolEffect::Mutating, ToolApproval::exec()),
        (
            "forced",
            ToolEffect::Mutating,
            ToolApproval {
                tier: ToolTier::Write,
                force_prompt: true,
                reason: Some("protected path".into()),
                policy: None,
            },
        ),
    ] {
        reg.register(Arc::new(FakeTool {
            name,
            effect,
            approval,
        }));
    }
    reg
}

fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
        signature: None,
    }
}

fn run(
    mode: PermissionMode,
    gate: Option<&Arc<dyn ApprovalGate>>,
    cancel: &CancelFlag,
    calls: &[ToolCall],
) -> BatchOutcome {
    run_with(&SessionControl::default(), mode, gate, cancel, calls)
}

fn run_with(
    control: &SessionControl,
    mode: PermissionMode,
    gate: Option<&Arc<dyn ApprovalGate>>,
    cancel: &CancelFlag,
    calls: &[ToolCall],
) -> BatchOutcome {
    let reg = registry();
    let events: Arc<dyn EventSink> = Arc::new(VecEvents::default());
    let urls = Arc::new(UrlRouter::new());
    let sid = "s".to_owned();
    let tid = "t".to_owned();
    let policy = UserApprovalPolicy::default();
    let grants = Arc::new(GrantCache::memory());
    let fs = MemFs::default();
    let env = BatchEnv {
        registry: &reg,
        hooks: &[],
        session_id: &sid,
        turn_id: &tid,
        cwd: Path::new("."),
        permission_mode: mode,
        control,
        cancel,
        events: &events,
        approval: gate,
        ask: None,
        urls: &urls,
        root_session_id: &sid,
        inbox: &Arc::default(),
        caller: None,
        todos: Arc::new(hrn_kernel::todo::TodoBoards::new()),
        yield_data: &Arc::default(),
        is_subagent: false,
        approval_policy: &policy,
        grants: &grants,
        fs: &fs,
        trace: None,
        store: None,
    };
    block_on(execute_tool_batch(&env, calls))
}

#[test]
fn approval_mapping_preserves_modes() {
    let read = ToolApproval::read();
    let write = ToolApproval::write();
    let exec = ToolApproval::exec();
    let forced = ToolApproval {
        tier: ToolTier::Read,
        policy: Some(ToolApprovalPolicy::Allow),
        reason: Some("safety override".into()),
        force_prompt: true,
    };

    // Default (`always-ask`): Read auto, Write & Exec prompt.
    assert!(!needs_confirmation(PermissionMode::Default, &read));
    assert!(needs_confirmation(PermissionMode::Default, &write));
    assert!(needs_confirmation(PermissionMode::Default, &exec));

    // AskCached (`always-ask-cached`): every tier prompts.
    assert!(needs_confirmation(PermissionMode::AskCached, &read));
    assert!(needs_confirmation(PermissionMode::AskCached, &write));
    assert!(needs_confirmation(PermissionMode::AskCached, &exec));

    // AutoEdit (`write`): Read & Write auto, Exec prompts.
    assert!(!needs_confirmation(PermissionMode::AutoEdit, &read));
    assert!(!needs_confirmation(PermissionMode::AutoEdit, &write));
    assert!(needs_confirmation(PermissionMode::AutoEdit, &exec));

    // Yolo: Read, Write, Exec all auto; force_prompt still prompts.
    assert!(!needs_confirmation(PermissionMode::Yolo, &read));
    assert!(!needs_confirmation(PermissionMode::Yolo, &write));
    assert!(!needs_confirmation(PermissionMode::Yolo, &exec));
    assert!(needs_confirmation(PermissionMode::Yolo, &forced));

    // AutoEdit runs `edit` headless without a gate, while `forced` still requires one.
    let edit_ok = run(
        PermissionMode::AutoEdit,
        None,
        &CancelFlag::new(),
        &[call("1", "edit", "{}")],
    );
    assert_eq!(edit_ok.outputs[0].content, "ran edit");
    let forced_blocked = run(
        PermissionMode::Yolo,
        None,
        &CancelFlag::new(),
        &[call("1", "forced", "{}")],
    );
    assert!(forced_blocked.outputs[0].is_error);
}

#[test]
fn the_live_permission_mode_applies_from_the_next_call() {
    // Tightened mid-turn: the step started in `yolo`, the user switched back to `default`.
    let control = SessionControl::default();
    control.set_permission_mode(PermissionMode::Default);
    let deny: Arc<dyn ApprovalGate> = Arc::new(Gate(ApprovalDecision::Deny));
    let one = [call("1", "bash", "{}")];
    let tightened = run_with(
        &control,
        PermissionMode::Yolo,
        Some(&deny),
        &CancelFlag::new(),
        &one,
    );
    let out = &tightened.outputs[0];
    assert!(out.is_error && out.content.contains("denied"));

    // Loosened while the batch runs: the second call no longer asks.
    let control = SessionControl::default();
    let gate: Arc<dyn ApprovalGate> = Arc::new(LoosenOnFirst(control.clone()));
    let two = [call("1", "bash", "{}"), call("2", "bash", "{}")];
    let loosened = run_with(
        &control,
        PermissionMode::Default,
        Some(&gate),
        &CancelFlag::new(),
        &two,
    );
    assert!(loosened.outputs.iter().all(|o| o.content == "ran bash"));
}

#[test]
fn confirmation_fails_closed_without_a_gate_and_honours_denial() {
    let calls = [call("1", "bash", "{}")];
    let none = run(PermissionMode::Default, None, &CancelFlag::new(), &calls);
    assert!(
        none.outputs
            .first()
            .is_some_and(|o| o.is_error && o.content.contains("no approval channel"))
    );

    let deny: Arc<dyn ApprovalGate> = Arc::new(Gate(ApprovalDecision::Deny));
    let denied = run(
        PermissionMode::Default,
        Some(&deny),
        &CancelFlag::new(),
        &calls,
    );
    assert!(
        denied
            .outputs
            .first()
            .is_some_and(|o| o.is_error && o.content.contains("denied"))
    );

    let control = SessionControl::default();
    let deny_steer: Arc<dyn ApprovalGate> = Arc::new(Gate(ApprovalDecision::DenyAndSend {
        message: "use cargo check instead".into(),
    }));
    let steered = run_with(
        &control,
        PermissionMode::Default,
        Some(&deny_steer),
        &CancelFlag::new(),
        &calls,
    );
    assert!(steered.outputs[0].is_error);
    assert_eq!(control.drain_steers(), ["use cargo check instead"]);

    let allow: Arc<dyn ApprovalGate> = Arc::new(Gate(ApprovalDecision::once()));
    let ok = run(
        PermissionMode::Default,
        Some(&allow),
        &CancelFlag::new(),
        &calls,
    );
    assert_eq!(
        ok.outputs.first().map(|o| o.content.as_str()),
        Some("ran bash")
    );

    let yolo = run(PermissionMode::Yolo, None, &CancelFlag::new(), &calls);
    assert_eq!(
        yolo.outputs.first().map(|o| o.content.as_str()),
        Some("ran bash")
    );

    let plan = run(PermissionMode::Plan, None, &CancelFlag::new(), &calls);
    assert!(
        plan.outputs
            .first()
            .is_some_and(|o| o.content.contains("plan"))
    );
}

#[test]
fn every_call_gets_an_output_in_order_even_when_cancelled() {
    let calls = [
        call("1", "read_a", "{}"),
        call("2", "read_b", "{}"),
        call("3", "bash", "{}"),
        call("4", "nope", "{}"),
        call("5", "read_a", "not json"),
    ];
    let ok = run(PermissionMode::Yolo, None, &CancelFlag::new(), &calls);
    let texts: Vec<&str> = ok.outputs.iter().map(|o| o.content.as_str()).collect();
    assert_eq!(texts.len(), 5);
    assert_eq!(texts.first(), Some(&"ran read_a"));
    assert_eq!(texts.get(1), Some(&"ran read_b"));
    assert!(
        ok.outputs
            .get(3)
            .is_some_and(|o| o.content.contains("unknown tool"))
    );
    assert!(
        ok.outputs
            .get(4)
            .is_some_and(|o| o.content.contains("invalid JSON"))
    );

    let cancel = CancelFlag::new();
    cancel.cancel();
    let cancelled = run(PermissionMode::Yolo, None, &cancel, &calls);
    assert!(cancelled.cancelled);
    assert_eq!(cancelled.outputs.len(), calls.len());
    assert!(cancelled.outputs.iter().all(|o| o.is_error));
}

struct CountingGate {
    decision: ApprovalDecision,
    asks: std::sync::atomic::AtomicUsize,
    last_choices: std::sync::Mutex<Vec<ApprovalGrantScope>>,
}

impl ApprovalGate for CountingGate {
    fn await_decision(
        &self,
        req: ApprovalRequest,
        _c: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>> {
        self.asks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.last_choices.lock().unwrap() = req.choices;
        let d = self.decision.clone();
        Box::pin(async move { Ok(d) })
    }
}

#[test]
fn grant_cache_and_restricted_folders_gate_batch_execution() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let reg = registry();
    let events: Arc<dyn EventSink> = Arc::new(VecEvents::default());
    let urls = Arc::new(UrlRouter::new());
    let (sid, tid) = ("s".to_owned(), "t".to_owned());
    let (control, cancel) = (SessionControl::default(), CancelFlag::new());
    let (grants, fs) = (Arc::new(GrantCache::memory()), MemFs::default());
    let policy = UserApprovalPolicy {
        restricted_folders: vec!["/repo/secret".to_owned()],
        ..UserApprovalPolicy::default()
    };
    let gate_inner = Arc::new(CountingGate {
        decision: ApprovalDecision::Approve {
            scope: ApprovalGrantScope::Directory,
        },
        asks: AtomicUsize::new(0),
        last_choices: std::sync::Mutex::new(Vec::new()),
    });
    let gate: Arc<dyn ApprovalGate> = gate_inner.clone();

    let env = BatchEnv {
        registry: &reg,
        hooks: &[],
        session_id: &sid,
        turn_id: &tid,
        cwd: Path::new("/repo"),
        permission_mode: PermissionMode::AskCached,
        control: &control,
        cancel: &cancel,
        events: &events,
        approval: Some(&gate),
        ask: None,
        urls: &urls,
        root_session_id: &sid,
        inbox: &Arc::default(),
        caller: None,
        todos: Arc::new(hrn_kernel::todo::TodoBoards::new()),
        yield_data: &Arc::default(),
        is_subagent: false,
        approval_policy: &policy,
        grants: &grants,
        fs: &fs,
        trace: None,
        store: None,
    };

    // First write in `/repo/src/a.rs` prompts and caches a Directory grant on `/repo/src`.
    let out1 = block_on(execute_tool_batch(
        &env,
        &[call("1", "edit", r#"{"path":"src/a.rs"}"#)],
    ));
    assert_eq!(out1.outputs[0].content, "ran edit");
    assert_eq!(gate_inner.asks.load(Ordering::SeqCst), 1);

    // Direct sibling `/repo/src/b.rs` (both edit and read) is covered without prompting again.
    let out2 = block_on(execute_tool_batch(
        &env,
        &[
            call("2", "edit", r#"{"path":"src/b.rs"}"#),
            call("3", "read_a", r#"{"path":"src/b.rs"}"#),
        ],
    ));
    assert_eq!(out2.outputs[0].content, "ran edit");
    assert_eq!(out2.outputs[1].content, "ran read_a");
    assert_eq!(gate_inner.asks.load(Ordering::SeqCst), 1);

    // Subdirectory `/repo/src/nested/c.rs` is NOT covered by `/repo/src` directory grant.
    let out3 = block_on(execute_tool_batch(
        &env,
        &[call("4", "edit", r#"{"path":"src/nested/c.rs"}"#)],
    ));
    assert_eq!(out3.outputs[0].content, "ran edit");
    assert_eq!(gate_inner.asks.load(Ordering::SeqCst), 2);

    assert_restricted_and_prompt_policies(env, &control, &gate_inner);
}

fn assert_restricted_and_prompt_policies(
    env: BatchEnv<'_>,
    control: &SessionControl,
    gate_inner: &CountingGate,
) {
    use std::sync::atomic::Ordering;

    // Restricted folder `/repo/secret/key` forces plain `[Once]` prompt even in `yolo` and is
    // NEVER cached on repeat calls.
    control.set_permission_mode(PermissionMode::Yolo);
    let out4 = block_on(execute_tool_batch(
        &env,
        &[
            call("5", "read_a", r#"{"path":"secret/key"}"#),
            call("6", "read_a", r#"{"path":"secret/key"}"#),
        ],
    ));
    assert_eq!(out4.outputs[0].content, "ran read_a");
    assert_eq!(out4.outputs[1].content, "ran read_a");
    assert_eq!(gate_inner.asks.load(Ordering::SeqCst), 4);
    assert_eq!(
        *gate_inner.last_choices.lock().unwrap(),
        vec![ApprovalGrantScope::Once]
    );

    // Explicit `[tools.approval] <tool> = "prompt"` is NEVER cacheable even if the gate returns
    // `ToolSession` or `Directory` (forged scope ignored).
    let mut prompt_policy = UserApprovalPolicy::default();
    prompt_policy
        .tool_policies
        .insert("edit".to_owned(), ToolApprovalPolicy::Prompt);
    let prompt_env = BatchEnv {
        approval_policy: &prompt_policy,
        permission_mode: PermissionMode::Yolo,
        ..env
    };
    let before = gate_inner.asks.load(Ordering::SeqCst);
    let _ = block_on(execute_tool_batch(
        &prompt_env,
        &[
            call("7", "edit", r#"{"path":"src/x.rs"}"#),
            call("8", "edit", r#"{"path":"src/x.rs"}"#),
        ],
    ));
    assert_eq!(gate_inner.asks.load(Ordering::SeqCst), before + 2);
}

struct OutWriteTool;

impl Tool for OutWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".to_owned(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        }
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }
    fn approval(&self, args: &Value) -> ToolApproval {
        if args
            .get("path")
            .and_then(Value::as_str)
            .and_then(UrlRouter::split)
            .is_some_and(|(s, _)| s.eq_ignore_ascii_case("out"))
        {
            return ToolApproval::read();
        }
        ToolApproval::write()
    }
    fn execute(&self, _ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let p = args["path"].as_str().unwrap_or("").to_owned();
        Box::pin(async move { Ok(ToolOutput::ok(format!("wrote {p}"))) })
    }
}

struct RedirectHook(&'static str);

impl HarnessHook for RedirectHook {
    fn before_tool(
        &self,
        _ctx: &ToolContext,
        _tool_name: &str,
        args: &Value,
    ) -> BoxFuture<Result<HookVerdict>> {
        let mut next = args.clone();
        next["path"] = json!(self.0);
        Box::pin(async move {
            Ok(HookVerdict::Revise {
                arguments: Some(next),
                context: None,
            })
        })
    }
}

#[test]
fn before_tool_revise_reruns_gate_call_for_plan_approval_and_restricted_folders() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(OutWriteTool));
    let sid = "s-rev".to_owned();
    let tid = "t-rev".to_owned();
    let control = SessionControl::default();
    let cancel = CancelFlag::new();
    let events: Arc<dyn EventSink> = Arc::new(VecEvents::default());
    let urls = Arc::new(UrlRouter::new());
    let grants = Arc::new(GrantCache::memory());
    let fs = MemFs::default();
    let policy = UserApprovalPolicy {
        restricted_folders: vec!["/repo/secret".to_owned()],
        tool_policies: std::collections::BTreeMap::new(),
    };
    let hook: Arc<dyn HarnessHook> = Arc::new(RedirectHook("secret/stolen.txt"));
    let hooks = [hook];
    let deny_gate: Arc<dyn ApprovalGate> = Arc::new(Gate(ApprovalDecision::Deny));

    // Plan mode: revising `write out://note` -> disk path must be blocked by plan mode.
    let plan_env = BatchEnv {
        registry: &reg,
        hooks: &hooks,
        session_id: &sid,
        turn_id: &tid,
        cwd: Path::new("/repo"),
        permission_mode: PermissionMode::Plan,
        control: &control,
        cancel: &cancel,
        events: &events,
        approval: Some(&deny_gate),
        ask: None,
        urls: &urls,
        root_session_id: &sid,
        inbox: &Arc::default(),
        caller: None,
        todos: Arc::new(hrn_kernel::todo::TodoBoards::new()),
        yield_data: &Arc::default(),
        is_subagent: false,
        approval_policy: &policy,
        grants: &grants,
        fs: &fs,
        trace: None,
        store: None,
    };
    let args = r#"{"path":"out://note","content":"hi"}"#;
    let plan_out = block_on(execute_tool_batch(&plan_env, &[call("1", "write", args)]));
    let p0 = &plan_out.outputs[0];
    assert!(
        p0.is_error && p0.content.contains("blocked by `plan` mode"),
        "{}",
        p0.content
    );

    // Yolo mode: revising `write out://note` -> restricted folder must re-prompt (denied here).
    let yolo_env = BatchEnv {
        permission_mode: PermissionMode::Yolo,
        ..plan_env
    };
    let yolo_out = block_on(execute_tool_batch(&yolo_env, &[call("2", "write", args)]));
    let y0 = &yolo_out.outputs[0];
    assert!(
        y0.is_error && y0.content.contains("user denied"),
        "{}",
        y0.content
    );
}
