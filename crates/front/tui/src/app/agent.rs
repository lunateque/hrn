//! The backend half of the app loop: agent events into transcript entries and state, turn
//! bookkeeping (tokens, cost, usage), interruption and the settlement of finished tasks
//! (steers back into the editor, follow-ups sent, session reload after compaction).

use super::{App, Task};
use crate::git;
use crate::port::{TaskEnd, TurnOutcome, UiEvent};
use crate::render;
use crate::transcript::{Entry, Level, SteerState, ToolState};
use hrn_kernel::{
    AgentEvent, AgentStatus, DagNode, DagPayload, PermissionMode, SessionMeta, Usage,
};
use std::path::PathBuf;
use std::time::Instant;

impl App {
    pub(super) fn start_task(&mut self, task: Task) {
        self.task = task;
        let now = Instant::now();
        self.busy_since = Some(now);
        self.last_tick = now;
    }

    /// Ask for input: the title flips and an unfocused window is told about it.
    pub(super) fn announce_input_needed(&mut self) {
        self.notify
            .title(crate::notify::State::NeedsInput, self.title.as_deref());
        self.notify.announce(crate::notify::State::NeedsInput);
    }

    fn apply_session_title(&mut self, session_id: &str, title: &str) {
        if self.focused_agent.is_some() {
            return;
        }
        if self.backend.current_session_id().as_deref() == Some(session_id) {
            let trimmed = title.trim();
            self.title = (!trimmed.is_empty()).then(|| trimmed.to_owned());
            let state = if self.is_busy() {
                crate::notify::State::Working
            } else {
                crate::notify::State::Idle
            };
            self.notify.title(state, self.title.as_deref());
        }
    }

    pub(super) fn on_backend(&mut self, event: UiEvent) {
        match event {
            UiEvent::Agent(event) => self.on_agent(event),
            UiEvent::Approval(ask) => self.on_approval(ask),
            UiEvent::Ask(prompt) => self.on_ask(prompt),
            UiEvent::Finished(end) => self.on_finished(end),
            UiEvent::Running(running) => self.background.running = running,
            UiEvent::Report(text) => self.notice(Level::Info, text),
            UiEvent::BtwDelta { topic_id, delta } => self.on_btw_delta(&topic_id, &delta),
            UiEvent::BtwDone(topic) => self.on_btw_done(topic),
            UiEvent::AgentHub(snapshot) => self.on_agent_hub_snapshot(snapshot),
            UiEvent::Login(ev) => self.on_login_event(ev),
            UiEvent::Appearance(cfg) => self.apply_appearance(*cfg, true),
            UiEvent::ThemeReloaded(outcome) => self.on_theme_reloaded(outcome),
            UiEvent::KeymapReload => self.reload_keymap(true),
            UiEvent::LoopCondition {
                check_id,
                exit_code,
                timed_out,
                error,
            } => self.on_loop_condition(check_id, exit_code, timed_out, error),
            UiEvent::SessionTitle { session_id, title } => {
                self.apply_session_title(&session_id, &title);
            }
            UiEvent::SessionLoaded {
                meta,
                nodes,
                notice,
            } => {
                self.show_session(&meta, &nodes);
                if let Some(msg) = notice {
                    self.notice(Level::Info, msg);
                }
            }
            UiEvent::CollabPrompt { badged_text } => {
                self.send_remote_collab_prompt(&badged_text);
            }
            UiEvent::CollabAbort { guest_name } => {
                self.notice(
                    Level::Warn,
                    format!("[collab] {guest_name} requested turn abort"),
                );
                self.interrupt();
            }
            UiEvent::RemotePrompt { text } => self.on_remote_prompt(&text),
            UiEvent::RemoteSteer { text } => self.on_remote_steer(&text),
            UiEvent::RemoteAbort => self.on_remote_abort(),
            UiEvent::RemoteApprovalResolved { call_id } => {
                self.on_remote_approval_resolved(&call_id);
            }
            UiEvent::RemoteAskResolved { id } => self.on_remote_ask_resolved(&id),
            UiEvent::RemoteReady => self.on_remote_ready(),
        }
    }

    fn on_agent(&mut self, event: AgentEvent) {
        let now = Instant::now();
        let t = &mut self.transcript;
        match event {
            AgentEvent::TurnStart { user_text, .. } => self.on_turn_start(user_text, now),
            AgentEvent::SteerInjected { text, .. } => {
                t.close_blocks(now);
                t.mark_steer(&text, SteerState::Sent);
            }
            AgentEvent::ThinkingDelta { delta } if !self.settings.hide_thinking => {
                t.thinking(&delta, now);
            }
            AgentEvent::GoalUpdated { goal } => self.on_goal(goal, now),
            AgentEvent::TextDelta { delta } => t.text(&delta, now),
            AgentEvent::ToolStart {
                call_id,
                name,
                arguments,
            } => t.tool_start(&call_id, &name, &arguments, now),
            AgentEvent::ToolDelta { call_id, delta } => t.tool_delta(&call_id, &delta),
            AgentEvent::ToolDone {
                call_id,
                output,
                is_error,
                diff,
                images,
                ..
            } => {
                let labeled = images
                    .into_iter()
                    .map(|img| (Some(self.images.register(img.clone())), img))
                    .collect();
                t.tool_done_with_images(&call_id, &output, is_error, diff.as_deref(), labeled, now);
            }
            AgentEvent::ApprovalRequested { request } => {
                if let Some(card) = t.tool_mut(&request.id) {
                    card.state = ToolState::Awaiting;
                }
            }
            AgentEvent::ApprovalResolved { id, approved } => {
                if let Some(card) = t.tool_mut(&id)
                    && approved
                {
                    card.state = ToolState::Running;
                }
            }
            AgentEvent::LlmCall { usage, .. } => {
                if let Some(usage) = usage {
                    self.tokens.0 += usage.prompt_tokens;
                    self.tokens.1 += usage.completion_tokens;
                    self.last_prompt_tokens = usage.prompt_tokens;
                    self.reasoning_tokens += usage.reasoning_tokens.unwrap_or(0);
                    if let Some(micros) = usage.cost_micros {
                        *self.cost.get_or_insert(0) += micros;
                    }
                }
            }
            AgentEvent::Compacted {
                method,
                saved_chars,
                ..
            } => self.on_compacted(method, saved_chars, now),
            AgentEvent::AssistantNote { text } => {
                t.assistant_note(&text, now);
            }
            AgentEvent::AdvisorNote { model, note } => {
                t.notice(Level::Info, format!("advisor ({model}):\n{note}"), now);
            }
            AgentEvent::StatusChanged {
                status: AgentStatus::Working,
                message: Some(message),
                ..
            } => t.notice(Level::Warn, message, now),
            AgentEvent::TurnDone { usage, steps, .. } => self.turn_done(usage, steps, now),
            AgentEvent::TurnError { code, message, .. } => {
                if code != "cancelled" {
                    t.notice(Level::Error, message, now);
                }
            }
            AgentEvent::TodosUpdated { phases } => self.todo = phases,
            AgentEvent::CompactionSpeculating { active } => self.speculating_compact = active,
            AgentEvent::SessionTitle { session_id, title } => {
                self.apply_session_title(&session_id, &title);
            }
            // The todo card draws itself from the call's arguments; with `--hide-thinking`
            // thoughts are spent but never rendered.
            AgentEvent::ThinkingDelta { .. }
            | AgentEvent::StatusChanged { .. }
            | AgentEvent::AskRequested { .. } => {}
        }
    }

    /// Handle `AgentEvent::TurnStart` for both local turns and remote collab turns.
    fn on_turn_start(&mut self, user_text: String, now: Instant) {
        self.turn_started = Some(now);
        self.notify.reset();
        self.notify
            .title(crate::notify::State::Working, self.title.as_deref());
        match &mut self.task {
            Task::Turn { awaiting_start } => {
                if *awaiting_start {
                    *awaiting_start = false;
                } else {
                    self.transcript.settle_steers(SteerState::Sent);
                }
            }
            Task::Idle => {
                self.transcript.push(Entry::User(user_text));
                self.start_task(Task::Turn {
                    awaiting_start: false,
                });
            }
            Task::Compact => {}
        }
    }

    /// Render a compacted divider with the latest persisted summary body (collapsed until `Ctrl+O`).
    fn on_compacted(&mut self, method: String, saved_chars: usize, now: Instant) {
        self.transcript.close_blocks(now);
        let summary = self.backend.latest_compaction_summary().unwrap_or_default();
        self.transcript.push(Entry::Compacted {
            method,
            summary,
            saved_chars,
        });
    }

    /// The session's goal changed: the band follows and the transcript notes it.
    fn on_goal(&mut self, goal: Option<hrn_kernel::Goal>, now: Instant) {
        self.settings.goal.clone_from(&goal);
        let text = match goal {
            Some(goal) => format!("goal ({}): {}", goal.status.as_str(), goal.objective),
            None => "goal mode ended".to_owned(),
        };
        self.transcript.notice(Level::Info, text, now);
    }

    /// Close the turn's blocks and add its footer: time, steps and tokens.
    fn turn_done(&mut self, usage: Option<Usage>, steps: usize, now: Instant) {
        self.transcript.close_blocks(now);
        if let Some((idx, text)) = self
            .transcript
            .last_assistant_text()
            .map(|(i, s)| (i, s.to_owned()))
        {
            let previews = self.images.resolve_assistant_previews(&text, &self.cwd);
            self.transcript.attach_images(idx, previews);
        }
        let mut parts = Vec::new();
        if let Some(started) = self.turn_started.take() {
            parts.push(render::duration(now.duration_since(started)));
        }
        parts.push(format!("{steps} step{}", if steps == 1 { "" } else { "s" }));
        if let Some(u) = usage {
            parts.push(format!(
                "↑{} ↓{}",
                render::tokens(u.prompt_tokens),
                render::tokens(u.completion_tokens)
            ));
        }
        if let Some(micros) = usage.and_then(|u| u.cost_micros) {
            parts.push(render::money(micros));
        }
        self.transcript.push(Entry::Footer(parts.join(" · ")));
    }

    pub(super) fn interrupt(&mut self) {
        if !self.is_busy() {
            return;
        }
        // Dropping the pending asks makes their gates report cancellation.
        self.approvals.clear();
        self.approval_dock = None;
        self.active_ask = None;
        self.ask_queue.clear();
        self.backend.interrupt();
        self.flash("interrupting…");
    }

    fn on_finished(&mut self, end: TaskEnd) {
        if self.focused_agent.is_some()
            && let TaskEnd::Turn { outcome, .. } = &end
        {
            match outcome {
                TurnOutcome::Done => self.flash("main session turn finished"),
                TurnOutcome::Interrupted => self.flash("main session interrupted"),
                TurnOutcome::Failed(message) => {
                    self.flash(format!("main session error: {message}"));
                }
            }
            self.refresh_hub_snapshot();
            return;
        }
        let now = Instant::now();
        self.transcript.turn_finished(now);
        self.approvals.clear();
        self.approval_dock = None;
        self.active_ask = None;
        self.ask_queue.clear();
        self.task = Task::Idle;
        let elapsed = self
            .turn_started
            .or(self.busy_since)
            .map(|at| now.duration_since(at));
        self.busy_since = None;
        self.turn_started = None;
        // A transient hint such as "interrupting…" is stale once the task is over.
        self.flash = None;
        self.notify.reset();
        self.notify
            .title(crate::notify::State::Idle, self.title.as_deref());
        match elapsed {
            Some(elapsed) => self.notify.finished(crate::notify::State::Idle, elapsed),
            None => self.notify.announce(crate::notify::State::Idle),
        }
        match end {
            TaskEnd::Turn {
                outcome,
                unsent_steers,
            } => self.on_turn_finished(&outcome, &unsent_steers),
            TaskEnd::Compact(result) => match result {
                Ok(true) => {
                    self.notice(Level::Info, "history compacted");
                    if let Some(prompt) = self.resume_after_compact.take() {
                        self.send_prompt(&prompt, None);
                    } else {
                        self.drain_next_queued();
                    }
                }
                Ok(false) => {
                    self.resume_after_compact = None;
                    self.notice(Level::Info, "nothing to compact yet");
                    self.drain_next_queued();
                }
                Err(message) => {
                    self.resume_after_compact = None;
                    self.notice(Level::Error, format!("compaction failed: {message}"));
                }
            },
            TaskEnd::Handoff(result) => match result {
                Ok(true) => self.notice(Level::Info, "handoff summary committed"),
                Ok(false) => self.notice(Level::Info, "handoff skipped (empty or branch changed)"),
                Err(message) => self.notice(Level::Error, format!("handoff failed: {message}")),
            },
        }
        self.refresh_git();
    }

    fn on_turn_finished(&mut self, outcome: &TurnOutcome, unsent_steers: &[String]) {
        if let Some(pending) = self.pending_compact.take() {
            self.start_compact(pending.instructions.as_deref(), pending.resume_prompt);
            return;
        }
        match outcome {
            TurnOutcome::Done => {
                if self.settings.mode == PermissionMode::Plan {
                    self.flash("plan mode · /plan off or ctrl+y to start editing");
                }
            }
            TurnOutcome::Interrupted => {
                self.background.held = true;
                self.suspend_loop_on_abort("interruption");
                self.notice(Level::Info, "interrupted");
            }
            TurnOutcome::Failed(message) => {
                self.suspend_loop_on_abort("turn failure");
                let shown = matches!(
                    self.transcript.entry(self.transcript.len().wrapping_sub(1)),
                    Some(Entry::Notice(Level::Error, text)) if *text == *message
                );
                if !shown {
                    self.notice(Level::Error, message);
                }
            }
        }
        if !unsent_steers.is_empty() {
            self.transcript.settle_steers(SteerState::Returned);
            let mut text = unsent_steers.join("\n\n");
            if !self.editor.is_blank() {
                text = format!("{text}\n\n{}", self.editor.text());
            }
            self.editor.set(&text);
        }
        if matches!(outcome, TurnOutcome::Done) && !self.drain_next_queued() {
            self.advance_loop_after_turn();
        }
    }

    /// Replace the transcript with a stored session and adopt its settings and directory.
    pub(super) fn show_session(&mut self, meta: &SessionMeta, nodes: &[DagNode]) {
        self.show_session_with_running(meta, nodes, false);
    }

    /// Replace the transcript with a stored session, keeping trailing tool calls live when
    /// `running` is `true` (e.g. swapping back to a still-running main or child session).
    pub(super) fn show_session_with_running(
        &mut self,
        meta: &SessionMeta,
        nodes: &[DagNode],
        running: bool,
    ) {
        self.cwd = PathBuf::from(&meta.cwd);
        self.transcript
            .load_with_tray_running(nodes, &mut self.images, &self.cwd, running);
        self.image_modal = None;
        self.restart_screen(false);
        self.title = Some(meta.title.clone()).filter(|t| !t.trim().is_empty());
        self.todo = latest_todo_board(nodes);
        self.always.clear();
        let used = self.backend.session_usage().unwrap_or_else(|err| {
            self.notice(Level::Warn, format!("usage totals unavailable: {err}"));
            Usage::default()
        });
        self.tokens = (used.prompt_tokens, used.completion_tokens);
        self.cost = used.cost_micros;
        self.reasoning_tokens = used.reasoning_tokens.unwrap_or(0);
        self.last_prompt_tokens = self.backend.last_prompt_tokens();
        self.speculating_compact = false;
        self.settings.thinking = meta.thinking;
        self.settings.mode = meta.permission_mode;
        self.settings.goal.clone_from(&meta.goal);
        if let Some(model) = &meta.model {
            model.clone_into(&mut self.settings.model);
        }
        self.context_window = self.backend.context_window(&self.settings.model);
        self.cwd_label = git::display_path(&self.cwd);
        self.refresh_git();
    }
}

/// The checklist of the newest board node in a loaded branch.
fn latest_todo_board(nodes: &[DagNode]) -> Vec<hrn_kernel::TodoPhase> {
    nodes
        .iter()
        .rev()
        .find_map(|n| match &n.payload {
            DagPayload::TodoBoard { phases } => Some(phases.clone()),
            _ => None,
        })
        .unwrap_or_default()
}
