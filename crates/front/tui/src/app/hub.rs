//! Live Agent Hub (`Alt+A` / double-`←`), child-agent focusing/steering, `/agents` definition
//! hub, and pinned Subagents click-to-focus orchestration on [`App`].

use super::{App, Task};
use crate::agents_hub::{AgentsHub, AgentsHubAction};
use crate::hub::{AgentHub, AgentHubAction};
use crate::port::{AgentHubSnapshot, AgentHubStatus};
use crate::transcript::{Entry, Level, SteerState};
use crossterm::event::KeyEvent;
use std::time::Duration;

/// Maximum gap between two `←` taps on an empty editor to open Agent Hub or leave child focus.
pub(super) const LEFT_DOUBLE_WINDOW: Duration = Duration::from_millis(600);

impl App {
    /// Refresh `self.background.hub` from the backend when the backend supplies non-empty data
    /// (preserving event-pushed snapshots in backends/tests that only emit `UiEvent::AgentHub`).
    pub(super) fn refresh_hub_snapshot(&mut self) {
        let fresh = self.backend.agent_hub_snapshot();
        if !fresh.agents.is_empty() || !fresh.activity.is_empty() {
            self.background.hub = fresh;
        }
        if let Some(hub) = &mut self.agent_hub {
            hub.update(self.background.hub.clone());
        }
    }

    /// Apply a live hub snapshot pushed by the backend (`UiEvent::AgentHub`).
    pub(super) fn on_agent_hub_snapshot(&mut self, snapshot: AgentHubSnapshot) {
        self.background.hub = snapshot.clone();
        if let Some(hub) = &mut self.agent_hub {
            hub.update(snapshot);
        }
        if let Some(focused) = self.focused_agent.as_deref()
            && let Some(entry) = self.background.hub.agents.iter().find(|e| e.id == focused)
        {
            match entry.status {
                AgentHubStatus::Running => {
                    if matches!(self.task, Task::Idle) {
                        self.start_task(Task::Turn {
                            awaiting_start: false,
                        });
                    }
                }
                _ => {
                    if matches!(self.task, Task::Turn { .. }) {
                        self.task = Task::Idle;
                        self.busy_since = None;
                    }
                }
            }
        }
    }

    /// Open the live Agent Hub (`Alt+A` or double-`←` on an empty editor).
    pub(super) fn open_agent_hub(&mut self) {
        self.refresh_hub_snapshot();
        self.agent_hub = Some(AgentHub::new(self.background.hub.clone()));
    }

    /// Open the `/agents` definition hub (separate from live Agent Hub).
    pub(super) fn open_agents_hub(&mut self) {
        let defs = self.backend.agent_definitions();
        if defs.is_empty() {
            self.notice(Level::Info, "no subagent definitions discovered");
            return;
        }
        self.agents_hub = Some(AgentsHub::new(defs));
    }

    /// Route one keystroke into the open live Agent Hub; returns `true` when consumed.
    pub(super) fn agent_hub_key(&mut self, key: KeyEvent) -> bool {
        let Some(hub) = &mut self.agent_hub else {
            return false;
        };
        match hub.key(key, self.overlay_area) {
            AgentHubAction::Stay => {}
            AgentHubAction::Close => self.agent_hub = None,
            AgentHubAction::Focus(id) => {
                self.agent_hub = None;
                self.focus_agent(&id);
            }
            AgentHubAction::Revive(id) => match self.backend.revive_agent(&id) {
                Ok(msg) => {
                    self.refresh_hub_snapshot();
                    self.flash(msg);
                }
                Err(err) => self.flash(err.to_string()),
            },
            AgentHubAction::Abort(id) => match self.backend.abort_agent(&id) {
                Ok(msg) => {
                    self.refresh_hub_snapshot();
                    self.flash(msg);
                }
                Err(err) => self.flash(err.to_string()),
            },
            AgentHubAction::JumpToTranscript {
                agent_id, summary, ..
            } => {
                self.agent_hub = None;
                if agent_id == "main" {
                    if self.focused_agent.is_some() {
                        self.unfocus_agent();
                    }
                } else {
                    self.focus_agent(&agent_id);
                }
                self.flow.replay = true;
                self.flash(format!("jumped to {agent_id}: {summary}"));
            }
        }
        true
    }

    /// Route one keystroke into the open `/agents` definition hub; returns `true` when consumed.
    pub(super) fn agents_hub_key(&mut self, key: KeyEvent) -> bool {
        let Some(hub) = &mut self.agents_hub else {
            return false;
        };
        match hub.key(key, self.overlay_area) {
            AgentsHubAction::Stay => {}
            AgentsHubAction::Close => self.agents_hub = None,
            AgentsHubAction::ToggleEnabled { name, enabled } => {
                match self.backend.set_agent_enabled(&name, enabled) {
                    Ok(msg) => {
                        let defs = self.backend.agent_definitions();
                        if let Some(h) = &mut self.agents_hub {
                            h.refresh(defs);
                        }
                        self.flash(msg);
                    }
                    Err(err) => self.flash(err.to_string()),
                }
            }
            AgentsHubAction::SetModelOverride { name, model } => {
                match self
                    .backend
                    .set_agent_model_override(&name, model.as_deref())
                {
                    Ok(msg) => {
                        let defs = self.backend.agent_definitions();
                        if let Some(h) = &mut self.agents_hub {
                            h.refresh(defs);
                        }
                        self.flash(msg);
                    }
                    Err(err) => self.flash(err.to_string()),
                }
            }
        }
        true
    }

    /// Swap the active TUI transcript/editor/status view onto child agent `agent_id`.
    /// Focusing a `parked` or `aborted` agent revives it first inside the backend.
    pub(super) fn focus_agent(&mut self, agent_id: &str) {
        let is_main = agent_id == "main"
            || self
                .background
                .hub
                .agents
                .iter()
                .find(|e| e.id == agent_id)
                .is_some_and(|e| e.is_main);
        if is_main {
            if self.focused_agent.is_some() {
                self.unfocus_agent();
            } else {
                self.flash("already on main session");
            }
            return;
        }
        match self.backend.focus_agent(agent_id) {
            Ok((meta, nodes)) => {
                if self.focused_agent.is_none() {
                    self.saved_main_settings = Some(self.settings.clone());
                }
                self.focused_agent = Some(agent_id.to_owned());
                self.show_session(&meta, &nodes);
                self.title = Some(format!("agent:{agent_id}"));
                self.refresh_hub_snapshot();
                let running = self
                    .background
                    .hub
                    .agents
                    .iter()
                    .find(|e| e.id == agent_id)
                    .is_some_and(|e| e.status == AgentHubStatus::Running);
                if running {
                    self.start_task(Task::Turn {
                        awaiting_start: false,
                    });
                } else {
                    self.task = Task::Idle;
                    self.busy_since = None;
                }
                self.flash(format!("focused agent `{agent_id}` · esc or ←← for main"));
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// Return from a focused child agent back to the main root session.
    pub(super) fn unfocus_agent(&mut self) {
        if self.focused_agent.is_none() {
            return;
        }
        match self.backend.unfocus_agent() {
            Ok(loaded) => {
                self.focused_agent = None;
                self.task = Task::Idle;
                self.busy_since = None;
                if let Some((meta, nodes)) = loaded {
                    self.show_session(&meta, &nodes);
                } else if let Some(saved) = self.saved_main_settings.take() {
                    self.settings = saved;
                    self.context_window = self.backend.context_window(&self.settings.model);
                    self.title = None;
                }
                self.saved_main_settings = None;
                self.flash("returned to main session");
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// Handle submitting text while focused on a child agent: steers a running child or prompts an
    /// idle/revived child.
    pub(super) fn send_focused_prompt(&mut self, prompt: &str, shown: String) {
        let Some(agent_id) = self.focused_agent.clone() else {
            return;
        };
        match self.backend.steer_or_prompt_agent(&agent_id, prompt) {
            Ok(true) => {
                self.transcript.push(Entry::Steer {
                    text: shown,
                    state: SteerState::Queued,
                });
                self.flash(format!("steered agent `{agent_id}`"));
            }
            Ok(false) => {
                self.transcript.push(Entry::User(shown));
                self.start_task(Task::Turn {
                    awaiting_start: true,
                });
                self.refresh_hub_snapshot();
                self.flash(format!("sent follow-up to `{agent_id}`"));
            }
            Err(err) => {
                self.editor.set(&shown);
                self.notice(Level::Error, err.to_string());
            }
        }
    }

    /// Double-`←` gesture on an empty editor: opens Live Agent Hub on main, or returns to main
    /// while focused on a child agent.
    pub(super) fn on_empty_left_key(&mut self) {
        let now = self.now();
        if self
            .left_armed
            .take()
            .is_some_and(|at| now.duration_since(at) < LEFT_DOUBLE_WINDOW)
        {
            self.flash = None;
            if self.focused_agent.is_some() {
                self.unfocus_agent();
            } else {
                self.open_agent_hub();
            }
            return;
        }
        self.left_armed = Some(now);
        if self.focused_agent.is_some() {
            self.flash("press ← again to return to main");
        } else {
            self.flash("press ← again to open agent hub");
        }
    }

    /// Handle a left mouse click when `[tui] mouse = true`, focusing the clicked pinned subagent.
    pub(super) fn on_mouse_click(&mut self, row: u16) {
        if !self.mouse || self.has_full_view() || self.pinned_hitboxes.is_empty() {
            return;
        }
        let target = self
            .pinned_hitboxes
            .iter()
            .find(|(hit_y, _)| {
                *hit_y == row || self.last_live_origin_y.saturating_add(*hit_y) == row
            })
            .map(|(_, id)| id.clone());
        if let Some(id) = target {
            self.focus_agent(&id);
        }
    }
}
