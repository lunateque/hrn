//! `/share`, `/collab`, `/join`, and `/leave` slash command handlers on [`App`].

use super::{App, Task};
use crate::transcript::{Entry, Level, SteerState};

impl App {
    pub(super) fn handle_share(&mut self, raw: Option<&str>) {
        let arg = raw.map(str::trim).unwrap_or_default();
        let no_redact = match arg {
            "" => false,
            "--no-redact" => true,
            other => {
                self.notice(
                    Level::Warn,
                    format!("unknown /share flag `{other}` · usage: /share [--no-redact]"),
                );
                return;
            }
        };
        match self.backend.share_session(no_redact) {
            Ok(msg) => self.notice(Level::Info, msg),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    pub(super) fn handle_collab(&mut self, raw: Option<&str>) {
        let arg = raw.map(str::trim).filter(|s| !s.is_empty());
        match self.backend.collab_command(arg) {
            Ok(msg) => self.notice(Level::Info, msg),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    pub(super) fn handle_join(&mut self, raw: Option<&str>) {
        let Some(link) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            self.notice(Level::Warn, "usage: /join <collab-link>");
            return;
        };
        if self.is_busy() {
            self.notice(
                Level::Warn,
                "the agent is working · press esc to interrupt first",
            );
            return;
        }
        match self.backend.join_collab(link) {
            Ok(msg) => self.notice(Level::Info, msg),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    pub(super) fn handle_leave(&mut self) {
        match self.backend.leave_collab() {
            Ok(msg) => self.notice(Level::Info, msg),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// Dispatch a remote collaborator prompt without resolving host `@path` image references or
    /// draining the host user's staged image tray.
    pub(super) fn send_remote_collab_prompt(&mut self, badged_text: &str) {
        self.dispatch_prompt(badged_text, Some(badged_text.to_owned()), false);
    }

    pub(super) fn dispatch_prompt(
        &mut self,
        prompt: &str,
        display: Option<String>,
        allow_local_images: bool,
    ) {
        if self.backend.collab_guest_read_only() {
            self.notice(Level::Warn, "view-only collab link · prompting is disabled");
            return;
        }
        let shown = display.unwrap_or_else(|| prompt.to_owned());
        if allow_local_images && self.focused_agent.is_some() {
            self.send_focused_prompt(prompt, shown);
            return;
        }
        self.background.held = false;
        match self.task {
            Task::Turn { .. } => {
                self.backend.steer(prompt);
                self.transcript.push(Entry::Steer {
                    text: shown,
                    state: SteerState::Queued,
                });
            }
            Task::Compact => {
                if allow_local_images {
                    self.enqueue_prompt(&shown);
                } else {
                    self.notice(Level::Warn, "collab prompt dropped while compaction runs");
                }
            }
            Task::Idle => {
                self.last_prompt = Some(prompt.to_owned());
                let labeled_images = if allow_local_images {
                    self.images.take_for_prompt(prompt, &self.cwd)
                } else {
                    Vec::new()
                };
                let raw_images: Vec<hrn_kernel::ImageAttachment> =
                    labeled_images.iter().map(|(_, img)| img.clone()).collect();
                let res = if raw_images.is_empty() {
                    self.backend.submit(prompt)
                } else {
                    self.backend.submit_with_images(prompt, raw_images)
                };
                match res {
                    Ok(()) => {
                        if self.title.is_none() {
                            self.title = Some(crate::toolfmt::one_line(&shown));
                        }
                        self.transcript
                            .push_with_images(Entry::User(shown), labeled_images);
                        self.start_task(Task::Turn {
                            awaiting_start: true,
                        });
                    }
                    Err(err) => {
                        if allow_local_images {
                            self.editor.set(&shown);
                        }
                        self.notice(Level::Error, err.to_string());
                    }
                }
            }
        }
    }
}
