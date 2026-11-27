use super::*;
use crate::summary::{build_summary_prompt, extract_modified_files, extract_todos, plan_summary};
use hrn_kernel::{ImageAttachment, TodoItem, ToolCall};
use serde_json::{Value, json};

fn node(id: &str, payload: DagPayload) -> DagNode {
    DagNode {
        id: id.into(),
        session_id: "s1".into(),
        parent_id: None,
        created_at_ms: 0,
        payload,
    }
}

fn msg_node(id: &str, msg: Msg) -> DagNode {
    node(
        id,
        DagPayload::Message {
            turn_id: "t".into(),
            msg,
        },
    )
}

fn call(id: &str, name: &str, args: &Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
        signature: None,
    }
}

fn read_result(id: &str, key: &str, body: &str) -> Msg {
    let mut m = Msg::tool_result(id, "read", body);
    m.read_key = Some(key.into());
    m
}

#[test]
fn batched_supersede_reads_triggers_above_threshold() {
    let big = "line\n".repeat(400);
    let args = json!({"path": "src/main.rs"});
    let nodes = vec![
        msg_node("1", Msg::user("inspect")),
        msg_node(
            "2",
            Msg::assistant_tool_calls("", None, vec![call("c1", "read", &args)]),
        ),
        msg_node("3", read_result("c1", "/w/src/main.rs:1-400", &big)),
        msg_node(
            "4",
            Msg::assistant_tool_calls("", None, vec![call("c2", "read", &args)]),
        ),
        msg_node("5", read_result("c2", "/w/src/main.rs:1-400", &big)),
        // Same file, different range: must NOT be superseded by (or supersede) the others.
        msg_node("6", read_result("c3", "/w/src/main.rs:500-900", &big)),
    ];
    let proj = project_branch(&nodes, &CompactionPolicy::default());
    assert_eq!(proj.superseded_reads, 1);
    assert!(
        proj.messages
            .get(2)
            .is_some_and(|m| m.content.contains("superseded"))
    );
    assert_eq!(
        proj.messages.get(4).map(|m| m.content.len()),
        Some(big.len())
    );
    assert_eq!(
        proj.messages.get(5).map(|m| m.content.len()),
        Some(big.len())
    );
}

#[test]
fn small_reclaimable_share_leaves_history_untouched() {
    let big = "x".repeat(300);
    let huge = "y".repeat(60_000);
    let nodes = vec![
        msg_node("1", read_result("c1", "/a:1-9", &big)),
        msg_node("2", read_result("c2", "/a:1-9", &big)),
        msg_node("3", Msg::assistant(huge)),
    ];
    assert_eq!(
        project_branch(&nodes, &CompactionPolicy::default()).superseded_reads,
        0
    );
}

fn long_session(turns: usize) -> Vec<DagNode> {
    let mut nodes = Vec::new();
    for t in 0..turns {
        nodes.push(msg_node(
            &format!("u{t}"),
            Msg::user(format!("question {t}")),
        ));
        nodes.push(msg_node(
            &format!("a{t}"),
            Msg::assistant_tool_calls(
                "",
                None,
                vec![call(&format!("c{t}"), "read", &json!({"path": "f"}))],
            ),
        ));
        nodes.push(msg_node(
            &format!("r{t}"),
            Msg::tool_result(format!("c{t}"), "read", "ok"),
        ));
        nodes.push(msg_node(
            &format!("f{t}"),
            Msg::assistant(format!("answer {t}")),
        ));
    }
    nodes
}

#[test]
fn summary_cut_keeps_the_last_turns_and_never_splits_a_tool_pair() {
    let nodes = long_session(10);
    let plan = plan_summary(&nodes, 3).expect("plan");
    // Tail = last 3 user turns (u7..), so the cut sits right before u7.
    assert_eq!(plan.covers_node_id, "f6");
    assert!(matches!(
        &nodes.get(plan.covers_idx + 1).map(|n| &n.payload),
        Some(DagPayload::Message { msg, .. }) if msg.role == Role::User
    ));
    assert!(
        plan_summary(&long_session(3), 3).is_none(),
        "nothing to fold"
    );
}

#[test]
fn projection_after_summary_is_summary_plus_verbatim_tail() {
    let mut nodes = long_session(10);
    let plan = plan_summary(&nodes, 3).expect("plan");
    nodes.push(node(
        "sum",
        DagPayload::Summary {
            covers_node_id: plan.covers_node_id,
            text: "S".into(),
            method: None,
            preserve: None,
        },
    ));
    nodes.push(msg_node("u10", Msg::user("next")));
    let proj = project_branch(&nodes, &CompactionPolicy::default());
    let first = proj.messages.first().expect("summary msg");
    assert!(first.content.contains("[Conversation summary") && first.content.ends_with('S'));
    // 3 kept turns * 4 messages + the new user message + summary.
    assert_eq!(proj.messages.len(), 1 + 12 + 1);
    assert_eq!(
        proj.messages.get(1).map(|m| m.content.as_str()),
        Some("question 7")
    );
    // A second plan must start after the first cut.
    let again = plan_summary(&nodes, 3);
    assert!(again.is_none_or(|p| p.covers_idx > plan.covers_idx));
}

#[test]
fn projection_carries_remote_preserve_and_snapcompact_images() {
    let nodes = vec![
        msg_node("u0", Msg::user("old")),
        node(
            "s_remote",
            DagPayload::Summary {
                covers_node_id: "u0".into(),
                text: "remote fallback".into(),
                method: Some("remote".into()),
                preserve: Some(json!({
                    "provider": "openai_responses",
                    "output": [{"type": "compaction", "encrypted_content": "blob123"}]
                })),
            },
        ),
        msg_node("u1", Msg::user("after remote")),
    ];
    let proj = project_branch(&nodes, &CompactionPolicy::default());
    let first = proj.messages.first().expect("remote msg");
    assert!(first.content.contains("remote fallback"));
    assert!(
        first
            .read_key
            .as_deref()
            .is_some_and(|k| k.starts_with(REMOTE_COMPACTION_PREFIX) && k.contains("blob123"))
    );

    let img = ImageAttachment {
        media_type: "image/png".into(),
        data_base64: "iVBORw0KGgo=".into(),
        label: Some("snapcompact-page-01.png".into()),
        width: Some(100),
        height: Some(100),
    };
    let snap_nodes = vec![
        msg_node("u0", Msg::user("old")),
        node(
            "s_snap",
            DagPayload::Summary {
                covers_node_id: "u0".into(),
                text: "1 archived page".into(),
                method: Some("snapcompact".into()),
                preserve: Some(json!({ "images": [img] })),
            },
        ),
        msg_node("u1", Msg::user("after snap")),
    ];
    let snap_proj = project_branch(&snap_nodes, &CompactionPolicy::default());
    let snap_first = snap_proj.messages.first().expect("snap msg");
    assert_eq!(snap_first.images.len(), 1);
    assert_eq!(snap_first.images[0].media_type, "image/png");
    assert_eq!(snap_first.images[0].data_base64, "iVBORw0KGgo=");
}

#[test]
fn single_long_agentic_turn_falls_back_to_assistant_step_cut() {
    let mut nodes = vec![msg_node("u", Msg::user("do everything"))];
    for i in 0..20 {
        nodes.push(msg_node(
            &format!("a{i}"),
            Msg::assistant_tool_calls("", None, vec![call(&format!("c{i}"), "bash", &json!({}))]),
        ));
        nodes.push(msg_node(
            &format!("r{i}"),
            Msg::tool_result(format!("c{i}"), "bash", "out"),
        ));
    }
    let plan = plan_summary(&nodes, 3).expect("plan");
    assert!(matches!(
        &nodes.get(plan.covers_idx + 1).map(|n| &n.payload),
        Some(DagPayload::Message { msg, .. }) if msg.role == Role::Assistant
    ));
}

#[test]
fn estimate_counts_tool_call_arguments_and_overhead() {
    let big_args = json!({"content": "z".repeat(40_000)});
    let nodes = vec![msg_node(
        "1",
        Msg::assistant_tool_calls("", None, vec![call("c", "write", &big_args)]),
    )];
    let policy = CompactionPolicy {
        fixed_overhead_tokens: 5,
        ..CompactionPolicy::default()
    };
    assert!(project_branch(&nodes, &policy).estimated_tokens >= 10_000 + 5);
}

#[test]
fn todos_and_modified_files_come_from_the_branch() {
    let nodes = vec![
        msg_node(
            "1",
            Msg::assistant_tool_calls(
                "",
                None,
                vec![
                    call(
                        "t1",
                        "todo",
                        &json!({"items": [{"id": "1", "content": "old", "status": "pending"}]}),
                    ),
                    call("e1", "edit", &json!({"path": "src/a.rs"})),
                ],
            ),
        ),
        msg_node(
            "2",
            Msg::assistant_tool_calls(
                "",
                None,
                vec![
                    call(
                        "t2",
                        "todo",
                        &json!({"items": [{"id": "1", "content": "new", "status": "done"}]}),
                    ),
                    call(
                        "t3",
                        "todo",
                        &json!({"items": [{"content": "bad", "status": "weird"}]}),
                    ),
                    call(
                        "p1",
                        "apply_patch",
                        &json!({"patch": "*** Begin Patch\n*** Update File: src/b.rs\n@@\n-x\n+y\n*** End Patch"}),
                    ),
                ],
            ),
        ),
    ];
    let todos = extract_todos(&nodes);
    assert_eq!(todos.len(), 1);
    assert_eq!(todos.first().map(|t| t.content.as_str()), Some("new"));
    assert_eq!(extract_modified_files(&nodes), ["src/a.rs", "src/b.rs"]);
}

#[test]
fn summary_prompt_mentions_tool_calls_files_and_todos() {
    let msgs = vec![
        Msg::user("fix it"),
        Msg::assistant_tool_calls("", None, vec![call("c", "edit", &json!({"path": "a.rs"}))]),
    ];
    let todos = vec![TodoItem {
        content: "ship".into(),
        status: hrn_kernel::TodoStatus::InProgress,
    }];
    let prompt = build_summary_prompt(&msgs, &todos, &["a.rs".to_owned()]);
    let body = &prompt.get(1).expect("user msg").content;
    assert!(body.contains("edit") && body.contains("[FILES MODIFIED]") && body.contains("ship"));
}
