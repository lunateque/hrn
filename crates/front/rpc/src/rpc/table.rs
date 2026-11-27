//! The RPC command table: which handler answers which command, and the queue commands.

use super::Core;
use super::commands::{
    Reply, compaction_json, field, flag, one_of, set_event_filter, set_fast_mode, set_slow_mode,
};
use super::prompt::{Job, Queue, images_of, job_from, queue_of};
use super::state::thinking_levels;
use crate::backend::{CompactionReport, SessionPatch};
use crate::events::usage_json;
use crate::history;
use crate::wire::id_key;
use hrn_kernel::{
    BoxFuture, CancelFlag, EventSink, HrnError, PermissionMode, Result, ThinkingLevel,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
impl Core {
    /// Dispatch one inbound line.
    pub(crate) async fn handle_line(core: &Arc<Core>, line: &str) {
        let frame = match serde_json::from_str::<Value>(line) {
            Ok(frame) if frame.is_object() => frame,
            Ok(_) => {
                core.respond(
                    None,
                    "parse",
                    &Self::fail("command must be a JSON object".to_owned(), None),
                )
                .await;
                return;
            }
            Err(err) => {
                core.respond(
                    None,
                    "parse",
                    &Self::fail(format!("invalid JSON: {err}"), None),
                )
                .await;
                return;
            }
        };
        let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "extension_ui_response" {
            // A side-channel frame: it answers a request of ours and gets no response itself.
            let id = frame.get("id").map(id_key).unwrap_or_default();
            core.pending.resolve(&id, frame);
            return;
        }
        let id = frame.get("id").filter(|v| !v.is_null()).cloned();
        if kind.is_empty() {
            core.respond(
                id.as_ref(),
                "parse",
                &Self::fail("command is missing `type`".to_owned(), None),
            )
            .await;
            return;
        }
        let reply = match dispatch(core, kind, &frame).await {
            Ok(reply) => reply,
            Err(err) => Reply::Body(Self::fail(err.to_string(), Some(err.code()))),
        };
        if let Reply::Body(body) = reply {
            core.respond(id.as_ref(), kind, &body).await;
        }
    }

    /// `prompt`, `steer`, `follow_up`: admit, or reject a plain prompt that met a live run.
    async fn prompt_command(core: &Arc<Self>, command: &str, frame: &Value) -> Result<Reply> {
        let job = job_from(core, frame)?;
        let queue = queue_of(frame, command);
        core.clone().admit(command, job, queue).await?;
        Ok(Reply::Deferred)
    }

    /// `abort_and_prompt`: end the live run, answer, then start a fresh one.
    async fn abort_and_prompt(core: &Arc<Self>, frame: &Value) -> Result<Reply> {
        let mut job = job_from(core, frame)?;
        core.abort_run().await;
        let id = job.id.clone();
        core.respond(
            id.as_ref(),
            "abort_and_prompt",
            &Core::ok(Some(json!({ "aborted": true }))),
        )
        .await;
        job.id = id;
        // The message is now the first thing a fresh run takes.
        core.clone().admit("prompt", job, Queue::FollowUp).await?;
        Ok(Reply::Deferred)
    }

    /// `set_model`: `provider` plus `modelId`, omp's failure text kept for a miss.
    async fn set_model(core: &Arc<Self>, frame: &Value) -> Result<Reply> {
        let provider = field(frame, "provider")
            .ok_or_else(|| HrnError::Config("`provider` is required".to_owned()))?;
        let id = field(frame, "modelId")
            .or_else(|| field(frame, "id"))
            .ok_or_else(|| HrnError::Config("`modelId` is required".to_owned()))?;
        let selector = format!("{provider}/{id}");
        let model = core
            .backend
            .find_model(&selector)
            .map_err(|_| HrnError::Config(format!("Model not found: {selector}")))?;
        if let Some(level) = field(frame, "thinkingLevel")
            .as_deref()
            .and_then(ThinkingLevel::parse)
        {
            core.lock().thinking = level;
        }
        core.apply_model(&model).await?;
        Ok(Reply::data(core.backend.model_json(&model.alias)))
    }

    /// `cycle_model`: the next configured model.
    async fn cycle_model(core: &Arc<Self>) -> Result<Reply> {
        let models = core.backend.models();
        let current = core.lock().model.clone();
        let Some(position) = models.iter().position(|m| m.alias == current) else {
            return Ok(Reply::err("no current model to cycle from"));
        };
        let Some(next) = models.get((position + 1) % models.len()) else {
            return Ok(Reply::err("the model list changed while cycling"));
        };
        core.apply_model(next).await?;
        Ok(Reply::data(core.backend.model_json(&next.alias)))
    }

    /// Make `model` the session's model and tell the runtime and the host.
    async fn apply_model(&self, model: &crate::backend::ModelEntry) -> Result {
        self.lock().model.clone_from(&model.alias);
        self.sync()?;
        self.send(&json!({
            "type": "model_changed",
            "provider": model.provider,
            "modelId": model.id,
        }))
        .await;
        Ok(())
    }

    /// `compact`: run a compaction, answering the command when it settles.
    async fn compact_command(core: &Arc<Self>) -> Result<Reply> {
        let Some(id) = core.session(true)? else {
            return Ok(Reply::err("Nothing to compact"));
        };
        {
            let mut state = core.lock();
            if state.compacting {
                return Ok(Reply::err("A compaction is already running"));
            }
            state.compacting = true;
            state.translator.set_manual_compaction(true);
        }
        core.send(&json!({ "type": "manual_compaction_start" }))
            .await;
        let cancel = CancelFlag::new();
        core.lock().compaction = Some(cancel.clone());
        let events: Arc<dyn EventSink> = core.sink();
        let report = core.backend.compact(&id, cancel, events);
        let core = Arc::clone(core);
        tokio::spawn(async move { core.finish_compaction(report).await });
        Ok(Reply::Deferred)
    }

    /// Report a finished compaction to the stream and to the waiting `compact` command.
    async fn finish_compaction(
        self: Arc<Self>,
        report: BoxFuture<Result<Option<CompactionReport>>>,
    ) {
        let outcome = report.await;
        {
            let mut state = self.lock();
            state.compacting = false;
            state.compaction = None;
            state.translator.set_manual_compaction(false);
        }
        self.idle.notify_waiters();
        let (end, body) = match &outcome {
            Ok(Some(report)) => (
                json!({
                    "type": "manual_compaction_end",
                    "aborted": false,
                    "result": compaction_json(report),
                }),
                Core::ok(Some(compaction_json(report))),
            ),
            Ok(None) => (
                json!({
                    "type": "manual_compaction_end",
                    "aborted": false,
                    "skipped": true,
                    "errorMessage": "Nothing to compact",
                }),
                Self::fail("Nothing to compact".to_owned(), None),
            ),
            Err(HrnError::Cancelled) => (
                json!({
                    "type": "manual_compaction_end",
                    "aborted": true,
                    "errorMessage": "cancelled",
                }),
                Self::fail("cancelled".to_owned(), Some("cancelled")),
            ),
            Err(err) => (
                json!({
                    "type": "manual_compaction_end",
                    "aborted": false,
                    "errorMessage": err.to_string(),
                }),
                Self::fail(err.to_string(), Some(err.code())),
            ),
        };
        self.send(&end).await;
        self.respond(None, "compact", &body).await;
        if self.is_settled() {
            self.send(&json!({ "type": "session_settled" })).await;
        }
    }

    /// `remove_queued_message`: drop one pending queue chip.
    fn remove_queued(core: &Arc<Core>, frame: &Value) -> Result<Reply> {
        let text = field(frame, "message")
            .ok_or_else(|| HrnError::Config("`message` must be a non-empty string".to_owned()))?;
        let queue = one_of(frame, "queue", &["steering", "followUp"])?;
        let removed = {
            let mut state = core.lock();
            let list = if queue == "steering" {
                &mut state.steering
            } else {
                &mut state.follow_up
            };
            list.iter()
                .position(|chip| *chip == text)
                .is_some_and(|index| {
                    list.remove(index);
                    true
                })
        };
        Ok(Reply::data(json!({ "removed": removed })))
    }

    /// `promote_queued_message`: a follow-up becomes steering, delivered by the live run.
    async fn promote_queued(core: &Arc<Core>, frame: &Value) -> Result<Reply> {
        let text = field(frame, "message")
            .ok_or_else(|| HrnError::Config("`message` must be a non-empty string".to_owned()))?;
        let promoted = {
            let mut state = core.lock();
            match state.follow_up.iter().position(|chip| *chip == text) {
                Some(index) => {
                    let job = state.follow_up.remove(index);
                    state.steering.push(job.clone());
                    if let (true, Some(id)) = (state.run.is_some(), state.session.clone()) {
                        core.backend.steer(&id, job);
                    }
                    true
                }
                None => false,
            }
        };
        core.sync_queue().await;
        Ok(Reply::data(json!({ "promoted": promoted })))
    }

    /// A session change: the live run's prompts complete as aborted.
    async fn change_session(core: &Arc<Core>) {
        core.abort_run().await;
        let mut state = core.lock();
        state.session = None;
        state.session_name = None;
        state.grants.clear();
        state.last_run = None;
        state.steering.clear();
        state.follow_up.clear();
    }

    /// `switch_session`: adopt a stored conversation.
    async fn adopt(core: &Arc<Core>, id: &str) -> Result<Reply> {
        let meta = core.backend.find_session(id)?;
        Core::change_session(core).await;
        let mut state = core.lock();
        state.session = Some(meta.id.clone());
        state.session_name = Some(meta.title.clone());
        state.cwd = PathBuf::from(&meta.cwd);
        if let Some(model) = &meta.model {
            state.model.clone_from(model);
        }
        state.thinking = meta.thinking;
        state.mode = meta.permission_mode;
        Ok(Reply::data(json!({
            "cancelled": false,
            "resumed": meta.head_node_id.is_some(),
            "sessionId": meta.id,
        })))
    }

    /// `fork`: a new session from the current one's head or from `entryId`.
    async fn fork(core: &Arc<Core>, frame: &Value) -> Result<Reply> {
        let Some(id) = core.lock().session.clone() else {
            return Ok(Reply::err("no active session"));
        };
        let node = field(frame, "entryId");
        let forked = core.backend.fork(&id, node.as_deref())?;
        Core::change_session(core).await;
        core.lock().session = Some(forked.id.clone());
        Ok(Reply::data(
            json!({ "cancelled": false, "sessionId": forked.id }),
        ))
    }
}

/// Run `command`; `Reply::Deferred` when the handler answered itself.
#[expect(
    clippy::too_many_lines,
    reason = "the command table is one flat arm per host command on purpose"
)]
async fn dispatch(core: &Arc<Core>, command: &str, frame: &Value) -> Result<Reply> {
    match command {
        "prompt" | "steer" | "follow_up" => Core::prompt_command(core, command, frame).await,
        "abort_and_prompt" => Core::abort_and_prompt(core, frame).await,
        "abort" => {
            core.abort_run().await;
            Ok(Reply::done())
        }
        "abort_and_restore_queue" => {
            let (had_run, steering, follow_up) = {
                let state = core.lock();
                (
                    state.run.is_some(),
                    state.steering.clone(),
                    state.follow_up.clone(),
                )
            };
            core.abort_run().await;
            let mut restored = steering;
            restored.extend(follow_up);
            for text in restored {
                core.clone()
                    .admit(
                        "prompt",
                        Job {
                            id: None,
                            text,
                            images: Vec::new(),
                        },
                        Queue::FollowUp,
                    )
                    .await?;
            }
            Ok(Reply::data(json!({ "aborted": had_run })))
        }
        "abort_and_deliver_queue" => {
            let queued = core.lock().follow_up.clone();
            let Some(first) = queued.first() else {
                core.abort_run().await;
                return Ok(Reply::data(json!({ "aborted": false })));
            };
            let job = Job {
                id: None,
                text: first.clone(),
                images: images_of(frame),
            };
            core.abort_run().await;
            core.respond(
                None,
                "abort_and_deliver_queue",
                &Core::ok(Some(json!({ "aborted": true }))),
            )
            .await;
            core.clone().admit("prompt", job, Queue::FollowUp).await?;
            Ok(Reply::Deferred)
        }
        "resume" => match core.last_user_message() {
            Some(job) => {
                core.clone().admit("resume", job, Queue::FollowUp).await?;
                Ok(Reply::Deferred)
            }
            None => Ok(Reply::err("Nothing to resume")),
        },
        "get_state" => Ok(Reply::data(core.state_payload()?)),
        "get_messages" => Ok(Reply::data(json!({
            "messages": history::messages(&core.branch()?),
        }))),
        "get_last_assistant_text" => Ok(Reply::data(
            history::last_assistant_text(&core.branch()?)
                .map_or_else(|| json!({ "text": null }), |text| json!({ "text": text })),
        )),
        "get_session_stats" => {
            let nodes = core.branch()?;
            let (user, assistant, tool_calls, results) = history::counts(&nodes);
            let usage = core
                .lock()
                .session
                .clone()
                .map(|id| core.backend.usage(&id))
                .transpose()?;
            Ok(Reply::data(json!({
                "sessionId": core.lock().session,
                "userMessages": user,
                "assistantMessages": assistant,
                "toolCalls": tool_calls,
                "toolResults": results,
                "totalMessages": user + assistant + results,
                "usage": usage_json(usage.as_ref()),
            })))
        }
        "get_available_models" => {
            let current = core.lock().model.clone();
            Ok(Reply::data(json!({
                "models": core.backend.model_rows(&current),
            })))
        }
        "get_available_thinking_levels" => Ok(Reply::data(json!({ "levels": thinking_levels() }))),
        "set_model" => Core::set_model(core, frame).await,
        "cycle_model" => Core::cycle_model(core).await,
        "set_thinking_level" | "cycle_thinking_level" => {
            let level = if command == "cycle_thinking_level" {
                core.lock().thinking.next()
            } else {
                field(frame, "level")
                    .as_deref()
                    .and_then(ThinkingLevel::parse)
                    .ok_or_else(|| {
                        HrnError::Config(format!(
                            "`level` must be one of {}",
                            thinking_levels().join(", ")
                        ))
                    })?
            };
            core.lock().thinking = level;
            core.sync()?;
            core.send(&json!({
                "type": "thinking_level_changed",
                "thinkingLevel": level.as_str(),
            }))
            .await;
            Ok(Reply::done())
        }
        "compact" => Core::compact_command(core).await,
        "set_auto_compaction" => {
            let enabled = flag(frame, "enabled", true)?;
            core.lock().auto_compaction = enabled;
            Ok(Reply::data(json!({ "enabled": enabled })))
        }
        "set_steering_mode" | "set_follow_up_mode" => {
            let mode = one_of(frame, "mode", &["all", "one-at-a-time"])?;
            let mut state = core.lock();
            if command == "set_steering_mode" {
                state.steering_mode.clone_from(&mode);
            } else {
                state.follow_up_mode.clone_from(&mode);
            }
            Ok(Reply::data(json!({ "mode": mode })))
        }
        "set_interrupt_mode" => {
            let mode = one_of(frame, "mode", &["immediate", "wait"])?;
            core.lock().interrupt_mode.clone_from(&mode);
            Ok(Reply::data(json!({ "mode": mode })))
        }
        "set_permission_mode" => {
            let mode = field(frame, "mode")
                .as_deref()
                .and_then(PermissionMode::parse)
                .ok_or_else(|| {
                    HrnError::Config(
                        "`mode` must be plan, default, always-ask-cached, auto-edit or yolo"
                            .to_owned(),
                    )
                })?;
            core.lock().mode = mode;
            core.sync()?;
            Ok(Reply::data(json!({ "mode": mode.as_str() })))
        }
        "forget_approvals" => {
            core.lock().grants.clear();
            core.backend.forget_approvals()?;
            Ok(Reply::done())
        }
        "set_event_filter" => Ok(set_event_filter(core, frame)),
        "negotiate_protocol" => match frame.get("protocolVersion").and_then(Value::as_u64) {
            Some(1) => Ok(Reply::data(json!({ "protocolVersion": 1 }))),
            Some(other) => Ok(Reply::err(format!(
                "protocol version {other} is not supported; this server speaks 1"
            ))),
            None => Err(HrnError::Config(
                "`protocolVersion` must be a number".to_owned(),
            )),
        },
        "set_ask_dialog" => {
            core.lock().ask_dialog = flag(frame, "enabled", true)?;
            Ok(Reply::done())
        }
        "get_available_commands" => Ok(Reply::data(json!({
            "commands": core.command_rows(),
        }))),
        "new_session" => {
            Core::change_session(core).await;
            Ok(Reply::data(json!({ "cancelled": false })))
        }
        "switch_session" => {
            let id = field(frame, "sessionId")
                .or_else(|| field(frame, "session"))
                .ok_or_else(|| HrnError::Config("`sessionId` is required".to_owned()))?;
            Core::adopt(core, &id).await
        }
        "fork" => Core::fork(core, frame).await,
        "set_session_name" => {
            let Some(name) = field(frame, "name") else {
                return Ok(Reply::err("Session name cannot be empty"));
            };
            core.lock().session_name = Some(name.clone());
            if let Some(id) = core.lock().session.clone() {
                core.backend.patch_session(
                    &id,
                    SessionPatch {
                        title: Some(name),
                        ..Default::default()
                    },
                )?;
            }
            Ok(Reply::done())
        }
        "export_html" => {
            let Some(id) = core.session(true)? else {
                return Ok(Reply::err("nothing to export: no session yet"));
            };
            let path = field(frame, "path").map(PathBuf::from);
            let written = core.backend.export_html(&id, path.as_deref())?;
            Ok(Reply::data(json!({ "path": written.to_string_lossy() })))
        }
        "remove_queued_message" => {
            let reply = Core::remove_queued(core, frame)?;
            core.sync_queue().await;
            Ok(reply)
        }
        "promote_queued_message" => Core::promote_queued(core, frame).await,
        "set_fast_mode" => set_fast_mode(core, frame),
        "set_slow_mode" => set_slow_mode(core, frame),
        "open_session"
        | "set_cache_warming"
        | "set_auto_retry"
        | "goal"
        | "btw"
        | "btw_cancel"
        | "get_btw_history"
        | "login"
        | "logout"
        | "get_login_providers"
        | "get_logout_accounts"
        | "set_host_tools"
        | "host_tool_result"
        | "host_tool_update"
        | "set_host_uri_schemes"
        | "host_uri_result"
        | "set_subagent_subscription"
        | "get_subagents"
        | "get_subagent_messages"
        | "cancel_subagent"
        | "steer_subagent"
        | "get_entries"
        | "get_tree"
        | "set_todos"
        | "navigate_tree"
        | "branch"
        | "switch_session_dir"
        | "bash"
        | "predict"
        | "live_start"
        | "live_stop"
        | "live_mute"
        | "live_connect"
        | "dictation_start"
        | "dictation_stop"
        | "get_model_picker"
        | "cycle_thinking"
        | "compact_manual" => Ok(Reply::Body(Core::unsupported(command))),
        unknown => Ok(Reply::err(format!("Unknown command: {unknown}"))),
    }
}
