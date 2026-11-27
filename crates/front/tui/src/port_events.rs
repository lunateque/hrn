//! UI channel events, approval/ask one-shots, and task-completion signals shared by the TUI
//! and its backend.

use crate::git;
use crate::port_hub::AgentHubSnapshot;
use crate::port_login::LoginUiEvent;
use crate::port_theme::AppearanceConfig;
use crossterm::event::Event;
use hrn_kernel::{
    AgentEvent, ApprovalDecision, ApprovalRequest, AskRequest, BtwTopic, DagNode, SessionMeta,
    Submission,
};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// A gated tool call waiting for the user, with its one-shot answer.
pub struct ApprovalAsk {
    /// What is being asked.
    pub request: ApprovalRequest,
    reply: Box<dyn FnOnce(ApprovalDecision) + Send>,
}

impl ApprovalAsk {
    /// Pair a request with the callback that delivers the decision. Dropping the ask without
    /// answering means the turn was interrupted.
    pub fn new(
        request: ApprovalRequest,
        reply: impl FnOnce(ApprovalDecision) + Send + 'static,
    ) -> Self {
        Self {
            request,
            reply: Box::new(reply),
        }
    }

    /// Deliver the decision.
    pub fn answer(self, decision: ApprovalDecision) {
        (self.reply)(decision);
    }
}

impl fmt::Debug for ApprovalAsk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovalAsk")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

/// A form from the `ask` tool, with its one-shot answer callback.
pub struct AskPrompt {
    /// The form to present.
    pub request: AskRequest,
    reply: Box<dyn FnOnce(Option<Submission>) + Send>,
}

impl AskPrompt {
    /// Pair an `AskRequest` with the callback that delivers `Some(submission)` or `None` on cancel.
    pub fn new(
        request: AskRequest,
        reply: impl FnOnce(Option<Submission>) + Send + 'static,
    ) -> Self {
        Self {
            request,
            reply: Box::new(reply),
        }
    }

    /// Deliver the submitted form (`None` when the user pressed `Esc`).
    pub fn answer(self, submission: Option<Submission>) {
        (self.reply)(submission);
    }
}

impl fmt::Debug for AskPrompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AskPrompt")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

/// How a turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Finished normally.
    Done,
    /// The user interrupted it.
    Interrupted,
    /// It failed with this message.
    Failed(String),
}

/// A background task finished; the backend is idle again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskEnd {
    /// A turn, including any follow-up turn for late steering, ended.
    Turn {
        /// How it ended.
        outcome: TurnOutcome,
        /// Steering text that never reached the model; the TUI puts it back in the input.
        unsent_steers: Vec<String>,
    },
    /// `/compact` ended: `Ok(true)` folded history, `Ok(false)` found nothing to fold.
    Compact(std::result::Result<bool, String>),
    /// `/handoff` ended: `Ok(true)` committed a handoff summary, `Ok(false)` was empty/discarded.
    Handoff(std::result::Result<bool, String>),
}

/// Messages from the backend to the TUI.
#[derive(Debug)]
pub enum UiEvent {
    /// A live runtime event.
    Agent(AgentEvent),
    /// A tool call needs the user's decision.
    Approval(ApprovalAsk),
    /// The `ask` tool needs structured answers from the user.
    Ask(AskPrompt),
    /// The background task ended.
    Finished(TaskEnd),
    /// The background work of the current session that is still running (agents, `bash` jobs
    /// and services, by id), or results waiting to be delivered: the UI refreshes its indicator
    /// and delivers when idle.
    Running(Vec<String>),
    /// Text for the transcript that arrived later than the command asking for it (`/usage`).
    Report(String),
    /// Streamed content chunk for an active `/btw` side question.
    BtwDelta {
        /// Topic identifier receiving the delta.
        topic_id: String,
        /// Text chunk appended to the active exchange.
        delta: String,
    },
    /// Active `/btw` side question finished or cancelled (persisted topic snapshot).
    BtwDone(BtwTopic),
    /// Updated live Agent Hub snapshot (subagent roster, tool previews, and activity log).
    AgentHub(AgentHubSnapshot),
    /// In-TUI `/login` progress, authorization URL, probe failure, or completion event.
    Login(LoginUiEvent),
    /// Live appearance reload (`[tui]` / `[tui.theme]` / `[tui.statusline]`) from config watcher.
    Appearance(Box<AppearanceConfig>),
    /// Active custom theme file reloaded on disk (`Ok(name)` or `Err(msg)` keeping last-good).
    ThemeReloaded(std::result::Result<String, String>),
    /// Re-read and merge the profile `keybindings.toml` layers on disk.
    KeymapReload,
    /// Active session title updated asynchronously (auto-title or bare `/rename` regeneration).
    SessionTitle {
        /// Session id whose title changed (ignored if the user switched sessions).
        session_id: String,
        /// New display title.
        title: String,
    },
    /// A collaboration guest received the full initial snapshot and loaded its replica session.
    SessionLoaded {
        /// Loaded session metadata.
        meta: Box<SessionMeta>,
        /// Chronological branch nodes.
        nodes: Vec<DagNode>,
        /// Optional informational notice printed after displaying the session.
        notice: Option<String>,
    },
    /// An authorized interactive collaboration guest submitted a prompt on the host.
    CollabPrompt {
        /// Name-badged prompt text (`[guest:<name>] ...`).
        badged_text: String,
    },
    /// An authorized interactive collaboration guest requested aborting the host's active turn.
    CollabAbort {
        /// Sanitized guest display name.
        guest_name: String,
    },
    /// A paired mobile remote control client submitted a new prompt.
    RemotePrompt {
        /// Prompt text submitted from the remote client.
        text: String,
    },
    /// A paired mobile remote control client steered the active turn.
    RemoteSteer {
        /// Steering text submitted from the remote client.
        text: String,
    },
    /// A paired mobile remote control client requested interrupting the active turn.
    RemoteAbort,
    /// A pending tool approval was already answered from a remote client; dismiss it locally.
    RemoteApprovalResolved {
        /// Tool call identifier of the resolved approval.
        call_id: String,
    },
    /// A pending `ask` form was already submitted from a remote client; dismiss it locally.
    RemoteAskResolved {
        /// Identifier of the resolved `ask` request.
        id: String,
    },
    /// An asynchronous `/remote` or `/collab` tunnel startup completed; poll `finish_remote`.
    RemoteReady,
    /// Background `/loop --while|--until` condition command completed.
    LoopCondition {
        /// Monotonic check id matching the active loop condition probe.
        check_id: u64,
        /// Process exit code (`Some(0)` means condition succeeded).
        exit_code: Option<i32>,
        /// True when the condition command exceeded its wall-clock timeout.
        timed_out: bool,
        /// Error message when spawning or running the command failed.
        error: Option<String>,
    },
}

/// One message on the UI channel: terminal input batches, backend events and the TUI's own
/// background probes share it, so the event loop sleeps on a single receiver.
#[derive(Debug)]
pub struct Inbound(pub(crate) Message);

impl Inbound {
    /// The backend event this message carries; `None` for the TUI's own messages.
    pub fn into_event(self) -> Option<UiEvent> {
        match self.0 {
            Message::Backend(event) => Some(event),
            Message::Terminal(_) | Message::Git(..) => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum Message {
    Terminal(Vec<Event>),
    Backend(UiEvent),
    /// The working-tree counts of a directory (`None`: not a repository).
    Git(PathBuf, Option<git::Status>),
}

type WakeHook = Arc<dyn Fn() + Send + Sync>;
type WakeSlot = Arc<Mutex<Option<WakeHook>>>;

/// The backend's end of the UI channel.
#[derive(Clone)]
pub struct UiSender {
    tx: mpsc::Sender<Inbound>,
    wake: WakeSlot,
}

impl fmt::Debug for UiSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UiSender").finish_non_exhaustive()
    }
}

impl UiSender {
    fn notify_waker(&self) {
        let hook = self
            .wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(wake) = hook {
            wake();
        }
    }

    /// Deliver an event; `false` once the TUI is gone.
    pub fn send(&self, event: UiEvent) -> bool {
        let ok = self.tx.send(Inbound(Message::Backend(event))).is_ok();
        if ok {
            self.notify_waker();
        }
        ok
    }

    pub(crate) fn send_terminal(&self, events: Vec<Event>) -> bool {
        let ok = self.tx.send(Inbound(Message::Terminal(events))).is_ok();
        if ok {
            self.notify_waker();
        }
        ok
    }

    pub(crate) fn send_git(&self, cwd: PathBuf, status: Option<git::Status>) -> bool {
        let ok = self.tx.send(Inbound(Message::Git(cwd, status))).is_ok();
        if ok {
            self.notify_waker();
        }
        ok
    }
}

/// The TUI's end of the UI channel.
pub struct UiReceiver {
    rx: mpsc::Receiver<Inbound>,
    tx: mpsc::Sender<Inbound>,
    wake: WakeSlot,
}

impl fmt::Debug for UiReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UiReceiver").finish_non_exhaustive()
    }
}

impl UiReceiver {
    pub(crate) fn sender(&self) -> UiSender {
        UiSender {
            tx: self.tx.clone(),
            wake: Arc::clone(&self.wake),
        }
    }

    /// Wait for the next message, at most `timeout` (`None`: indefinitely).
    pub fn recv(&self, timeout: Option<Duration>) -> Option<Inbound> {
        match timeout {
            Some(timeout) => self.rx.recv_timeout(timeout).ok(),
            None => self.rx.recv().ok(),
        }
    }

    /// The next message if one is already waiting.
    pub fn try_recv(&self) -> Option<Inbound> {
        self.rx.try_recv().ok()
    }
}

/// A connected UI channel.
pub fn channel() -> (UiSender, UiReceiver) {
    let (tx, rx) = mpsc::channel();
    let wake: WakeSlot = Arc::default();
    (
        UiSender {
            tx: tx.clone(),
            wake: Arc::clone(&wake),
        },
        UiReceiver { rx, tx, wake },
    )
}
