//! What a session knows about its targets, kept current by the adapters' events.

use crate::client::DapClient;
use hrn_kernel::{Clock, SystemClock};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::watch;

/// How much program output a session keeps (the newest part).
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// Where a target is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Started; the adapter has not said it is ready.
    Launching,
    /// The adapter waits for `configurationDone`.
    Configuring,
    /// The program runs.
    Running,
    /// The program is stopped (breakpoint, step, pause).
    Stopped,
    /// The program ended or the adapter is gone.
    Terminated,
}

impl Status {
    /// Lowercase label for tool output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Launching => "launching",
            Self::Configuring => "configuring",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Terminated => "terminated",
        }
    }
}

/// Where and why the program last stopped.
#[derive(Clone, Debug, Default)]
pub struct Stop {
    /// Thread the stop or resume acts on.
    pub thread_id: Option<i64>,
    /// Why the program stopped (`entry`, `breakpoint`, `pause`, …).
    pub reason: Option<String>,
    /// Id of the top frame, once read.
    pub frame_id: Option<i64>,
    /// Name of the top frame.
    pub frame_name: Option<String>,
    /// Instruction pointer reference of the top frame.
    pub instruction_pointer: Option<String>,
    /// Source file of the top frame.
    pub source_path: Option<String>,
    /// Line of the top frame.
    pub line: Option<i64>,
    /// Column of the top frame.
    pub column: Option<i64>,
}

/// One DAP connection of a session: the root, or a child the adapter asked for.
pub struct Target {
    /// Target id: the session id, or `<session>.<n>` for a child.
    pub id: String,
    /// The target that asked for this one.
    pub parent: Option<String>,
    /// Its adapter connection, until the session ends.
    pub client: Option<Arc<DapClient>>,
    /// Where this target is in its life.
    pub status: Status,
    /// The adapter's capabilities from `initialize`.
    pub caps: Value,
    /// Where and why it last stopped.
    pub stop: Stop,
    /// Threads as last reported.
    pub threads: Vec<Value>,
    /// Program exit code, once reported.
    pub exit_code: Option<i64>,
    /// The adapter sent `initialized`.
    pub initialized: bool,
    /// Its capabilities ask for `configurationDone`.
    pub needs_cfg: bool,
    /// `configurationDone` has been sent.
    pub cfg_sent: bool,
    /// Launch target, for display.
    pub program: Option<String>,
    /// Working directory of the debuggee.
    pub cwd: PathBuf,
}

impl Target {
    /// A target that has not been handed a connection yet.
    pub fn new(id: String, parent: Option<String>, program: Option<String>, cwd: PathBuf) -> Self {
        Self {
            id,
            parent,
            client: None,
            status: Status::Launching,
            caps: Value::Null,
            stop: Stop::default(),
            threads: Vec::new(),
            exit_code: None,
            initialized: false,
            needs_cfg: false,
            cfg_sent: false,
            program,
            cwd,
        }
    }

    /// Whether the adapter's capabilities say `capability` is true.
    pub fn supports(&self, capability: &str) -> bool {
        self.caps.get(capability).and_then(Value::as_bool) == Some(true)
    }
}

/// The newest 128 KiB of everything the program and adapter printed.
#[derive(Default)]
pub struct Output {
    text: String,
}

impl Output {
    /// Append program or adapter output, dropping the oldest bytes past the cap.
    pub fn push(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        if self.text.len() > MAX_OUTPUT_BYTES {
            let cut = self
                .text
                .ceil_char_boundary(self.text.len() - MAX_OUTPUT_BYTES);
            self.text.drain(..cut);
        }
    }

    /// What is buffered.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Everything events change.
#[derive(Default)]
pub struct State {
    /// Every target, root first.
    pub targets: Vec<Target>,
    /// The target the model's next request reaches.
    pub active: Option<String>,
    /// What the program and adapter printed.
    pub output: Output,
    /// When every target ended (ms since epoch); `0` while one lives.
    pub finished_ms: i64,
}

impl State {
    /// The target `id`.
    pub fn target(&self, id: &str) -> Option<&Target> {
        self.targets.iter().find(|t| t.id == id)
    }

    /// The target `id`, mutably.
    pub fn target_mut(&mut self, id: &str) -> Option<&mut Target> {
        self.targets.iter_mut().find(|t| t.id == id)
    }

    /// The focused target.
    pub fn active_target(&self) -> Option<&Target> {
        self.active.as_deref().and_then(|id| self.target(id))
    }

    /// Whether any target is stopped.
    pub fn any_stopped(&self) -> bool {
        self.targets.iter().any(|t| t.status == Status::Stopped)
    }

    /// Whether every target ended (and there is at least one).
    pub fn all_terminated(&self) -> bool {
        !self.targets.is_empty() && self.targets.iter().all(|t| t.status == Status::Terminated)
    }

    /// After a target ended: focus a live target, preferring a stopped one.
    fn refocus(&mut self) {
        if self
            .active_target()
            .is_some_and(|t| t.status == Status::Terminated)
        {
            let next = self
                .targets
                .iter()
                .filter(|t| t.status != Status::Terminated)
                .min_by_key(|t| t.status != Status::Stopped)
                .map(|t| t.id.clone());
            if let Some(next) = next {
                self.active = Some(next);
            }
        }
        if self.all_terminated() && self.finished_ms == 0 {
            self.finished_ms = SystemClock.now_ms();
        }
    }

    /// Mark `id` ended and move focus to a live target.
    pub fn end(&mut self, id: &str) {
        if let Some(t) = self.target_mut(id) {
            t.status = Status::Terminated;
        }
        self.refocus();
    }

    /// Apply one adapter event to target `id`.
    pub fn on_event(&mut self, id: &str, name: &str, body: &Value) {
        let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
        match name {
            "output" => {
                if body.get("category").and_then(Value::as_str) != Some("telemetry")
                    && let Some(out) = body.get("output").and_then(Value::as_str)
                {
                    self.output.push(out);
                }
            }
            "initialized" => {
                if let Some(t) = self.target_mut(id) {
                    t.initialized = true;
                    if !t.cfg_sent && t.status == Status::Launching {
                        t.status = Status::Configuring;
                    }
                }
            }
            "stopped" => {
                if let Some(t) = self.target_mut(id) {
                    t.status = Status::Stopped;
                    t.stop = Stop {
                        thread_id: body.get("threadId").and_then(Value::as_i64),
                        reason: text("reason").map(|r| match text("description") {
                            Some(d) if d != r => format!("{r} ({d})"),
                            _ => r,
                        }),
                        ..Stop::default()
                    };
                }
                self.active = Some(id.to_owned());
            }
            "continued" => {
                if let Some(t) = self.target_mut(id) {
                    t.status = Status::Running;
                    t.stop = Stop {
                        thread_id: body.get("threadId").and_then(Value::as_i64),
                        ..Stop::default()
                    };
                }
            }
            "exited" => {
                if let Some(t) = self.target_mut(id) {
                    t.exit_code = body.get("exitCode").and_then(Value::as_i64);
                }
                self.end(id);
            }
            "terminated" => self.end(id),
            _ => {}
        }
    }
}

/// The state behind a lock, with a change counter waiters can sleep on.
pub struct Shared {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    /// No targets and no output yet.
    pub fn new() -> Self {
        Self {
            state: Mutex::default(),
            changed: watch::channel(0).0,
        }
    }

    /// Mutate the state and wake every waiter.
    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let out = f(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner));
        self.changed.send_modify(|n| *n += 1);
        out
    }

    /// Read the state.
    /// Inspect the state.
    pub fn read<R>(&self, f: impl FnOnce(&State) -> R) -> R {
        f(&self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Wait until `f` yields a value; `None` when `timeout` passes first.
    pub async fn wait_for<R>(
        &self,
        timeout: Duration,
        f: impl Fn(&State) -> Option<R>,
    ) -> Option<R> {
        // Subscribing first means a change between the check and the sleep is not lost.
        let mut rx = self.changed.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(found) = self.read(&f) {
                return Some(found);
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return self.read(&f),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> State {
        let mut s = State::default();
        for id in ["a", "b"] {
            s.targets
                .push(Target::new(id.into(), None, None, PathBuf::new()));
        }
        s.active = Some("a".into());
        s
    }

    #[test]
    fn a_stop_moves_focus_and_the_end_of_the_focused_target_hands_it_to_a_live_one() {
        let mut s = state();
        s.on_event(
            "b",
            "stopped",
            &json!({"threadId": 7, "reason": "breakpoint"}),
        );
        assert_eq!(s.active.as_deref(), Some("b"));
        assert_eq!(s.target("b").and_then(|t| t.stop.thread_id), Some(7));
        s.on_event("b", "terminated", &json!({}));
        assert_eq!(s.active.as_deref(), Some("a"));
        assert!(!s.all_terminated());
        s.on_event("a", "exited", &json!({"exitCode": 3}));
        assert!(s.all_terminated());
        assert_eq!(s.target("a").and_then(|t| t.exit_code), Some(3));
    }

    #[test]
    fn output_keeps_the_newest_bytes() {
        let mut o = Output::default();
        o.push(&"x".repeat(MAX_OUTPUT_BYTES));
        o.push("tail");
        assert_eq!(o.text().len(), MAX_OUTPUT_BYTES);
        assert!(o.text().ends_with("tail"));
    }

    #[tokio::test]
    async fn waiters_wake_on_a_change_made_after_they_subscribed() {
        let shared = Arc::new(Shared::new());
        shared.with(|s| {
            s.targets
                .push(Target::new("a".into(), None, None, PathBuf::new()));
        });
        let waiter = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                shared
                    .wait_for(Duration::from_secs(5), |s| s.any_stopped().then_some(()))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        shared.with(|s| s.on_event("a", "stopped", &json!({"reason": "step"})));
        assert_eq!(waiter.await.expect("join"), Some(()));
    }
}
