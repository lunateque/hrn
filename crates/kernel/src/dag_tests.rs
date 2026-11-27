use super::*;

#[test]
fn dag_payloads_roundtrip() {
    let samples = vec![
        DagPayload::Summary {
            covers_node_id: "n1".into(),
            text: "compact".into(),
            method: Some("remote".into()),
            preserve: Some(serde_json::json!({ "cursor": 42 })),
        },
        DagPayload::SessionExit {
            reason: ExitReason::Fatal,
            pending: vec![PendingToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            }],
        },
        DagPayload::Label {
            target_node_id: "n1".into(),
            label: Some("checkpoint".into()),
        },
        DagPayload::BranchSummary {
            from_node_id: "n9".into(),
            text: "abandoned path".into(),
        },
        DagPayload::ModelChange {
            selector: "openai/gpt-5:high".into(),
            role: Some("temporary".into()),
        },
    ];
    for payload in samples {
        let json = serde_json::to_string(&payload).expect("serialize");
        let back: DagPayload = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, payload);
    }
    // Legacy v3 Summary without method/preserve deserializes cleanly.
    let legacy: DagPayload =
        serde_json::from_str(r#"{"kind":"summary","covers_node_id":"n0","text":"old"}"#)
            .expect("legacy summary");
    assert_eq!(
        legacy,
        DagPayload::Summary {
            covers_node_id: "n0".into(),
            text: "old".into(),
            method: None,
            preserve: None,
        }
    );
    // New AgentEvent variants round-trip.
    for ev in [
        AgentEvent::Compacted {
            covers_node_id: "n1".into(),
            method: "summary".into(),
            tokens_before: 1000,
            tokens_after: 200,
            saved_chars: 3200,
        },
        AgentEvent::AssistantNote {
            text: "working on tests".into(),
        },
    ] {
        let json = serde_json::to_string(&ev).expect("serialize event");
        let back: AgentEvent = serde_json::from_str(&json).expect("deserialize event");
        assert_eq!(back, ev);
    }
}
