//! Background idle-session compaction drivers for `hrn chat` and `hrn rpc`/`acp`.
//!
//! When `[compaction.idle] enabled = true`, an idle session whose inactivity exceeds
//! `effective_timeout_secs()` (>= 60 s) and whose projected context exceeds `threshold_tokens`
//! triggers `AgentRuntime::compact_idle` with `auto_continue = false`.

use crate::chat::chat_ui::UiEvents;
use hrn_harness::{AgentRuntime, SessionControl};
use hrn_kernel::{AgentEvent, BoxFuture, CancelFlag, EventSink};
use hrn_tui::UiSender;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::runtime::Handle;

/// Poll interval for checking idle compaction eligibility.
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(5);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shared state connecting `ChatBackend` to its background idle-compaction timer.
#[derive(Clone)]
pub(crate) struct ChatIdleHandle {
    runtime: Arc<Mutex<AgentRuntime>>,
    session: Arc<Mutex<Option<String>>>,
    busy: Arc<AtomicBool>,
    last_activity_ms: Arc<AtomicI64>,
}

impl ChatIdleHandle {
    /// Construct a fresh idle tracker over the initial runtime and shared session id slot.
    pub(crate) fn new(runtime: AgentRuntime, session: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            runtime: Arc::new(Mutex::new(runtime)),
            session,
            busy: Arc::new(AtomicBool::new(false)),
            last_activity_ms: Arc::new(AtomicI64::new(now_ms())),
        }
    }

    /// Update the live runtime after hot-reload, login, role change, or preset switch.
    pub(crate) fn sync_runtime(&self, runtime: &AgentRuntime) {
        *lock(&self.runtime) = runtime.clone();
    }

    /// Record user/turn activity to reset the idle countdown.
    pub(crate) fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::SeqCst);
    }

    /// Mark whether a turn or compaction task is currently active.
    pub(crate) fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::SeqCst);
        self.touch();
    }

    fn idle_secs(&self) -> u64 {
        let elapsed_ms = now_ms().saturating_sub(self.last_activity_ms.load(Ordering::SeqCst));
        u64::try_from(elapsed_ms.max(0) / 1000).unwrap_or(0)
    }
}

/// Spawn the background idle compaction loop for `hrn chat` (never triggers `auto_continue`).
pub(crate) fn watch_chat(handle: &Handle, idle: ChatIdleHandle, tx: UiSender) {
    handle.spawn(async move {
        let events: Arc<dyn EventSink> = Arc::new(UiEvents::tui_only(tx));
        loop {
            tokio::time::sleep(IDLE_POLL_INTERVAL).await;
            if idle.busy.load(Ordering::SeqCst) {
                continue;
            }
            let Some(session_id) = lock(&idle.session).clone() else {
                continue;
            };
            let runtime = lock(&idle.runtime).clone();
            let cfg = runtime.compaction_config();
            if !cfg.enabled || !cfg.idle.enabled {
                continue;
            }
            let elapsed = idle.idle_secs();
            if elapsed < cfg.idle.effective_timeout_secs() {
                continue;
            }
            if idle
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                continue;
            }
            let cancel = CancelFlag::new();
            let outcome = runtime
                .compact_idle(&session_id, elapsed, &cancel, &events)
                .await;
            idle.set_busy(false);
            if let Err(err) = outcome {
                crate::term::err_line(&format!("idle compaction failed: {err}"));
            }
        }
    });
}

/// Shared idle activity tracker for `hrn rpc` / `hrn acp` sessions.
#[derive(Clone, Default)]
pub(crate) struct RpcIdleTracker {
    sessions: Arc<Mutex<BTreeMap<String, (SessionControl, i64)>>>,
}

impl RpcIdleTracker {
    /// Record activity for `session_id` alongside its live [`SessionControl`].
    pub(crate) fn touch(&self, session_id: &str, control: SessionControl) {
        lock(&self.sessions).insert(session_id.to_owned(), (control, now_ms()));
    }

    fn snapshot(&self) -> Vec<(String, SessionControl, u64)> {
        let now = now_ms();
        lock(&self.sessions)
            .iter()
            .map(|(id, (ctrl, ts))| {
                let secs = u64::try_from(now.saturating_sub(*ts).max(0) / 1000).unwrap_or(0);
                (id.clone(), ctrl.clone(), secs)
            })
            .collect()
    }
}

struct DiscardSink;
impl EventSink for DiscardSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

/// Spawn the background idle compaction loop for `hrn rpc` / `hrn acp` (always `auto_continue = false`).
pub(crate) fn watch_rpc(
    handle: &Handle,
    runtime_slot: Arc<Mutex<AgentRuntime>>,
    tracker: RpcIdleTracker,
) {
    handle.spawn(async move {
        let events: Arc<dyn EventSink> = Arc::new(DiscardSink);
        loop {
            tokio::time::sleep(IDLE_POLL_INTERVAL).await;
            let runtime = lock(&runtime_slot).clone();
            let cfg = runtime.compaction_config();
            if !cfg.enabled || !cfg.idle.enabled {
                continue;
            }
            let min_secs = cfg.idle.effective_timeout_secs();
            for (session_id, control, idle_secs) in tracker.snapshot() {
                if control.is_running() || idle_secs < min_secs {
                    continue;
                }
                let cancel = CancelFlag::new();
                let outcome = runtime
                    .compact_idle(&session_id, idle_secs, &cancel, &events)
                    .await;
                tracker.touch(&session_id, control);
                if let Err(err) = outcome {
                    crate::term::err_line(&format!("rpc idle compaction failed: {err}"));
                }
            }
        }
    });
}
