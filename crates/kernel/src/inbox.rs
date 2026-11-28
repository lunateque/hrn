//! Session inbox: where background work (subagents today, bash jobs later) delivers results to
//! the session that started it.
//!
//! A front-end (or the agent loop of a subagent) drains a session's items when that session is
//! idle, or at its next turn boundary, as one system-authored follow-up message. The inbox also
//! counts work still in flight, so a headless run knows when it may exit. Std-only: waiting is a
//! plain [`Future`] over wakers, so any executor can drive it.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

/// One delivered result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxItem {
    /// Stable id of what produced it (agent id, job id); lets `wait` consume exactly its own.
    pub source: String,
    /// What the model reads.
    pub text: String,
    /// One short line for a UI.
    pub summary: String,
}

#[derive(Default)]
struct State {
    queues: HashMap<String, VecDeque<InboxItem>>,
    pending: HashMap<String, usize>,
    steer_versions: HashMap<String, u64>,
    version: u64,
    wakers: Vec<Waker>,
}

impl State {
    fn bump(&mut self) {
        self.version += 1;
        for waker in self.wakers.drain(..) {
            waker.wake();
        }
    }

    fn release(&mut self, session: &str) {
        if let Some(n) = self.pending.get_mut(session) {
            *n -= 1;
            if *n == 0 {
                self.pending.remove(session);
            }
        }
    }
}

/// Per-session queues of delivered results plus the count of results still expected. Keyed by
/// the id of the session that owns the work: for the interactive session that is the root.
#[derive(Default)]
pub struct Inbox {
    state: Mutex<State>,
}

/// A promise that one result will be delivered to a session. While it lives the session counts
/// as having work in flight; dropping it unfulfilled releases that count without a result.
pub struct Expectation {
    inbox: Arc<Inbox>,
    session: String,
    open: bool,
}

impl Expectation {
    /// Deliver `item` and release the in-flight count in one step.
    pub fn deliver(mut self, item: InboxItem) {
        self.open = false;
        let mut state = self.inbox.lock();
        state.release(&self.session);
        state
            .queues
            .entry(self.session.clone())
            .or_default()
            .push_back(item);
        state.bump();
    }
}

impl Drop for Expectation {
    fn drop(&mut self) {
        if self.open {
            let mut state = self.inbox.lock();
            state.release(&self.session);
            state.bump();
        }
    }
}

/// Resolves once the inbox changed after the version it was created with.
pub struct Changed<'a> {
    inbox: &'a Inbox,
    seen: u64,
}

impl Future for Changed<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.inbox.lock();
        if state.version != self.seen {
            return Poll::Ready(());
        }
        if !state.wakers.iter().any(|w| w.will_wake(cx.waker())) {
            state.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Inbox {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Announce that one result will be delivered to `session`.
    #[must_use]
    pub fn expect(self: &Arc<Self>, session: &str) -> Expectation {
        let mut state = self.lock();
        *state.pending.entry(session.to_owned()).or_default() += 1;
        state.bump();
        drop(state);
        Expectation {
            inbox: Arc::clone(self),
            session: session.to_owned(),
            open: true,
        }
    }

    /// Deliver `item` to `session` without a prior [`Inbox::expect`].
    pub fn push(&self, session: &str, item: InboxItem) {
        let mut state = self.lock();
        state
            .queues
            .entry(session.to_owned())
            .or_default()
            .push_back(item);
        state.bump();
    }

    /// Take everything delivered to `session`, oldest first.
    pub fn take(&self, session: &str) -> Vec<InboxItem> {
        self.lock()
            .queues
            .remove(session)
            .map(Vec::from)
            .unwrap_or_default()
    }

    /// Take the delivered item produced by `source`, leaving the rest queued.
    pub fn take_source(&self, session: &str, source: &str) -> Option<InboxItem> {
        let mut state = self.lock();
        let queue = state.queues.get_mut(session)?;
        let at = queue.iter().position(|item| item.source == source)?;
        let item = queue.remove(at);
        if queue.is_empty() {
            state.queues.remove(session);
        }
        item
    }

    /// How many delivered items wait for `session`.
    pub fn queued(&self, session: &str) -> usize {
        self.lock().queues.get(session).map_or(0, VecDeque::len)
    }

    /// How many results `session` still expects.
    pub fn pending(&self, session: &str) -> usize {
        self.lock().pending.get(session).copied().unwrap_or(0)
    }

    /// A counter that changes whenever anything is delivered, expected or released.
    pub fn version(&self) -> u64 {
        self.lock().version
    }

    /// Wait for a change after `seen` (a value of [`Inbox::version`] read *before* inspecting
    /// the inbox, so no change between the two is missed).
    pub fn changed(&self, seen: u64) -> Changed<'_> {
        Changed { inbox: self, seen }
    }

    /// Wake every waiter without delivering anything (a message for a waiting agent, a UI
    /// refresh).
    pub fn wake(&self) {
        self.lock().bump();
    }

    /// Record that a user steering message arrived for `session` and wake any waiter (such as
    /// `wait({})`).
    pub fn notify_steer(&self, session: &str) {
        let mut state = self.lock();
        *state.steer_versions.entry(session.to_owned()).or_default() += 1;
        state.bump();
    }

    /// Monotonic counter of steering notifications recorded for `session`.
    pub fn steer_version(&self, session: &str) -> u64 {
        self.lock()
            .steer_versions
            .get(session)
            .copied()
            .unwrap_or(0)
    }
}

/// The system-authored follow-up message that carries `items` to the model.
pub fn format_inbox(items: &[InboxItem]) -> String {
    let mut out = String::from(
        "[system] Background work finished. These results are system-authored, not from the \
         user; act on them as part of your current task.",
    );
    for item in items {
        out.push_str("\n\n");
        out.push_str(&item.text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Waker;

    fn item(source: &str) -> InboxItem {
        InboxItem {
            source: source.to_owned(),
            text: format!("text {source}"),
            summary: source.to_owned(),
        }
    }

    #[test]
    fn expectation_counts_until_delivered_or_dropped() {
        let inbox = Arc::new(Inbox::default());
        let a = inbox.expect("s");
        let b = inbox.expect("s");
        assert_eq!(inbox.pending("s"), 2);
        a.deliver(item("a"));
        assert_eq!((inbox.pending("s"), inbox.queued("s")), (1, 1));
        drop(b);
        assert_eq!((inbox.pending("s"), inbox.queued("s")), (0, 1));
        assert_eq!(inbox.take("s"), [item("a")]);
        assert_eq!(inbox.queued("s"), 0);
    }

    #[test]
    fn take_source_leaves_other_items() {
        let inbox = Inbox::default();
        inbox.push("s", item("a"));
        inbox.push("s", item("b"));
        assert_eq!(inbox.take_source("s", "b"), Some(item("b")));
        assert_eq!(inbox.take_source("s", "b"), None);
        assert_eq!(inbox.take("s"), [item("a")]);
    }

    #[test]
    fn changed_resolves_after_any_change_since_the_seen_version() {
        let inbox = Arc::new(Inbox::default());
        let mut cx = Context::from_waker(Waker::noop());
        let seen = inbox.version();
        let mut fut = std::pin::pin!(inbox.changed(seen));
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        inbox.push("s", item("a"));
        assert!(fut.as_mut().poll(&mut cx).is_ready());
        // A change that happened before the future was created is not missed either.
        let mut stale = std::pin::pin!(inbox.changed(seen));
        assert!(stale.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn the_follow_up_message_carries_every_item() {
        let text = format_inbox(&[item("a"), item("b")]);
        assert!(text.starts_with("[system]"));
        assert!(text.contains("text a") && text.contains("text b"));
    }

    #[test]
    fn notify_steer_bumps_session_version_and_wakes_waiters() {
        let inbox = Arc::new(Inbox::default());
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(inbox.steer_version("s1"), 0);
        let seen = inbox.version();
        let mut fut = std::pin::pin!(inbox.changed(seen));
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        inbox.notify_steer("s1");
        assert_eq!(inbox.steer_version("s1"), 1);
        assert_eq!(inbox.steer_version("s2"), 0);
        assert!(fut.as_mut().poll(&mut cx).is_ready());
    }
}
