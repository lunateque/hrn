//! Out-of-band notices: when a turn ends or the agent needs input while the window is not
//! focused (or a running turn grew older than `notify_after_secs`), ring the terminal bell and
//! try the desktop-notification escapes the hosts understand: OSC 9 (Windows Terminal,
//! `Kitty`, `WezTerm`, `iTerm2`), OSC 777 `notify;title;body` (`urxvt`, `Ghostty`, `Warp`),
//! and the terminal bell — and the window title (OSC 0) stays in step with the agent's state.

use crate::port::Notify;
use std::io::Write;
use std::time::Duration;

/// What the agent is doing, as the title and the notice decide it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Idle,
    Working,
    NeedsInput,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "hrn",
            Self::Working => "hrn · working…",
            Self::NeedsInput => "hrn · waiting for input",
        }
    }

    fn toast(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Idle => Some(("Task finished", "hrn is waiting for your next prompt")),
            Self::Working => None,
            Self::NeedsInput => Some(("Waiting for input", "hrn needs your answer")),
        }
    }
}

/// The notifier: focus is tracked by the app (terminal focus events), delivery is immediate
/// and best-effort. Writes never emit newlines so they cannot move the cursor underneath the
/// inline renderer.
pub(crate) struct Notifier {
    config: Notify,
    focused: bool,
    title: Option<String>,
    /// The long-turn warning fired for the current turn (at most once per turn).
    warned: bool,
}

impl Notifier {
    pub(crate) fn new(config: Notify) -> Self {
        Self {
            config,
            focused: true,
            title: None,
            warned: false,
        }
    }

    pub(crate) fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Keep the terminal window title at `state` (plus the session title when there is one).
    pub(crate) fn title(&mut self, state: State, session: Option<&str>) {
        let wanted = match session.map(str::trim).filter(|s| !s.is_empty()) {
            Some(title) => format!("{} · {}", state.label(), crate::toolfmt::one_line(title)),
            None => state.label().to_owned(),
        };
        if self.title.as_deref() == Some(wanted.as_str()) {
            return;
        }
        self.title = Some(wanted.clone());
        Self::write_osc(0, &wanted);
    }

    /// Announce a state change (input needed) when the window lost focus.
    pub(crate) fn announce(&mut self, state: State) {
        if !self.config.enabled || self.focused {
            return;
        }
        self.deliver(state);
    }

    /// A turn just ended or settled into needing input: notice when the window is unfocused,
    /// or when it took at least `after` (the timer covers hosts that report no focus events).
    pub(crate) fn finished(&mut self, state: State, elapsed: Duration) {
        if !self.config.enabled {
            return;
        }
        if !self.focused {
            self.deliver(state);
            return;
        }
        if !self.config.after.is_zero() && elapsed >= self.config.after {
            self.warned = true;
            self.deliver(state);
        }
    }

    /// A still-running turn older than `after` warns once, focused or not.
    pub(crate) fn long_turn(&mut self, elapsed_secs: u64, state: State) {
        if self.warned
            || !self.config.enabled
            || state != State::Working
            || self.config.after.is_zero()
            || elapsed_secs < self.config.after.as_secs()
        {
            return;
        }
        self.warned = true;
        if self.focused {
            Self::deliver_toast(&self.config.title, "is still running the current turn");
        } else {
            self.deliver(state);
        }
    }

    /// Clear the once-per-turn warning when a new turn starts or ends.
    pub(crate) fn reset(&mut self) {
        self.warned = false;
    }

    fn deliver(&self, state: State) {
        if let Some((kind, body)) = state.toast() {
            Self::deliver_toast(&format!("{} · {}", self.config.title, kind), body);
        }
    }

    fn deliver_toast(title: &str, body: &str) {
        Self::write_osc(9, &format!("{title};{body}"));
        Self::write_osc(777, &format!("notify;{title};{body}"));
        Self::write_bytes(b"\x07");
    }

    /// `ESC ] <code> ; <text> BEL` — no trailing newline, so the cursor never moves.
    fn write_osc(code: u16, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        let seq = format!("\x1b]{code};{clean}\x07");
        Self::write_bytes(seq.as_bytes());
    }

    fn write_bytes(bytes: &[u8]) {
        let mut out = std::io::stdout();
        let _ = out.write_all(bytes).and_then(|()| out.flush());
    }
}
