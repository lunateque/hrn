//! Tests of the RPC event translator: frame order, ids, deltas.
use super::*;

fn types(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .map(|f| f["type"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn run(translator: &mut Translator, events: &[AgentEvent]) -> Vec<Value> {
    events.iter().flat_map(|e| translator.handle(e)).collect()
}

fn turn_start() -> AgentEvent {
    AgentEvent::TurnStart {
        turn_id: "t".to_owned(),
        user_text: "go".to_owned(),
    }
}

fn step(finish_reason: FinishReason) -> AgentEvent {
    AgentEvent::LlmCall {
        model: "m".to_owned(),
        first_token_ms: Some(1),
        duration_ms: 10,
        finish_reason,
        usage: Some(Usage::default()),
    }
}

fn turn_done() -> AgentEvent {
    AgentEvent::TurnDone {
        turn_id: "t".to_owned(),
        model: "m".to_owned(),
        text: "done".to_owned(),
        usage: Some(Usage::default()),
        steps: 2,
    }
}

#[test]
fn a_tool_step_nests_inside_its_turn_and_closes_before_the_next() {
    let mut t = Translator::default();
    let frames = run(
        &mut t,
        &[
            turn_start(),
            AgentEvent::TextDelta {
                delta: "ok".to_owned(),
            },
            step(FinishReason::ToolCalls),
            AgentEvent::ToolStart {
                call_id: "c".to_owned(),
                name: "read".to_owned(),
                arguments: "{}".to_owned(),
            },
            AgentEvent::ToolDone {
                call_id: "c".to_owned(),
                name: "read".to_owned(),
                output: "x".to_owned(),
                is_error: false,
                diff: None,
                images: Vec::new(),
            },
            AgentEvent::TextDelta {
                delta: "done".to_owned(),
            },
            step(FinishReason::Stop),
            turn_done(),
        ],
    );
    let want = [
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "message_start",
        "message_update",
        "message_update",
        "message_update",
        "message_end",
        "tool_execution_start",
        "tool_execution_end",
        "message_start",
        "message_end",
        "turn_end",
        "turn_start",
        "message_start",
        "message_update",
        "message_update",
        "message_update",
        "message_end",
        "turn_end",
        "agent_end",
    ];
    assert_eq!(types(&frames), want);
    let end = frames
        .last()
        .and_then(|f| f["messages"].as_array())
        .map_or(0, Vec::len);
    assert_eq!(end, 4);
    let first_turn_end = frames
        .iter()
        .find(|f| f["type"] == "turn_end")
        .expect("a turn ends");
    assert_eq!(
        first_turn_end["toolResults"].as_array().map_or(0, Vec::len),
        1
    );
}

#[test]
fn thinking_and_text_become_separate_blocks() {
    let mut t = Translator::default();
    let frames = run(
        &mut t,
        &[
            AgentEvent::ThinkingDelta {
                delta: "hmm".to_owned(),
            },
            AgentEvent::TextDelta {
                delta: "hi".to_owned(),
            },
        ],
    );
    let message = frames
        .iter()
        .find(|f| {
            f["type"] == "message_update" && f["assistantMessageEvent"]["type"] == "text_start"
        })
        .expect("a text block starts");
    let blocks = message["assistantMessageEvent"]["partial"]["content"]
        .as_array()
        .map_or(0, Vec::len);
    assert_eq!(blocks, 2);
}

#[test]
fn an_error_without_output_still_ends_with_an_error_message() {
    let mut t = Translator::default();
    let frames = run(
        &mut t,
        &[
            turn_start(),
            AgentEvent::TurnError {
                turn_id: "t".to_owned(),
                code: "cancelled".to_owned(),
                message: "cancelled".to_owned(),
            },
        ],
    );
    let end = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "message_end")
        .expect("a message ends");
    assert_eq!(end["message"]["stopReason"], json!("aborted"));
    assert_eq!(end["message"]["errorMessage"], json!("cancelled"));
    assert_eq!(
        frames.last().map(|f| f["type"].clone()),
        Some(json!("agent_end"))
    );
}

#[test]
fn delta_mode_leaves_out_the_snapshots() {
    let mut t = Translator::default();
    t.set_delta(true);
    let frames = run(
        &mut t,
        &[AgentEvent::TextDelta {
            delta: "a".to_owned(),
        }],
    );
    let update = frames
        .iter()
        .find(|f| f["type"] == "message_update")
        .expect("an update");
    assert_eq!(update["assistantMessageEvent"]["partial"], Value::Null);
    assert_eq!(update["message"]["content"], Value::Null);
}

#[test]
fn a_manual_compaction_is_not_streamed_as_an_auto_pair() {
    let mut t = Translator::default();
    t.set_manual_compaction(true);
    let frames = t.handle(&AgentEvent::Compacted {
        covers_node_id: "n1".to_owned(),
        method: "summary".to_owned(),
        tokens_before: 100,
        tokens_after: 20,
        saved_chars: 42,
    });
    assert!(frames.is_empty());
    t.set_manual_compaction(false);
    let frames = t.handle(&AgentEvent::Compacted {
        covers_node_id: "n1".to_owned(),
        method: "summary".to_owned(),
        tokens_before: 100,
        tokens_after: 20,
        saved_chars: 42,
    });
    assert_eq!(
        types(&frames),
        ["auto_compaction_start", "auto_compaction_end"]
    );
}

#[test]
fn assistant_note_emits_notice_frame() {
    let mut t = Translator::default();
    let frames = t.handle(&AgentEvent::AssistantNote {
        text: "Mid-turn update".to_owned(),
    });
    assert_eq!(frames.len(), 1);
    let frame = frames.first().expect("frame");
    assert_eq!(frame["type"], json!("notice"));
    assert_eq!(frame["source"], json!("assistant"));
    assert_eq!(frame["message"], json!("Mid-turn update"));
}
