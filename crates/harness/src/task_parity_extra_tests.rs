//! Additional Wave 10a parity tests: dynamic schema toggles, permissive schema warnings,
//! Plan mode restrictions, dynamic tools hook, level-tagged profile conflict, and budgets.

use super::{DummyTool, Env, StubIsolator, reply, tool_call};
use crate::agents::JobStatus;
use crate::isolation::DynamicToolResolver;
use crate::runtime::AgentRuntime;
use crate::subagent::SubagentTool;
use crate::task_config::{TaskConfig, TaskIsolationConfig};
use crate::testutil::{MemFs, block_on};
use hrn_kernel::{
    HrnError, ModelInfo, PermissionMode, Result, SessionStore, ThinkingLevel, Tool, ToolEffect,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

struct StubDynamicTools;
impl DynamicToolResolver for StubDynamicTools {
    fn resolve(&self, _session_id: &str, names: &[String]) -> Result<Vec<Arc<dyn Tool>>> {
        if names.iter().any(|n| n == "custom_eval_fn") {
            Ok(vec![Arc::new(DummyTool(
                "custom_eval_fn",
                ToolEffect::ReadOnly,
            ))])
        } else {
            Err(HrnError::Tool("unknown dynamic tool".into()))
        }
    }
}

#[test]
fn task_spec_omits_disabled_fields_and_exposes_enabled_ones() {
    let def_env = Env::build(
        "spec-def",
        vec![],
        TaskConfig::default(),
        MemFs::default(),
        vec![],
    );
    let props = &SubagentTool::new(def_env.rt.clone()).spec().parameters["properties"]["tasks"]["items"]
        ["properties"];
    for omitted in [
        "effort",
        "steps",
        "outputSchema",
        "schemaMode",
        "isolated",
        "model",
    ] {
        assert!(
            props.get(omitted).is_none(),
            "`{omitted}` must be omitted when disabled"
        );
    }

    let all_on = Env::build(
        "spec-on",
        vec![],
        TaskConfig {
            enable_effort: true,
            steps: true,
            structured: true,
            per_call_model: "on".into(),
            isolation: TaskIsolationConfig {
                enabled: true,
                ..TaskIsolationConfig::default()
            },
            ..TaskConfig::default()
        },
        MemFs::default(),
        vec![],
    );
    let on_props = &SubagentTool::new(all_on.rt.clone()).spec().parameters["properties"]["tasks"]["items"]
        ["properties"];
    for present in [
        "effort",
        "steps",
        "outputSchema",
        "schemaMode",
        "isolated",
        "model",
    ] {
        assert!(
            on_props.get(present).is_some(),
            "`{present}` must appear when enabled"
        );
    }
}

#[test]
fn task_permissive_schema_warns_and_level_tagged_profile_conflicts() {
    let schema = json!({
        "type": "object",
        "required": ["ok"],
        "properties": {"ok": {"type": "boolean"}}
    });
    let bad = json!({"result": {"ok": "wrong"}});
    let perm = Env::build(
        "schema-perm",
        vec![
            tool_call("c0", "yield", &bad),
            reply("d0"),
            reply("r1"),
            reply("r2"),
            reply("r3"),
        ],
        TaskConfig {
            structured: true,
            ..TaskConfig::default()
        },
        MemFs::default(),
        vec![],
    );
    perm.call(&json!({
        "context": "audit",
        "tasks": [{
            "name": "perm-kid",
            "task": "score",
            "solutionSpace": "s",
            "outputSchema": schema,
            "schemaMode": "permissive"
        }]
    }))
    .unwrap();
    perm.run_all();
    let psnap = perm.rt.agents().get("perm-kid").unwrap().snapshot();
    assert_eq!(psnap.status, JobStatus::Idle);
    assert!(
        psnap.output.contains("[schema warning]"),
        "{}",
        psnap.output
    );

    let models = vec![ModelInfo {
        alias: "flash".into(),
        id: "qwen-flash".into(),
        provider: "local".into(),
        default: true,
    }];
    let env = Env::build(
        "lvl-conflict",
        vec![],
        TaskConfig {
            per_call_model: "on".into(),
            ..TaskConfig::default()
        },
        MemFs::default(),
        models,
    );
    let profile_path = env.tmp.0.join(".harness/agents/tagged.md");
    let fs = MemFs::default().with(
        profile_path.to_string_lossy().as_ref(),
        "---\nmodel: local/qwen-flash:high\n---\nTagged.",
    );
    let rt = AgentRuntime {
        fs: Arc::new(fs),
        ..env.rt.clone()
    };
    let err = block_on(SubagentTool::new(rt).execute(
        &env.ctx(PermissionMode::Yolo),
        json!({
            "context": "c",
            "tasks": [{"agent": "tagged", "task": "t", "solutionSpace": "s", "model": "local/qwen-flash"}]
        }),
    ))
    .expect_err("level-tagged profile must conflict with per-call model")
    .to_string();
    assert!(err.contains("level-tagged"), "{err}");
}

#[test]
fn task_plan_mode_dynamic_tools_and_budgets() {
    let def_env = Env::build(
        "dyn-off",
        vec![],
        TaskConfig::default(),
        MemFs::default(),
        vec![],
    );
    let err = def_env
        .call(&json!({
            "context": "c",
            "tasks": [{"task": "t", "solutionSpace": "s", "tools": ["custom_eval_fn"]}]
        }))
        .expect_err("dynamic tools disabled")
        .to_string();
    assert!(
        err.contains("eval kernel tools are not enabled in this runtime"),
        "{err}"
    );

    let mut plan_env = Env::build(
        "plan-mode",
        vec![reply("ok")],
        TaskConfig {
            isolation: TaskIsolationConfig {
                enabled: true,
                ..TaskIsolationConfig::default()
            },
            ..TaskConfig::default()
        },
        MemFs::default(),
        vec![],
    );
    plan_env.rt = plan_env
        .rt
        .with_isolator(Arc::new(StubIsolator::new(plan_env.tmp.0.clone(), "")))
        .with_dynamic_tools(Arc::new(StubDynamicTools));
    let plan_ctx = plan_env.ctx(PermissionMode::Plan);
    assert!(
        block_on(SubagentTool::new(plan_env.rt.clone()).execute(
            &plan_ctx,
            json!({"context": "c", "tasks": [{"task": "t", "solutionSpace": "s", "isolated": true}]})
        ))
        .is_err()
    );
    assert!(
        block_on(SubagentTool::new(plan_env.rt.clone()).execute(
            &plan_ctx,
            json!({"context": "c", "tasks": [{"task": "t", "solutionSpace": "s", "tools": ["custom_eval_fn"]}]})
        ))
        .is_err()
    );
    block_on(SubagentTool::new(plan_env.rt.clone()).execute(
        &plan_ctx,
        json!({"context": "c", "tasks": [{"name": "ro-task", "agent": "task", "task": "t", "solutionSpace": "s"}]}),
    ))
    .unwrap();
    let sid = plan_env
        .rt
        .agents()
        .get("ro-task")
        .unwrap()
        .snapshot()
        .session_id
        .unwrap();
    assert_eq!(
        plan_env
            .store
            .get_session(&sid)
            .unwrap()
            .unwrap()
            .permission_mode,
        PermissionMode::Plan
    );

    // Soft budget = 2 -> hard stop at 3 requests.
    let bud = Env::build(
        "budget",
        vec![
            tool_call("c1", "read", &json!({})),
            tool_call("c2", "read", &json!({})),
            tool_call("c3", "read", &json!({})),
            reply("never"),
        ],
        TaskConfig {
            soft_request_budget: 2,
            ..TaskConfig::default()
        },
        MemFs::default(),
        vec![],
    );
    bud.call(
        &json!({"context": "c", "tasks": [{"name": "b", "task": "loop", "solutionSpace": "s"}]}),
    )
    .unwrap();
    bud.run_all();
    let bsnap = bud.rt.agents().get("b").unwrap().snapshot();
    assert_eq!(bsnap.status, JobStatus::Failed);
    assert!(
        bsnap.output.contains("hard request budget (3)"),
        "{}",
        bsnap.output
    );
    bud.clock.advance(10);
}

#[test]
fn task_sync_mode_returns_summary() {
    let env = Env::build(
        "sync",
        vec![reply("synchronous result payload")],
        TaskConfig {
            async_mode: false,
            ..TaskConfig::default()
        },
        MemFs::default(),
        vec![],
    );
    let out = env
        .call(&json!({"context": "sync", "tasks": [{"name": "syncer", "task": "compute", "solutionSpace": "s"}]}))
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("synchronous result payload"),
        "{}",
        out.content
    );
    assert!(env.spawned.0.lock().unwrap().is_empty());
    assert_eq!(env.rt.agents().inbox().queued(&env.parent.id), 0);
    assert_eq!(
        env.rt.agents().get("syncer").unwrap().snapshot().status,
        JobStatus::Idle
    );
}

#[test]
fn task_per_call_model_gated() {
    let models = vec![
        ModelInfo {
            alias: "sonnet".into(),
            id: "claude-sonnet".into(),
            provider: "anthropic".into(),
            default: true,
        },
        ModelInfo {
            alias: "flash".into(),
            id: "qwen-flash".into(),
            provider: "local".into(),
            default: false,
        },
    ];
    let off = Env::build(
        "m-off",
        vec![],
        TaskConfig::default(),
        MemFs::default(),
        models.clone(),
    );
    assert!(
        off.call(&json!({"context": "c", "tasks": [{"task": "t", "solutionSpace": "s", "model": "local/qwen-flash"}]}))
            .is_err()
    );

    let mut agent_models = BTreeMap::new();
    agent_models.insert("reviewer".to_owned(), "anthropic/claude-sonnet".to_owned());
    let on = Env::build(
        "m-on",
        vec![reply("ok")],
        TaskConfig {
            per_call_model: "on".into(),
            per_call_model_allow: vec!["local/*".into()],
            agent_models,
            ..TaskConfig::default()
        },
        MemFs::default(),
        models,
    );
    assert!(
        on.call(&json!({"context": "c", "tasks": [{"task": "t", "solutionSpace": "s", "model": "flash"}]}))
            .is_err()
    );
    assert!(
        on.call(&json!({"context": "c", "tasks": [{"task": "t", "solutionSpace": "s", "model": "anthropic/claude-sonnet"}]}))
            .is_err()
    );
    assert!(
        on.call(&json!({"context": "c", "tasks": [{"agent": "reviewer", "task": "t", "solutionSpace": "s", "model": "local/qwen-flash"}]}))
            .is_err()
    );
    let ok = on
        .call(&json!({"context": "c", "tasks": [{"name": "okm", "agent": "task", "task": "t", "solutionSpace": "s", "model": "local/qwen-flash:high"}]}))
        .unwrap();
    assert!(!ok.is_error);
    let sid = on
        .rt
        .agents()
        .get("okm")
        .unwrap()
        .snapshot()
        .session_id
        .unwrap();
    let meta = on.store.get_session(&sid).unwrap().unwrap();
    assert_eq!(
        (meta.model.as_deref(), meta.thinking),
        (Some("local/qwen-flash"), ThinkingLevel::High)
    );
}

#[test]
fn task_unknown_agent_lists_alternatives() {
    let env = Env::build(
        "unk",
        vec![],
        TaskConfig::default(),
        MemFs::default(),
        vec![],
    );
    let err = env
        .call(&json!({"context": "ctx", "tasks": [{"agent": "nonexistent", "task": "work", "solutionSpace": "s"}]}))
        .expect_err("unknown agent")
        .to_string();
    assert!(err.contains("scout") && err.contains("explore"), "{err}");
    assert!(err.contains("sonic") && err.contains("quick_task"), "{err}");
    assert!(err.contains("security-reviewer"), "{err}");
}
