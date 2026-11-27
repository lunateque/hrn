//! Acceptance tests for Wave 9a2: `/handoff`, `/branch`, shared leaf rule, node labels,
//! abandoned-branch summaries, and `/btw` side-thread persistence.

use crate::branch_ops::{resolve_labels, resolve_leaf_selection};
use crate::btw::{BtwStatus, extract_inject_block};
use crate::compaction::{CompactionPolicy, project_branch};
use crate::registry::ToolRegistry;
use crate::runtime::{AgentRuntime, HarnessConfig};
use crate::summary::COMPACTION_METHODS;
use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents, block_on};
use hrn_kernel::{
    CancelFlag, DagNode, DagPayload, EventSink, FinishReason, LlmResponse, Msg, PermissionMode,
    Role, SessionStore, ThinkingLevel,
};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn scripted_llm(responses: Vec<LlmResponse>) -> Arc<ScriptedLlm> {
    Arc::new(ScriptedLlm {
        responses: Mutex::new(VecDeque::from(
            responses.into_iter().map(Ok).collect::<Vec<_>>(),
        )),
        ..ScriptedLlm::default()
    })
}

fn text_resp(content: &str) -> LlmResponse {
    LlmResponse {
        content: content.into(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        usage: None,
        model_used: "mock".into(),
        fallback_from: None,
        finish_reason: FinishReason::Stop,
    }
}

fn append_msg(store: &MemStore, sid: &str, id: &str, parent: Option<&str>, msg: Msg) {
    store
        .append_node(&DagNode {
            id: id.into(),
            session_id: sid.into(),
            parent_id: parent.map(Into::into),
            created_at_ms: 100,
            payload: DagPayload::Message {
                turn_id: "t1".into(),
                msg,
            },
        })
        .expect("append");
    let mut meta = store.get_session(sid).expect("get").expect("meta");
    meta.head_node_id = Some(id.into());
    store.update_session(&meta).expect("update");
}

#[test]
fn handoff_requires_goal_section() {
    assert!(COMPACTION_METHODS.contains(&"handoff"));
    let llm = scripted_llm(vec![
        text_resp("Summary without the required heading.\n\n## Next Steps\n- keep going"),
        text_resp("## Goal\nShip wave 9a2 cleanly.\n\n## Next Steps\n- run CI"),
    ]);
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let meta = runtime
        .create_session(
            "handoff",
            Path::new("/ws"),
            Some("default".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    append_msg(&store, &meta.id, "n1", None, Msg::user("build feature"));
    append_msg(&store, &meta.id, "n2", Some("n1"), Msg::assistant("on it"));
    let events: Arc<dyn EventSink> = Arc::new(VecEvents::default());

    let err = block_on(runtime.handoff_now(&meta.id, None, &CancelFlag::new(), &events))
        .expect_err("handoff without ## Goal must fail");
    assert!(err.to_string().contains("## Goal"), "{err}");

    let covered = block_on(runtime.handoff_now(&meta.id, None, &CancelFlag::new(), &events))
        .expect("handoff with ## Goal succeeds");
    assert_eq!(covered.as_deref(), Some("n2"));

    let updated = store.get_session(&meta.id).expect("get").expect("meta");
    let branch = store
        .load_branch(&meta.id, updated.head_node_id.as_deref().expect("head"))
        .expect("branch");
    let projected = project_branch(&branch, &CompactionPolicy::default());
    assert!(
        projected.messages.first().is_some_and(|m| m
            .content
            .starts_with("[Handoff from earlier turns]\n## Goal")),
        "projected handoff header mismatch: {:?}",
        projected.messages
    );
}

#[test]
fn handoff_focus_reaches_prompt() {
    let llm = scripted_llm(vec![text_resp(
        "## Goal\nMigrate auth middleware only.\n\n## Next Steps\n- update tests",
    )]);
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let meta = runtime
        .create_session(
            "focus",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    append_msg(
        &store,
        &meta.id,
        "n1",
        None,
        Msg::user("refactor everything"),
    );
    let events: Arc<dyn EventSink> = Arc::new(VecEvents::default());

    block_on(runtime.handoff_now(
        &meta.id,
        Some("focus strictly on auth middleware"),
        &CancelFlag::new(),
        &events,
    ))
    .expect("handoff");

    let reqs = llm.requests.lock().expect("reqs");
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0]
            .messages
            .iter()
            .any(|m| m.content.contains("focus strictly on auth middleware")),
        "focus text did not reach handoff prompt: {:?}",
        reqs[0].messages
    );
}

#[test]
fn handoff_discarded_on_branch_change() {
    let llm = scripted_llm(vec![text_resp("## Goal\nKeep branch A state.")]);
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let meta = runtime
        .create_session(
            "discard",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    append_msg(&store, &meta.id, "n1", None, Msg::user("first"));
    append_msg(
        &store,
        &meta.id,
        "n2",
        Some("n1"),
        Msg::assistant("branch A"),
    );
    let draft = block_on(runtime.generate_handoff(&meta.id, None, &CancelFlag::new()))
        .expect("draft")
        .expect("some");
    // Switch head back to n1 before committing -> branch changed off expected_head (n2).
    runtime.switch_head(&meta.id, "n1").expect("switch");
    let events: Arc<dyn EventSink> = Arc::new(VecEvents::default());
    let committed = block_on(runtime.commit_handoff(&draft, &events)).expect("commit");
    assert!(!committed, "branch change must discard stale handoff draft");
}

#[test]
fn tree_user_selection_lands_on_parent_and_branch_rewinds_past_prompt() {
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        scripted_llm(Vec::new()) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let meta = runtime
        .create_session(
            "tree",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    append_msg(&store, &meta.id, "u1", None, Msg::user("first prompt"));
    append_msg(
        &store,
        &meta.id,
        "a1",
        Some("u1"),
        Msg::assistant("first reply"),
    );
    append_msg(
        &store,
        &meta.id,
        "u2",
        Some("a1"),
        Msg::user("second prompt"),
    );
    append_msg(
        &store,
        &meta.id,
        "a2",
        Some("u2"),
        Msg::assistant("second reply"),
    );

    let nodes = store.load_tree(&meta.id).expect("tree");

    // Selecting current leaf is a no-op.
    let same = resolve_leaf_selection(&nodes, Some("a2"), "a2").expect("same");
    assert!(same.already_at_leaf);

    // Selecting user message u2 lands on its parent a1 and prefills "second prompt".
    let user_second = resolve_leaf_selection(&nodes, Some("a2"), "u2").expect("u2");
    assert!(!user_second.already_at_leaf);
    assert_eq!(user_second.target_head.as_deref(), Some("a1"));
    assert_eq!(user_second.prefill.as_deref(), Some("second prompt"));

    // Selecting assistant node a1 lands on a1 directly with no prefill.
    let assistant_first = resolve_leaf_selection(&nodes, Some("a2"), "a1").expect("a1");
    assert_eq!(assistant_first.target_head.as_deref(), Some("a1"));
    assert_eq!(assistant_first.prefill, None);

    // Selecting root user message u1 rewinds past the first prompt (head = None) with prefill.
    let user_first = resolve_leaf_selection(&nodes, Some("a2"), "u1").expect("u1");
    assert_eq!(user_first.target_head, None);
    assert_eq!(user_first.prefill.as_deref(), Some("first prompt"));
    let (rewound, branch) = runtime
        .switch_head_optional(&meta.id, user_first.target_head.as_deref())
        .expect("rewind");
    assert_eq!(rewound.head_node_id, None);
    assert!(branch.is_empty());
}

#[test]
fn label_resolution_last_write_wins() {
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        scripted_llm(Vec::new()) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let meta = runtime
        .create_session(
            "labels",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    append_msg(&store, &meta.id, "n1", None, Msg::user("first"));
    append_msg(&store, &meta.id, "n2", Some("n1"), Msg::assistant("reply"));

    runtime
        .set_label(&meta.id, "n1", Some("draft".into()))
        .expect("label 1");
    runtime
        .set_label(&meta.id, "n1", Some("final".into()))
        .expect("label 2");
    runtime
        .set_label(&meta.id, "n2", Some("temp".into()))
        .expect("label n2");
    runtime
        .set_label(&meta.id, "n2", None)
        .expect("clear label n2");

    let labels = resolve_labels(&store.load_tree(&meta.id).expect("tree"));
    assert_eq!(labels.get("n1").map(String::as_str), Some("final"));
    assert!(!labels.contains_key("n2"));
}

#[test]
fn branch_summary_attaches_at_new_leaf() {
    let llm = scripted_llm(vec![text_resp("Tried option A; found missing migration.")]);
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let meta = runtime
        .create_session(
            "bsum",
            Path::new("/ws"),
            None,
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    append_msg(&store, &meta.id, "n1", None, Msg::user("base"));
    append_msg(
        &store,
        &meta.id,
        "n2",
        Some("n1"),
        Msg::assistant("base reply"),
    );
    append_msg(
        &store,
        &meta.id,
        "n3",
        Some("n2"),
        Msg::user("try option A"),
    );
    append_msg(
        &store,
        &meta.id,
        "n4",
        Some("n3"),
        Msg::assistant("dead end"),
    );

    let (updated, branch) = block_on(runtime.switch_with_branch_summary(
        &meta.id,
        Some("n2"),
        true,
        &CancelFlag::new(),
    ))
    .expect("switch with summary");

    let last = branch.last().expect("branch summary attached at leaf");
    assert_eq!(last.parent_id.as_deref(), Some("n2"));
    assert_eq!(updated.head_node_id.as_deref(), Some(last.id.as_str()));
    assert!(matches!(
        &last.payload,
        DagPayload::BranchSummary { from_node_id, text }
            if from_node_id == "n4" && text.contains("missing migration")
    ));
}

#[test]
fn btw_not_in_branch_and_history_survives_restart() {
    let llm = scripted_llm(vec![
        text_resp("Side answer with <inject>check cargo.toml</inject>."),
        text_resp("Follow-up side answer."),
    ]);
    let store = Arc::new(MemStore::default());
    let runtime = AgentRuntime::new(
        Arc::clone(&llm) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let meta = runtime
        .create_session(
            "btw",
            Path::new("/ws"),
            Some("session-model".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    append_msg(&store, &meta.id, "n1", None, Msg::user("main task"));
    append_msg(
        &store,
        &meta.id,
        "n2",
        Some("n1"),
        Msg::assistant("working"),
    );
    let before_len = store.load_branch(&meta.id, "n2").expect("branch").len();

    let deltas = Arc::new(Mutex::new(String::new()));
    let d_clone = Arc::clone(&deltas);
    let topic = block_on(runtime.run_btw(
        &meta.id,
        None,
        "what crate owns this?",
        &CancelFlag::new(),
        Arc::new(move |s| d_clone.lock().expect("lock").push_str(&s)),
    ))
    .expect("btw");

    assert_eq!(topic.exchanges.len(), 1);
    assert_eq!(topic.exchanges[0].status, BtwStatus::Done);
    assert_eq!(
        extract_inject_block(&topic.exchanges[0].answer).as_deref(),
        Some("check cargo.toml")
    );

    // Side request had zero tools and used the session model.
    {
        let reqs = llm.requests.lock().expect("reqs");
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].tools.is_empty(), "btw must send no tools");
        assert_eq!(reqs[0].model.as_deref(), Some("session-model"));
        assert!(
            reqs[0]
                .messages
                .iter()
                .any(|m| m.role == Role::User && m.content == "main task")
        );
    }

    // DAG branch length is completely unchanged.
    let meta_after = store.get_session(&meta.id).expect("get").expect("meta");
    assert_eq!(meta_after.head_node_id.as_deref(), Some("n2"));
    assert_eq!(
        store.load_branch(&meta.id, "n2").expect("branch").len(),
        before_len
    );

    // Follow up on the same topic, then construct a fresh AgentRuntime (simulating restart).
    let topic2 = block_on(runtime.run_btw(
        &meta.id,
        Some(&topic.id),
        "and which module?",
        &CancelFlag::new(),
        Arc::new(|_| {}),
    ))
    .expect("followup");
    assert_eq!(topic2.exchanges.len(), 2);

    let restarted = AgentRuntime::new(
        scripted_llm(Vec::new()) as _,
        Arc::clone(&store) as _,
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig::default(),
    );
    let loaded = restarted.list_btw_topics(&meta.id).expect("list topics");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id, topic.id);
    assert_eq!(loaded[0].exchanges.len(), 2);
}
