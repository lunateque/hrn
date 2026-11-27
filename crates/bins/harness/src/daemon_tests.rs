//! Unit tests for `daemon` (orphan attach prompt and fail-closed approval gate).

use super::*;

#[test]
fn orphan_attach_choice_parses_r_c_and_n() {
    assert_eq!(parse_orphan_choice("r"), OrphanChoice::Restore);
    assert_eq!(parse_orphan_choice("Restore"), OrphanChoice::Restore);
    assert_eq!(parse_orphan_choice("n"), OrphanChoice::NewSession);
    assert_eq!(parse_orphan_choice("new"), OrphanChoice::NewSession);
    assert_eq!(parse_orphan_choice("c"), OrphanChoice::ContinueViewing);
    assert_eq!(parse_orphan_choice(""), OrphanChoice::ContinueViewing);
}

fn req(id: &str, reason: Option<&str>, choices: Vec<ApprovalGrantScope>) -> ApprovalRequest {
    ApprovalRequest {
        id: id.to_owned(),
        session_id: "s1".to_owned(),
        tool_name: "bash".to_owned(),
        summary: "echo hi".to_owned(),
        diff: None,
        reason: reason.map(str::to_owned),
        targets: Vec::new(),
        choices,
        tier: Some(hrn_kernel::ToolTier::Exec),
    }
}

#[tokio::test]
async fn daemon_auto_approve_refuses_or_prompts_restricted_and_forced_calls() {
    let chan = Arc::new(SessionChannel::new());
    let gate = DaemonApprovalGate {
        session_id: "s1".to_owned(),
        auto_approve: true,
        chan: Arc::clone(&chan),
    };

    // Routine multi-choice prompt with no reason auto-approves.
    let routine = gate
        .await_decision(
            req(
                "1",
                None,
                vec![ApprovalGrantScope::Once, ApprovalGrantScope::ToolSession],
            ),
            CancelFlag::new(),
        )
        .await
        .expect("routine");
    assert!(routine.is_approved());

    // Plain-only restricted/forced prompt fails closed when no daemon client is subscribed.
    let no_client = gate
        .await_decision(
            req(
                "2",
                Some("target is inside restricted folder"),
                vec![ApprovalGrantScope::Once],
            ),
            CancelFlag::new(),
        )
        .await
        .expect("no client");
    assert_eq!(no_client, ApprovalDecision::Deny);

    // With a subscribed client, mandatory prompts publish and wait for explicit resolution.
    let mut rx = chan.tx.subscribe();
    let pending = gate.await_decision(
        req(
            "3",
            Some("critical command"),
            vec![ApprovalGrantScope::Once],
        ),
        CancelFlag::new(),
    );
    let ev = rx.recv().await.expect("approval event");
    assert!(matches!(ev.event, AgentEvent::ApprovalRequested { .. }));
    let tx = chan
        .pending_approvals
        .lock()
        .unwrap()
        .remove("3")
        .expect("pending sender");
    let _ = tx.send(false);
    assert_eq!(pending.await.expect("denied"), ApprovalDecision::Deny);
}
