//! The run loop of an RPC connection: admitting prompts, keeping the steer and follow-up queues,
//! and reporting `prompt_result` / `session_settled`.
//!
//! One run is live at a time. An `abort` (or a session change) ends the live run's bookkeeping at
//! once and bumps [`State::generation`]; the looping task notices and stops without reporting, so
//! every admitted prompt is answered exactly once. A prompt that arrives while the previous run is
//! still unwinding waits for it, which keeps omp's documented ordering: `abort` completes before
//! the next command is admitted.

use super::ui::{Asker, Gate};
use super::{Core, State};
use crate::backend::{TurnRequest, TurnSummary};
use crate::wire::put;
use hrn_kernel::{
    ApprovalGate, AskHandler, CancelFlag, DagNode, DagPayload, EventSink, HrnError,
    ImageAttachment, Result, Role,
};

use serde_json::{Value, json};
use std::sync::Arc;

/// One prompt admitted by the connection.
#[derive(Clone, Debug)]
pub(crate) struct Job {
    /// The command id, when the host sent one; every admitted prompt answers it once.
    pub(crate) id: Option<Value>,
    /// The prompt text; queue chips show it verbatim.
    pub(crate) text: String,
    /// Attached images.
    pub(crate) images: Vec<ImageAttachment>,
}

/// How a host asked to queue a message while a run is live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Behavior {
    /// Deliver as steering at the next step boundary.
    Steer,
    /// Deliver when the agent would otherwise stop.
    FollowUp,
}

/// Where an accepted message goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Queue {
    /// A `prompt`: rejected while a run is live unless it carries a valid behavior.
    Prompt(Option<Behavior>),
    /// A `steer`: joins the live run, or runs alone when the session is idle.
    Steer,
    /// A `follow_up`: waits until the agent would otherwise stop.
    FollowUp,
}

/// `AgentContent` as the runtime wants it; `None` for anything but an image.
pub(crate) fn image(entry: &Value) -> Option<ImageAttachment> {
    if entry.get("type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    Some(ImageAttachment {
        media_type: entry
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_owned(),
        data_base64: entry
            .get("data")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        label: entry
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_owned),
        width: None,
        height: None,
    })
}

/// The images of a `prompt`-style frame.
pub(crate) fn images_of(frame: &Value) -> Vec<ImageAttachment> {
    frame
        .get("images")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(image).collect())
        .unwrap_or_default()
}

/// Where a frame's message is headed. An unrecognized `streamingBehavior` counts as absent, so a
/// plain prompt meets the same rejection during a live run.
pub(crate) fn queue_of(frame: &Value, command: &str) -> Queue {
    let behavior = match frame.get("streamingBehavior").and_then(Value::as_str) {
        Some("steer") => Some(Behavior::Steer),
        Some("followUp") => Some(Behavior::FollowUp),
        _ => None,
    };
    match command {
        "steer" => Queue::Steer,
        "follow_up" => Queue::FollowUp,
        _ => Queue::Prompt(behavior),
    }
}

/// The `prompt`/`steer`/`follow_up`/`abort_and_prompt` payload, with a slash command expanded.
///
/// # Errors
/// `message` is missing or blank, or names a command this runtime does not have.
pub(crate) fn job_from(core: &Core, frame: &Value) -> Result<Job> {
    let raw = frame
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HrnError::Config("`message` must be a non-empty string".to_owned()))?;
    let mut job = Job {
        id: frame.get("id").filter(|v| !v.is_null()).cloned(),
        text: raw.to_owned(),
        images: images_of(frame),
    };
    let Some(rest) = raw.strip_prefix('/') else {
        return Ok(job);
    };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if name.is_empty() {
        return Ok(job);
    }
    let cwd = core.lock().cwd.clone();
    job.text = core
        .backend
        .expand_command(&cwd, name, args.trim())
        .ok_or_else(|| {
            HrnError::Config(format!(
                "`/{name}` is not a command of this session; describe its work as a prompt instead"
            ))
        })?;
    Ok(job)
}

impl State {
    /// Take the queued follow-ups one delivery hands over: one, or all of them when the host asked
    /// for `"all"`.
    fn take_follow_ups(&mut self) -> Vec<Job> {
        let count = if self.follow_up_mode == "all" {
            self.follow_up.len()
        } else {
            usize::from(!self.follow_up.is_empty())
        };
        self.follow_up
            .drain(..count)
            .map(|text| Job {
                id: None,
                text,
                images: Vec::new(),
            })
            .collect()
    }
}

impl Core {
    /// Admit `job`: start a run when the connection is idle, queue it when one is live. `reply` is
    /// the `response` body written once the message is admitted.
    ///
    /// # Errors
    /// A plain `prompt` met a live run: `streamingBehavior` is required to queue it.
    pub(crate) async fn admit(
        self: Arc<Self>,
        command: &str,
        job: Job,
        queue: Queue,
    ) -> Result<Value> {
        let waiter = job.id.clone().unwrap_or(Value::Null);
        if self.lock().run.is_some() {
            match queue {
                Queue::Steer | Queue::Prompt(Some(Behavior::Steer)) => {
                    self.respond(job.id.as_ref(), command, &Core::ok(None))
                        .await;
                    self.push_steering(job);
                }
                Queue::FollowUp | Queue::Prompt(Some(Behavior::FollowUp)) => {
                    self.respond(job.id.as_ref(), command, &Core::ok(None))
                        .await;
                    self.lock().follow_up.push(job.text);
                }
                Queue::Prompt(None) => {
                    return Err(HrnError::Config(
                        "the agent is already processing a prompt; pass `streamingBehavior` \
                         (\"steer\" or \"followUp\") to queue this message"
                            .to_owned(),
                    ));
                }
            }
            if !waiter.is_null() {
                self.lock().waiters.push(waiter);
            }
            self.sync_queue().await;
            return Ok(Value::Null);
        }
        self.respond(job.id.as_ref(), command, &Core::ok(None))
            .await;
        let generation = {
            let mut state = self.lock();
            state.run = Some(CancelFlag::new());
            state.run_active = true;
            if !waiter.is_null() {
                state.waiters.push(waiter);
            }
            state.generation
        };
        let core = Arc::clone(&self);
        tokio::spawn(async move {
            core.run_loop(generation, job).await;
        });
        Ok(Value::Null)
    }

    /// Queue steering: the live run takes it through the runtime, and the chip shows until it is
    /// injected.
    fn push_steering(&self, job: Job) {
        if let Some(id) = self.lock().session.clone() {
            self.backend.steer(&id, job.text.clone());
        }
        self.lock().steering.push(job.text);
    }

    /// Report one prompt's outcome; prompts sent without an `id` are not reported.
    async fn prompt_result(&self, id: &Value, status: &str, error: Option<&str>) {
        if id.is_null() {
            return;
        }
        let settled = self.is_settled();
        let mut frame = json!({
            "type": "prompt_result",
            "id": id,
            "agentInvoked": true,
            "status": status,
            "sessionSettled": settled,
        });
        if let Some(error) = error {
            put(
                &mut frame,
                "error",
                json!({ "message": error, "retryable": false }),
            );
        }
        self.send(&frame).await;
    }

    /// The run loop: `first`, then the follow-up queue, until the session yields.
    async fn run_loop(self: Arc<Self>, generation: u64, first: Job) {
        let mut next = Some(first);
        let mut status = "completed";
        let mut error: Option<String> = None;
        while let Some(job) = next.take() {
            let cancel = self.lock().run.clone();
            let outcome = match cancel {
                Some(cancel) => self.one_turn(&job, &cancel).await,
                None => Err(HrnError::Cancelled),
            };
            if self.lock().generation != generation {
                self.finish_run();
                return;
            }
            match outcome {
                Ok(summary) => {
                    let mut state = self.lock();
                    state.last_run = Some((job.text.clone(), summary));
                    state.steering.clear();
                }
                Err(HrnError::Cancelled) => {
                    status = "aborted";
                    break;
                }
                Err(err) => {
                    status = "error";
                    error = Some(err.to_string());
                    break;
                }
            }
            next = self.lock().take_follow_ups().into_iter().next();
        }
        let (ids, dropped) = {
            let mut state = self.lock();
            state.run = None;
            state.run_active = false;
            let dropped = if status == "completed" {
                Vec::new()
            } else {
                let mut dropped = std::mem::take(&mut state.follow_up);
                dropped.append(&mut std::mem::take(&mut state.steering));
                dropped
            };
            (std::mem::take(&mut state.waiters), dropped)
        };
        for id in &ids {
            self.prompt_result(id, status, error.as_deref()).await;
        }
        self.idle.notify_waiters();
        if !dropped.is_empty() {
            for text in &dropped {
                self.send(&json!({
                    "type": "prompt_dropped", "message": text, "reason": status,
                }))
                .await;
            }
            self.sync_queue().await;
        }
        if self.is_settled() {
            self.send(&json!({ "type": "session_settled" })).await;
        }
    }

    /// The live run ended by a session change rather than by its own work.
    fn finish_run(&self) {
        let mut state = self.lock();
        state.run = None;
        state.run_active = false;
        state.waiters.clear();
        drop(state);
        self.idle.notify_waiters();
    }

    /// One model turn for `job`.
    async fn one_turn(self: &Arc<Self>, job: &Job, cancel: &CancelFlag) -> Result<TurnSummary> {
        self.sync()?;
        let session_id = self
            .session(true)?
            .ok_or_else(|| HrnError::Config("no session".to_owned()))?;
        let events: Arc<dyn EventSink> = self.sink();
        let approval: Option<Arc<dyn ApprovalGate>> = self
            .opts
            .approvals
            .then(|| Arc::new(Gate::new(Arc::clone(self))) as Arc<_>);
        let ask: Option<Arc<dyn AskHandler>> = self
            .opts
            .tool_ui
            .then(|| Arc::new(Asker::new(Arc::clone(self))) as Arc<_>);
        self.backend
            .run_turn(TurnRequest {
                session_id,
                prompt: job.text.clone(),
                images: job.images.clone(),
                cancel: cancel.clone(),
                events,
                approval,
                ask,
                hooks: Vec::new(),
            })
            .await
    }

    /// What a resume runs again: the prompt of the last run, else the newest user message.
    pub(crate) fn last_user_message(&self) -> Option<Job> {
        let remembered = self.lock().last_run.as_ref().map(|(text, _)| Job {
            id: None,
            text: text.clone(),
            images: Vec::new(),
        });
        remembered.or_else(|| {
            let nodes: Vec<DagNode> = self.branch().ok()?;
            nodes.iter().rev().find_map(|node| match &node.payload {
                DagPayload::Message { msg, .. } if msg.role == Role::User => Some(Job {
                    id: None,
                    text: msg.content.clone(),
                    images: msg.images.clone(),
                }),
                _ => None,
            })
        })
    }

    /// Cancel the live run, dropping everything queued, and wait for it to end.
    pub(crate) async fn abort_run(self: &Arc<Self>) {
        let (cancel, waiters, dropped) = {
            let mut state = self.lock();
            state.generation += 1;
            let mut dropped = std::mem::take(&mut state.follow_up);
            dropped.append(&mut std::mem::take(&mut state.steering));
            (
                state.run.take(),
                std::mem::take(&mut state.waiters),
                dropped,
            )
        };
        if let Some(flag) = cancel {
            flag.cancel();
            if let Some(id) = self.lock().session.clone() {
                self.backend.interrupt(&id);
            }
        }
        for id in &waiters {
            self.prompt_result(id, "aborted", None).await;
        }
        for text in &dropped {
            self.send(&json!({
                "type": "prompt_dropped", "message": text, "reason": "abort",
            }))
            .await;
        }
        self.sync_queue().await;
        while self.lock().run_active {
            self.idle.notified().await;
        }
    }
}
