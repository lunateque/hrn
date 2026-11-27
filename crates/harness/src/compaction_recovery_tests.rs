//! Wave 10d recovery tests: overflow compaction retry, context promotion precedence,
//! `FinishReason::Length` recovery, `mid_turn_enabled = false` deferral, and idle compaction.

use super::{BigOutputTool, CompactionTestLlm, make_io, ok_resp, seed_turns, tool_call_resp};
use crate::compaction::{CompactionConfig, IdleCompactionConfig, ThresholdSpec};
use crate::compaction::{CompactionPolicy, project_branch};
use crate::control::SessionControl;
use crate::registry::ToolRegistry;
use crate::runtime::{AgentRuntime, HarnessConfig};
use crate::testutil::{MemFs, MemStore, VecEvents, block_on};
use hrn_kernel::{
    AgentEvent, CancelFlag, DagPayload, EventSink, FinishReason, HrnError, ModelInfo,
    PermissionMode, Role, SessionStore, ThinkingLevel,
};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[test]
fn overflow_retries_once_after_compaction() {
    let overflow_err = || {
        Err(HrnError::Provider {
            message: "context_length_exceeded: maximum context length reached".into(),
            retryable: false,
            retry_after_secs: None,
        })
    };
    let llm = CompactionTestLlm::new(
        vec![
            overflow_err(),
            Ok(ok_resp(
                "Compact summary after overflow.",
                FinishReason::Stop,
            )),
            Ok(ok_resp("recovered after compact", FinishReason::Stop)),
        ],
        200_000,
    );
    let store = Arc::new(MemStore::default());
    let cfg = CompactionConfig {
        method_order: vec!["handoff".into(), "shake".into(), "summary".into()],
        ..CompactionConfig::default()
    };
    let rt = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            compaction: CompactionPolicy::from_config(cfg),
            context_promotion: false,
            ..HarnessConfig::default()
        },
    );
    let meta = rt
        .create_session(
            "overflow",
            Path::new("/ws"),
            Some("small".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    seed_turns(&store, &meta.id, 4, 1_200);

    let events = Arc::new(VecEvents::default());
    let res = block_on(rt.run_turn(
        &meta.id,
        "fix overflow bug",
        &SessionControl::default(),
        make_io(&events),
    ))
    .expect("turn recovers after overflow compaction");
    assert_eq!(res.final_text, "recovered after compact");

    let captured = llm.requests.lock().expect("reqs");
    assert_eq!(captured.len(), 3, "1 overflow + 1 summary + 1 retry");
    let user_hits = captured[2]
        .messages
        .iter()
        .filter(|m| m.role == Role::User && m.content == "fix overflow bug")
        .count();
    assert_eq!(user_hits, 1, "failing user message must not be duplicated");
    assert!(captured[2].messages.iter().any(|m| {
        m.content
            .contains("[Conversation summary from earlier turns]")
    }));
}

#[test]
fn promotion_precedes_compaction_and_keeps_roles() {
    let mut windows = BTreeMap::new();
    windows.insert("small".to_owned(), 64_000);
    windows.insert("large".to_owned(), 256_000);
    let mut promotion_targets = BTreeMap::new();
    promotion_targets.insert("small".to_owned(), "large".to_owned());

    let llm = Arc::new(CompactionTestLlm {
        responses: Mutex::new(VecDeque::from(vec![
            Err(HrnError::Provider {
                message: "context_length_exceeded".into(),
                retryable: false,
                retry_after_secs: None,
            }),
            Ok(ok_resp(
                "solved on promoted large model",
                FinishReason::Stop,
            )),
        ])),
        requests: Mutex::new(Vec::new()),
        models: Vec::new(),
        windows,
        default_window: 64_000,
        promotion_targets,
    });
    let store = Arc::new(MemStore::default());
    let rt = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            context_promotion: true,
            ..HarnessConfig::default()
        },
    );
    let meta = rt
        .create_session(
            "promote",
            Path::new("/ws"),
            Some("small".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    seed_turns(&store, &meta.id, 4, 800);

    let events = Arc::new(VecEvents::default());
    let res = block_on(rt.run_turn(
        &meta.id,
        "deep audit",
        &SessionControl::default(),
        make_io(&events),
    ))
    .expect("promotion retry ok");
    assert_eq!(res.final_text, "solved on promoted large model");

    let captured = llm.requests.lock().expect("reqs");
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].model.as_deref(), Some("small"));
    assert_eq!(captured[1].model.as_deref(), Some("large"));

    let updated = store.get_session(&meta.id).expect("get").expect("meta");
    assert_eq!(updated.model.as_deref(), Some("large"));
    let tree = store.load_tree(&meta.id).expect("tree");
    assert!(tree.iter().any(|n| matches!(
        &n.payload,
        DagPayload::ModelChange { selector, role }
            if selector == "large" && role.as_deref() == Some("fallback")
    )));
    assert!(
        !tree
            .iter()
            .any(|n| matches!(n.payload, DagPayload::Summary { .. })),
        "promotion must skip compaction"
    );
    let user_nodes = tree
        .iter()
        .filter(|n| matches!(&n.payload, DagPayload::Message { msg, .. } if msg.content == "deep audit"))
        .count();
    assert_eq!(user_nodes, 1, "user prompt must not be duplicated");
}

#[test]
fn length_stop_triggers_recovery() {
    let llm = CompactionTestLlm::new(
        vec![
            Ok(ok_resp(
                "Halfway through the answer...",
                FinishReason::Length,
            )),
            Ok(ok_resp("Summary of earlier turns.", FinishReason::Stop)),
            Ok(ok_resp(
                "finished after length recovery",
                FinishReason::Stop,
            )),
        ],
        200_000,
    );
    let store = Arc::new(MemStore::default());
    let cfg = CompactionConfig {
        auto_continue: true,
        method_order: vec!["summary".into()],
        ..CompactionConfig::default()
    };
    let rt = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            compaction: CompactionPolicy::from_config(cfg),
            ..HarnessConfig::default()
        },
    );
    let meta = rt
        .create_session(
            "length",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    seed_turns(&store, &meta.id, 4, 1_000);

    let events = Arc::new(VecEvents::default());
    let res = block_on(rt.run_turn(
        &meta.id,
        "write full report",
        &SessionControl::default(),
        make_io(&events),
    ))
    .expect("length recovery ok");
    assert_eq!(res.final_text, "finished after length recovery");
    assert_eq!(llm.requests.lock().expect("reqs").len(), 3);
    assert!(events.0.lock().expect("events").iter().any(|e| matches!(
        e,
        AgentEvent::Compacted { method, .. } if method == "summary"
    )));
}

#[test]
fn mid_turn_disabled_defers() {
    let llm = CompactionTestLlm::new(
        vec![
            Ok(tool_call_resp("c1", "big_tool")),
            Ok(ok_resp(
                "finished without mid-turn compact",
                FinishReason::Stop,
            )),
            Ok(ok_resp("Post-turn summary.", FinishReason::Stop)),
        ],
        20_000,
    );
    let store = Arc::new(MemStore::default());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BigOutputTool {
        payload: "x".repeat(20_000),
    }));
    let cfg = CompactionConfig {
        threshold_tokens: 4_000,
        reserve_tokens: Some(500),
        mid_turn_enabled: false,
        method_order: vec!["summary".into()],
        ..CompactionConfig::default()
    };
    let rt = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        tools,
        Arc::new(MemFs::default()),
        HarnessConfig {
            compaction: CompactionPolicy::from_config(cfg),
            ..HarnessConfig::default()
        },
    );
    let meta = rt
        .create_session(
            "defer",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    seed_turns(&store, &meta.id, 2, 200);

    let events = Arc::new(VecEvents::default());
    let res = block_on(rt.run_turn(
        &meta.id,
        "run big tool",
        &SessionControl::default(),
        make_io(&events),
    ))
    .expect("turn ok");
    assert_eq!(res.final_text, "finished without mid-turn compact");

    let captured = llm.requests.lock().expect("reqs");
    assert_eq!(
        captured.len(),
        3,
        "step 0 + step 1 (uncompacted) + post-turn summary"
    );
    assert!(
        !captured[1]
            .messages
            .iter()
            .any(|m| m.content.contains("[Conversation summary")),
        "mid_turn_enabled = false must NOT compact before step 1"
    );
    let updated = store.get_session(&meta.id).expect("get").expect("meta");
    let branch = store
        .load_branch(&meta.id, updated.head_node_id.as_deref().expect("head"))
        .expect("branch");
    let proj = project_branch(&branch, &CompactionPolicy::default());
    assert!(
        proj.messages
            .first()
            .is_some_and(|m| m.content.contains("[Conversation summary"))
    );
}

#[test]
fn idle_compaction_no_autocontinue() {
    let llm = CompactionTestLlm::new(
        vec![Ok(ok_resp("Idle summary.", FinishReason::Stop))],
        200_000,
    );
    let store = Arc::new(MemStore::default());
    let cfg = CompactionConfig {
        auto_continue: true,
        method_order: vec!["summary".into()],
        idle: IdleCompactionConfig {
            enabled: true,
            threshold_tokens: 1_000,
            timeout_secs: 30,
        },
        ..CompactionConfig::default()
    };
    let rt = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            compaction: CompactionPolicy::from_config(cfg),
            ..HarnessConfig::default()
        },
    );
    let meta = rt
        .create_session(
            "idle",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    seed_turns(&store, &meta.id, 4, 1_000);

    let events = Arc::new(VecEvents::default());
    let sink: Arc<dyn EventSink> = Arc::clone(&events) as Arc<dyn EventSink>;

    let early = block_on(rt.compact_idle(&meta.id, 45, &CancelFlag::new(), &sink)).expect("early");
    assert!(early.committed.is_none());
    assert!(!early.allow_auto_continue);
    assert_eq!(llm.requests.lock().expect("reqs").len(), 0);

    let done = block_on(rt.compact_idle(&meta.id, 60, &CancelFlag::new(), &sink)).expect("idle");
    assert!(done.committed.is_some());
    assert!(
        !done.allow_auto_continue,
        "idle compaction must NEVER auto-continue"
    );
    assert_eq!(llm.requests.lock().expect("reqs").len(), 1);
    assert!(events.0.lock().expect("events").iter().any(|e| matches!(
        e,
        AgentEvent::Compacted { method, .. } if method == "summary"
    )));
}

#[test]
fn model_thresholds_prefix_match() {
    let mut thresholds = BTreeMap::new();
    thresholds.insert("anthropic/*".to_owned(), ThresholdSpec::Percent(60));
    thresholds.insert(
        "anthropic/claude-opus*".to_owned(),
        ThresholdSpec::Fixed(100_000),
    );
    thresholds.insert(
        "anthropic/claude-opus-4-5".to_owned(),
        ThresholdSpec::Base(80_000),
    );
    let cfg = CompactionConfig {
        model_thresholds: thresholds,
        ..CompactionConfig::default()
    };

    assert_eq!(
        cfg.trigger_tokens(200_000, Some("anthropic/claude-opus-4-5")),
        80_000
    );
    assert_eq!(
        cfg.trigger_tokens(200_000, Some("anthropic/claude-opus-4-6")),
        100_000
    );
    assert_eq!(
        cfg.trigger_tokens(200_000, Some("anthropic/claude-sonnet-4")),
        120_000
    );
    assert_eq!(cfg.trigger_tokens(200_000, Some("openai/gpt-5")), 170_000);

    let llm = Arc::new(CompactionTestLlm {
        responses: Mutex::new(VecDeque::new()),
        requests: Mutex::new(Vec::new()),
        models: vec![ModelInfo {
            alias: "opus".into(),
            provider: "anthropic".into(),
            id: "claude-opus-4-6".into(),
            default: true,
        }],
        windows: BTreeMap::new(),
        default_window: 200_000,
        promotion_targets: BTreeMap::new(),
    });
    let rt = AgentRuntime::new(
        llm,
        Arc::new(MemStore::default()),
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            compaction: CompactionPolicy::from_config(cfg),
            ..HarnessConfig::default()
        },
    );
    assert_eq!(rt.policy_for(Some("opus")).trigger_tokens(), 100_000);
}

#[test]
fn custom_instructions_appended() {
    let llm = CompactionTestLlm::new(
        vec![Ok(ok_resp("Summary keeping DB IDs.", FinishReason::Stop))],
        200_000,
    );
    let store = Arc::new(MemStore::default());
    let cfg = CompactionConfig {
        method_order: vec!["shake".into(), "summary".into()],
        ..CompactionConfig::default()
    };
    let rt = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            compaction: CompactionPolicy::from_config(cfg),
            ..HarnessConfig::default()
        },
    );
    let meta = rt
        .create_session(
            "instr",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    seed_turns(&store, &meta.id, 4, 400);

    let events = Arc::new(VecEvents::default());
    let sink: Arc<dyn EventSink> = Arc::clone(&events) as Arc<dyn EventSink>;
    let covered = block_on(rt.compact_now(
        &meta.id,
        Some("preserve all migration IDs and schema notes"),
        &CancelFlag::new(),
        &sink,
    ))
    .expect("compact ok");
    assert!(covered.is_some());

    let captured = llm.requests.lock().expect("reqs");
    assert_eq!(captured.len(), 1);
    assert!(captured[0].messages.iter().any(|m| {
        m.content
            .contains("preserve all migration IDs and schema notes")
    }));
}
