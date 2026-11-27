//! Unit tests for [`AgentRegistry`]: slots, delivery truncation thresholds, guard drop/discard,
//! and session root resolution.

use super::*;

fn new_agent(name: Option<&str>, owner: &str) -> NewAgent {
    NewAgent {
        name: name.map(str::to_owned),
        profile: "explore".to_owned(),
        owner: owner.to_owned(),
        root: "root".to_owned(),
        parent: None,
        task: "t".to_owned(),
        cancel: CancelFlag::new(),
    }
}

#[test]
fn slots_are_limited_per_tree_and_released_on_settle() {
    let reg = Arc::new(AgentRegistry::detached());
    let a = reg
        .register(new_agent(Some("lint run"), "root"), 2)
        .unwrap();
    assert_eq!(a.record().id, "lint-run");
    let b = reg.register(new_agent(None, "root"), 2).unwrap();
    assert_eq!(b.record().id, "explore");
    assert_eq!(reg.register(new_agent(None, "root"), 2).err(), Some(2));
    assert_eq!(reg.inbox().pending("root"), 2);
    a.finish(JobStatus::Idle, "done".to_owned(), None, None);
    let c = reg.register(new_agent(None, "root"), 2).unwrap();
    assert_eq!(c.record().id, "explore-2");
    assert_eq!(reg.inbox().queued("root"), 1);
}

#[test]
fn a_dropped_guard_delivers_a_failure_and_discard_delivers_nothing() {
    let reg = Arc::new(AgentRegistry::detached());
    drop(reg.register(new_agent(None, "root"), 4).unwrap());
    let items = reg.inbox().take("root");
    assert_eq!(items.len(), 1);
    assert!(items[0].text.contains("status=\"failed\""));
    reg.register(new_agent(None, "root"), 4)
        .unwrap()
        .discard("no executor".to_owned());
    assert_eq!(reg.inbox().queued("root"), 0);
    assert_eq!(reg.inbox().pending("root"), 0);
    let last = reg.get("explore-2").unwrap().snapshot();
    assert_eq!(
        (last.status, last.output.as_str()),
        (JobStatus::Failed, "no executor")
    );
}

#[test]
fn delivery_keeps_11500_chars_whole_and_truncates_longer_to_5000() {
    let reg = Arc::new(AgentRegistry::detached());
    let exact = "a".repeat(DELIVERY_FULL_CHARS);
    let g1 = reg.register(new_agent(Some("ok"), "root"), 4).unwrap();
    g1.finish(JobStatus::Idle, exact.clone(), None, None);
    let item1 = reg.inbox().take("root").remove(0);
    assert!(item1.text.contains(&exact));
    assert!(!item1.text.contains("output truncated"));

    let over = "x".repeat(DELIVERY_FULL_CHARS + 1);
    let g2 = reg.register(new_agent(Some("big"), "root"), 4).unwrap();
    g2.finish(JobStatus::Idle, over.clone(), None, None);
    let item2 = reg.inbox().take("root").remove(0);
    assert!(item2.text.contains("output truncated to 5000 chars"));
    assert!(item2.text.contains("agent://big"));
    assert_eq!(reg.get("big").unwrap().snapshot().output, over);
    assert!(!item2.text.contains(&"x".repeat(DELIVERY_PREVIEW_CHARS + 1)));
    assert!(item2.text.contains(&"x".repeat(DELIVERY_PREVIEW_CHARS)));
}

#[test]
fn sessions_of_agents_resolve_to_their_root() {
    let reg = Arc::new(AgentRegistry::detached());
    let guard = reg.register(new_agent(None, "root"), 4).unwrap();
    guard.record().set_session("child");
    assert_eq!(reg.root_of("child"), "root");
    assert_eq!(reg.root_of("other"), "other");
    assert_eq!(hrn_kernel::ProcSource::entries(reg.as_ref()).len(), 1);
}
