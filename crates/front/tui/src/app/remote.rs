//! `/remote` QR pairing overlay actions and live remote supervision handlers on [`App`].

use super::{App, Task};
use crate::port::RemoteCommandOutcome;
use crate::remote_qr::{RemoteQrAction, RemoteQrOverlay};
use crate::transcript::{Entry, Level, SteerState};
use crossterm::event::KeyEvent;

impl App {
    pub(super) fn handle_remote(&mut self, raw: Option<&str>) {
        let arg = raw.map(str::trim).filter(|s| !s.is_empty());
        let outcome = self.backend.remote_command(arg);
        self.apply_remote_outcome(outcome);
    }

    pub(super) fn on_remote_ready(&mut self) {
        if let Some(outcome) = self.backend.finish_remote() {
            self.apply_remote_outcome(outcome);
        }
    }

    fn apply_remote_outcome(&mut self, outcome: hrn_kernel::Result<RemoteCommandOutcome>) {
        match outcome {
            Ok(RemoteCommandOutcome::Open(view)) => {
                self.remote_qr = Some(RemoteQrOverlay::new(view));
            }
            Ok(RemoteCommandOutcome::Notice(msg)) => {
                self.notice(Level::Info, msg);
            }
            Err(err) => {
                self.notice(Level::Error, err.to_string());
            }
        }
    }

    pub(super) fn remote_qr_key(&mut self, key: KeyEvent) -> bool {
        let Some(overlay) = &mut self.remote_qr else {
            return false;
        };
        match overlay.key(key) {
            RemoteQrAction::Stay => {}
            RemoteQrAction::Close => {
                self.remote_qr = None;
                self.flash("remote running in background · /remote to show QR · /remote stop");
            }
            RemoteQrAction::Rotate(control) => match self.backend.remote_rotate(control) {
                Ok(view) => {
                    if let Some(qr) = &mut self.remote_qr {
                        qr.set_view(view);
                    }
                    let label = if control { "control" } else { "view-only" };
                    self.flash(format!("fresh {label} QR minted"));
                }
                Err(err) => {
                    self.remote_qr = None;
                    self.notice(Level::Error, err.to_string());
                }
            },
            RemoteQrAction::Stop => {
                self.remote_qr = None;
                match self.backend.remote_stop() {
                    Ok(msg) => self.notice(Level::Info, msg),
                    Err(err) => self.notice(Level::Error, err.to_string()),
                }
            }
        }
        true
    }

    /// Dispatch a prompt sent from a paired phone/browser without resolving host `@path` images
    /// or draining the desktop user's staged image tray.
    pub(super) fn on_remote_prompt(&mut self, text: &str) {
        self.dispatch_prompt(text, Some(format!("[remote] {text}")), false);
    }

    /// Steer the running turn from a paired phone/browser, or start a new turn if idle.
    pub(super) fn on_remote_steer(&mut self, text: &str) {
        if matches!(self.task, Task::Turn { .. }) {
            self.backend.steer(text);
            self.transcript.push(Entry::Steer {
                text: format!("[remote] {text}"),
                state: SteerState::Queued,
            });
        } else {
            self.on_remote_prompt(text);
        }
    }

    /// Abort the active turn on behalf of a paired phone/browser control client.
    pub(super) fn on_remote_abort(&mut self) {
        self.notice(Level::Warn, "[remote] requested turn abort");
        self.interrupt();
    }

    /// Dismiss a locally queued approval dock when a remote device already answered it.
    pub(super) fn on_remote_approval_resolved(&mut self, call_id: &str) {
        let Some(pos) = self
            .approvals
            .iter()
            .position(|ask| ask.request.id == call_id)
        else {
            return;
        };
        let was_front = pos == 0;
        let _ = self.approvals.remove(pos);
        if self.approvals.is_empty() {
            self.approval_dock = None;
        } else if was_front {
            self.open_front_dock();
        }
    }

    /// Dismiss a locally queued or active `ask` form when a remote device already submitted it.
    pub(super) fn on_remote_ask_resolved(&mut self, id: &str) {
        if self
            .active_ask
            .as_ref()
            .is_some_and(|(_, prompt)| prompt.request.id == id)
        {
            self.active_ask = None;
            self.advance_ask();
            return;
        }
        self.ask_queue.retain(|prompt| prompt.request.id != id);
    }
}
