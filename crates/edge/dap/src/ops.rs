//! What a session does once it runs: resume, pause, inspect, shut down.

use crate::client::DapClient;
use crate::session::{Outcome, Resume, RunState, Session, Summary};
use crate::state::{Status, Stop};
use hrn_kernel::{HrnError, Result};
use serde_json::{Value, json};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

/// How long after a resume or pause we look for the stop it causes before reporting the program
/// as running, at most.
const STOP_CAPTURE: Duration = Duration::from_secs(5);
/// Longest wait for the stack frame of a stop and for each step of a shutdown.
const SHORT_REQUEST: Duration = Duration::from_secs(5);

impl Session {
    /// Where the program is now: fetch the top frame if stopped.
    pub(crate) async fn outcome(&self, timed_out: bool) -> Outcome {
        let mut notes = Vec::new();
        let stopped = self.shared.read(|s| {
            s.active_target()
                .filter(|t| t.status == Status::Stopped)
                .map(|t| t.id.clone())
        });
        if let Some(id) = stopped
            && let Err(e) = self.fetch_top_frame(&id).await
        {
            notes.push(format!("could not read the stop location: {e}"));
        }
        let summary = self.summary();
        let state = match summary.status {
            "stopped" => RunState::Stopped,
            "terminated" => RunState::Terminated,
            _ => RunState::Running,
        };
        Outcome {
            timed_out: timed_out && state == RunState::Running,
            summary,
            state,
            notes,
        }
    }

    async fn fetch_top_frame(&self, id: &str) -> Result {
        let (client, thread) = self.shared.read(|s| {
            s.target(id)
                .map_or((None, None), |t| (t.client.clone(), t.stop.thread_id))
        });
        let (Some(client), Some(thread)) = (client, thread) else {
            return Ok(());
        };
        let body = client
            .request(
                "stackTrace",
                json!({"threadId": thread, "levels": 1}),
                SHORT_REQUEST.min(self.timeout),
            )
            .await?;
        self.apply_top_frame(id, body.get("stackFrames").and_then(|f| f.get(0)));
        Ok(())
    }

    fn apply_top_frame(&self, id: &str, frame: Option<&Value>) {
        let Some(frame) = frame else { return };
        self.shared.with(|s| {
            if let Some(t) = s.target_mut(id) {
                let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
                t.stop.frame_id = frame.get("id").and_then(Value::as_i64);
                t.stop.frame_name = text(frame.get("name"));
                t.stop.instruction_pointer = text(frame.get("instructionPointerReference"));
                t.stop.source_path = text(frame.pointer("/source/path"))
                    .or_else(|| text(frame.pointer("/source/name")));
                t.stop.line = frame.get("line").and_then(Value::as_i64);
                t.stop.column = frame.get("column").and_then(Value::as_i64);
            }
        });
    }

    async fn resolve_thread(&self, id: &str, client: &DapClient, timeout: Duration) -> Result<i64> {
        let known = self.shared.read(|s| {
            s.target(id).and_then(|t| {
                t.stop.thread_id.or_else(|| {
                    t.threads
                        .first()
                        .and_then(|th| th.get("id"))
                        .and_then(Value::as_i64)
                })
            })
        });
        if let Some(thread) = known {
            return Ok(thread);
        }
        let body = client.request("threads", Value::Null, timeout).await?;
        let threads = body
            .get("threads")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let first = threads
            .first()
            .and_then(|t| t.get("id"))
            .and_then(Value::as_i64);
        self.shared.with(|s| {
            if let Some(t) = s.target_mut(id) {
                t.threads = threads;
            }
        });
        first.ok_or_else(|| HrnError::Tool("Debugger reported no threads.".into()))
    }

    /// `continue` or a step, then wait for the next stop.
    ///
    /// # Errors
    ///
    /// No session, or the adapter refuses the request outright.
    pub async fn resume(&self, how: Resume, timeout: Duration) -> Result<Outcome> {
        let (id, client) = self.active()?;
        self.ensure_configured(&id, &client, timeout).await?;
        let thread = self.resolve_thread(&id, &client, timeout).await?;
        // Reset before sending: a stop that races the response must not be mistaken for old state.
        self.shared.with(|s| {
            if let Some(t) = s.target_mut(&id) {
                t.status = Status::Running;
                t.stop = Stop::default();
            }
        });
        // Many adapters answer a resume only when the program next stops, and queue later
        // requests behind that answer: the stop event, not the response, is what this call is
        // about. A refusal arrives quickly and is still reported.
        let command = how.command();
        let response = tokio::spawn(async move {
            client
                .request(command, json!({"threadId": thread}), timeout * 2)
                .await
        });
        let reached = self
            .shared
            .wait_for(timeout, |s| {
                (s.any_stopped() || s.all_terminated()).then_some(())
            })
            .await
            .is_some();
        if reached {
            // The adapter answers with the stop; the answer itself carries nothing we need.
            response.abort();
        } else if let Ok(joined) = tokio::time::timeout(SHORT_REQUEST, response).await {
            // A refusal shows up right away; no answer means the program is running and the
            // adapter will answer the request whenever execution next halts.
            joined.map_err(|e| HrnError::Internal(format!("DAP resume task: {e}")))??;
        }
        Ok(self.outcome(!reached).await)
    }

    /// Interrupt a running program (a stopped one is reported as is).
    ///
    /// # Errors
    ///
    /// No session, or the adapter refuses.
    pub async fn pause(&self, timeout: Duration) -> Result<Outcome> {
        let (id, client) = self.active()?;
        if self
            .shared
            .read(|s| s.target(&id).is_some_and(|t| t.status == Status::Stopped))
        {
            return Ok(self.outcome(false).await);
        }
        let thread = self.resolve_thread(&id, &client, timeout).await?;
        client
            .request("pause", json!({"threadId": thread}), timeout)
            .await?;
        let reached = self
            .shared
            .wait_for(timeout.min(STOP_CAPTURE), |s| s.any_stopped().then_some(()))
            .await;
        Ok(self.outcome(reached.is_none()).await)
    }

    /// Frames of the focused thread; also refreshes the stop location.
    ///
    /// # Errors
    ///
    /// No session, or the adapter rejects the request.
    pub async fn stack_trace(&self, levels: Option<u64>, timeout: Duration) -> Result<Vec<Value>> {
        let (id, client) = self.active()?;
        self.ensure_configured(&id, &client, timeout).await?;
        let thread = self.resolve_thread(&id, &client, timeout).await?;
        let mut args = json!({"threadId": thread});
        if let Some(levels) = levels
            && let Some(map) = args.as_object_mut()
        {
            map.insert("levels".to_owned(), json!(levels));
        }
        let body = client.request("stackTrace", args, timeout).await?;
        let frames = body
            .get("stackFrames")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.apply_top_frame(&id, frames.first());
        Ok(frames)
    }

    /// Threads of every live target (ids repeat across targets, so each carries its `target`).
    ///
    /// # Errors
    ///
    /// No session. A target that fails to answer is reported in the second element.
    pub async fn threads(&self, timeout: Duration) -> Result<(Vec<Value>, Vec<String>)> {
        self.active()?;
        let live: Vec<(String, Arc<DapClient>)> = self.shared.read(|s| {
            s.targets
                .iter()
                .filter(|t| t.status != Status::Terminated)
                .filter_map(|t| t.client.clone().map(|c| (t.id.clone(), c)))
                .collect()
        });
        let (mut merged, mut warnings) = (Vec::new(), Vec::new());
        for (id, client) in &live {
            if let Err(e) = self.ensure_configured(id, client, timeout).await {
                warnings.push(format!("{id}: {e}"));
                continue;
            }
            match client.request("threads", Value::Null, timeout).await {
                Ok(body) => {
                    let threads = body
                        .get("threads")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    for mut thread in threads.iter().cloned() {
                        if live.len() > 1
                            && let Some(map) = thread.as_object_mut()
                        {
                            map.insert("target".to_owned(), json!(id));
                        }
                        merged.push(thread);
                    }
                    self.shared.with(|s| {
                        if let Some(t) = s.target_mut(id) {
                            t.threads = threads;
                        }
                    });
                }
                Err(e) => warnings.push(format!("{id}: {e}")),
            }
        }
        Ok((merged, warnings))
    }

    /// Any DAP request against the focused target (after `configurationDone`).
    ///
    /// # Errors
    ///
    /// No session, or the adapter rejects the request.
    pub async fn request(&self, command: &str, args: Value, timeout: Duration) -> Result<Value> {
        let (id, client) = self.active()?;
        self.ensure_configured(&id, &client, timeout).await?;
        client.request(command, args, timeout).await
    }

    /// End the debuggee (if launched) and every connection; problems of the shutdown come back
    /// as notes. The session is unusable afterwards.
    pub async fn terminate(&self) -> (Summary, Vec<String>) {
        let before = self.summary();
        let targets: Vec<(String, Arc<DapClient>, bool)> = self.shared.read(|s| {
            s.targets
                .iter()
                .rev()
                .filter_map(|t| {
                    t.client
                        .clone()
                        .map(|c| (t.id.clone(), c, t.supports("supportsTerminateRequest")))
                })
                .collect()
        });
        let mut notes = Vec::new();
        for (id, client, can_terminate) in targets {
            if !client.is_alive() {
                continue;
            }
            if can_terminate
                && self.launched
                && let Err(e) = client
                    .request("terminate", Value::Null, SHORT_REQUEST)
                    .await
            {
                notes.push(format!("{id}: terminate: {e}"));
            }
            if client.is_alive()
                && let Err(e) = client
                    .request(
                        "disconnect",
                        json!({"terminateDebuggee": self.launched}),
                        SHORT_REQUEST,
                    )
                    .await
            {
                notes.push(format!("{id}: disconnect: {e}"));
            }
        }
        self.dispose();
        (before, notes)
    }

    /// Drop every connection and spawned process now.
    pub fn dispose(&self) {
        let clients: Vec<Arc<DapClient>> = self.shared.with(|s| {
            let ids: Vec<String> = s.targets.iter().map(|t| t.id.clone()).collect();
            for id in ids {
                s.end(&id);
            }
            s.targets
                .iter_mut()
                .filter_map(|t| t.client.take())
                .collect()
        });
        drop(clients);
        self.terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}
