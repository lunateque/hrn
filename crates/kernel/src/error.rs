//! Error types and cooperative cancellation flag for `hrn`.

use std::fmt;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Unified error type across the `hrn` workspace.
#[derive(Debug)]
pub enum HrnError {
    /// Operation was cooperatively cancelled.
    Cancelled,
    /// Invalid configuration or policy violation (e.g. `guard.rs` rejection).
    Config(String),
    /// Authentication / OAuth error.
    Auth(String),
    /// Upstream LLM or HTTP transport error.
    Provider {
        /// Human-readable message.
        message: String,
        /// Whether this error is eligible for retry or fallback before first token.
        retryable: bool,
        /// Optional `Retry-After` hint in seconds.
        retry_after_secs: Option<u64>,
    },
    /// Tool execution or validation failure.
    Tool(String),
    /// Persistence / `SQLite` error.
    Store(String),
    /// Local filesystem or process I/O error.
    Io(String),
    /// Maximum agent loop steps reached.
    MaxSteps(usize),
    /// Generic internal failure.
    Internal(String),
}

impl HrnError {
    /// Short stable error code for wire events and logs.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Config(_) => "config",
            Self::Auth(_) => "auth",
            Self::Provider { .. } => "provider",
            Self::Tool(_) => "tool",
            Self::Store(_) => "store",
            Self::Io(_) => "io",
            Self::MaxSteps(_) => "max_steps",
            Self::Internal(_) => "internal",
        }
    }

    /// Whether this error is retryable before the first streamed token.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Provider {
                retryable: true,
                ..
            }
        )
    }

    /// Optional `Retry-After` cooldown hint in seconds.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::Provider {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }
}

impl fmt::Display for HrnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled"),
            Self::Config(msg) => write!(f, "config error: {msg}"),
            Self::Auth(msg) => write!(f, "auth error: {msg}"),
            Self::Provider { message, .. } => write!(f, "provider error: {message}"),
            Self::Tool(msg) => write!(f, "tool error: {msg}"),
            Self::Store(msg) => write!(f, "store error: {msg}"),
            Self::Io(msg) => write!(f, "io error: {msg}"),
            Self::MaxSteps(n) => write!(f, "max steps exceeded ({n})"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for HrnError {}

impl From<std::io::Error> for HrnError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<serde_json::Error> for HrnError {
    fn from(err: serde_json::Error) -> Self {
        Self::Internal(format!("json error: {err}"))
    }
}

/// Convenience result alias used across the workspace.
pub type Result<T = ()> = std::result::Result<T, HrnError>;

#[derive(Debug, Default)]
struct CancelInner {
    cancelled: AtomicBool,
    parent: Option<Arc<CancelInner>>,
    /// Tasks parked in [`CancelFlag::cancelled`], woken by `cancel`.
    wakers: Mutex<Vec<Waker>>,
}

impl CancelInner {
    fn register(&self, waker: &Waker) {
        let mut wakers = self
            .wakers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !wakers.iter().any(|w| w.will_wake(waker)) {
            wakers.push(waker.clone());
        }
    }
}

/// Cooperative lock-free cancellation flag shared across an agent run and its children.
#[derive(Clone, Debug, Default)]
pub struct CancelFlag(Arc<CancelInner>);

impl CancelFlag {
    /// Create a fresh uncancelled flag.
    pub fn new() -> Self {
        Self(Arc::new(CancelInner::default()))
    }

    /// Create a child flag that observes parent cancellation without cancelling the parent.
    pub fn child(&self) -> Self {
        Self(Arc::new(CancelInner {
            cancelled: AtomicBool::new(false),
            parent: Some(Arc::clone(&self.0)),
            wakers: Mutex::default(),
        }))
    }

    /// Signal cancellation on this flag (and any children derived from it).
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        let wakers = std::mem::take(
            &mut *self
                .0
                .wakers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for waker in wakers {
            waker.wake();
        }
    }

    /// A future that resolves once this flag or any ancestor is cancelled.
    pub fn cancelled(&self) -> Cancelled {
        Cancelled(self.clone())
    }

    /// Race `fut` against cancellation, returning [`HrnError::Cancelled`] immediately once
    /// this flag or any ancestor cancels.
    pub fn race<'a, T, F>(&self, fut: F) -> RaceCancel<'a, T>
    where
        F: Future<Output = Result<T>> + Send + 'a,
    {
        RaceCancel {
            cancel: self.cancelled(),
            fut: Box::pin(fut),
        }
    }

    /// Check whether cancellation has been requested on this flag or any ancestor.
    pub fn is_cancelled(&self) -> bool {
        let mut cur: Option<&CancelInner> = Some(&self.0);
        while let Some(node) = cur {
            if node.cancelled.load(Ordering::SeqCst) {
                return true;
            }
            cur = node.parent.as_deref();
        }
        false
    }
}

/// Future returned by [`CancelFlag::cancelled`]; holds no resources, so it may be dropped freely.
pub struct Cancelled(CancelFlag);

impl Future for Cancelled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Register on the whole ancestor chain before checking, so a cancel racing with this
        // poll either wakes the task or is seen by the check.
        let mut cur: Option<&CancelInner> = Some(&(self.0).0);
        while let Some(node) = cur {
            node.register(cx.waker());
            cur = node.parent.as_deref();
        }
        if self.0.is_cancelled() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// Future returned by [`CancelFlag::race`]; biased toward immediate cancellation.
pub struct RaceCancel<'a, T> {
    cancel: Cancelled,
    fut: Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>,
}

impl<T> Future for RaceCancel<'_, T> {
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if Pin::new(&mut self.cancel).poll(cx).is_ready() {
            return Poll::Ready(Err(HrnError::Cancelled));
        }
        self.fut.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn cancelling_a_parent_wakes_a_waiter_on_its_child() {
        let parent = CancelFlag::new();
        let child = parent.child();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);
        let mut fut = child.cancelled();
        assert!(Pin::new(&mut fut).poll(&mut cx).is_pending());
        parent.cancel();
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert!(Pin::new(&mut fut).poll(&mut cx).is_ready());
    }
}
