//! Tool dialogs over RPC: approvals as `select` requests, `ask` forms as one `form` request,
//! both correlated through the connection's pending-dialog map.

use super::Core;
use crate::form_io;
use crate::wire::put;
use hrn_kernel::{
    ApprovalDecision, ApprovalGate, ApprovalGrantScope, ApprovalRequest, AskHandler, AskRequest,
    BoxFuture, CancelFlag, HrnError, PermissionMode, Result, Submission, ToolTier,
};

use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// How often a pending dialog re-checks its turn's cancellation.
const POLL: Duration = Duration::from_millis(100);

/// The number of diff lines an approval dialog shows.
const DIFF_LINES: usize = 40;

/// The approval dialog's labels, in answer order.
const APPROVE: &str = "Approve";
const APPROVE_FILE: &str = "Approve for this file";
const APPROVE_DIR: &str = "Approve for this directory";
const APPROVE_SESSION: &str = "Approve for this session";
const REMEMBER: &str = "Remember across sessions";
const DENY: &str = "Deny";

fn allowed_scopes(request: &ApprovalRequest, mode: PermissionMode) -> Vec<ApprovalGrantScope> {
    let mut out = Vec::new();
    if !request.choices.is_empty() {
        for &scope in &request.choices {
            if !out.contains(&scope) {
                out.push(scope);
            }
        }
    } else if mode == PermissionMode::AskCached {
        out.push(ApprovalGrantScope::Once);
        if !request.targets.is_empty() && request.tool_name != "bash" {
            out.push(ApprovalGrantScope::File);
            out.push(ApprovalGrantScope::Directory);
        }
        out.push(ApprovalGrantScope::ToolSession);
        out.push(ApprovalGrantScope::Persistent);
    } else {
        out.push(ApprovalGrantScope::Once);
        out.push(ApprovalGrantScope::ToolSession);
    }
    out
}

fn options_for(request: &ApprovalRequest, mode: PermissionMode) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = allowed_scopes(request, mode)
        .into_iter()
        .map(|scope| match scope {
            ApprovalGrantScope::Once => APPROVE,
            ApprovalGrantScope::File => APPROVE_FILE,
            ApprovalGrantScope::Directory => APPROVE_DIR,
            ApprovalGrantScope::ToolSession => APPROVE_SESSION,
            ApprovalGrantScope::Persistent => REMEMBER,
        })
        .collect();
    out.push(DENY);
    out
}

/// Send `frame` and wait for the host's answer to it. `None` when `cancel` fires first (the dialog
/// is cancelled on the host) or the connection stopped answering (stdin closed).
async fn ask_host(core: &Arc<Core>, mut frame: Value, cancel: &CancelFlag) -> Option<Value> {
    let (id, answer) = core.pending.register("ui_");
    put(&mut frame, "id", json!(id));
    core.send(&frame).await;
    let mut answer = std::pin::pin!(answer);
    loop {
        match tokio::time::timeout(POLL, answer.as_mut()).await {
            Ok(Ok(reply)) => return Some(reply),
            // The waiter was dropped: the connection is closing.
            Ok(Err(_)) => return None,
            Err(_) => {
                if cancel.is_cancelled() {
                    core.pending.forget(&id);
                    core.send(&json!({
                        "type": "extension_ui_request",
                        "method": "cancel",
                        "targetId": id,
                    }))
                    .await;
                    return None;
                }
            }
        }
    }
}

/// A diff short enough to read in a dialog: its leading lines.
fn trim_diff(diff: &str) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= DIFF_LINES {
        return diff.to_owned();
    }
    let Some(head) = lines.get(..DIFF_LINES) else {
        return diff.to_owned();
    };
    let mut text = head.join("\n");
    let _ = std::fmt::Write::write_fmt(
        &mut text,
        format_args!("\n… {} more diff lines", lines.len() - DIFF_LINES),
    );
    text
}

/// The approval gate behind an RPC session.
pub(crate) struct Gate {
    core: Arc<Core>,
}

impl Gate {
    pub(crate) fn new(core: Arc<Core>) -> Self {
        Self { core }
    }
}

impl ApprovalGate for Gate {
    fn await_decision(
        &self,
        request: ApprovalRequest,
        cancel: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>> {
        let core = Arc::clone(&self.core);
        Box::pin(async move {
            let plain_only = request.choices.as_slice() == [ApprovalGrantScope::Once];
            let req_tier = request.tier.unwrap_or(ToolTier::Read);
            let mode = {
                let state = core.lock();
                if !plain_only
                    && state
                        .grants
                        .get(&request.tool_name)
                        .is_some_and(|granted| granted.covers(req_tier))
                {
                    return Ok(ApprovalDecision::tool_session());
                }
                state.mode
            };
            let allowed = allowed_scopes(&request, mode);
            let message = match &request.diff {
                Some(diff) => format!("{}\n{}", request.summary, trim_diff(diff)),
                None => request.summary.clone(),
            };
            let options = options_for(&request, mode);
            let deny_index = options.len().saturating_sub(1);
            let mut frame = json!({
                "type": "extension_ui_request",
                "method": "select",
                "title": format!("Approve `{}`?", request.tool_name),
                "message": message,
                "options": options,
                "initialIndex": deny_index,
            });
            if let Some(reason) = &request.reason {
                put(&mut frame, "reason", json!(reason));
            }
            if !request.targets.is_empty() {
                put(&mut frame, "targets", json!(request.targets));
            }
            // No answer at all (abort, stdin closed) is a refusal, never an approval.
            let Some(reply) = ask_host(&core, frame, &cancel).await else {
                return Ok(ApprovalDecision::Deny);
            };
            if reply.get("cancelled").and_then(Value::as_bool) == Some(true) {
                return Ok(ApprovalDecision::Deny);
            }
            let value = reply.get("value").and_then(Value::as_str).unwrap_or("");
            let chosen = match value {
                APPROVE => Some(ApprovalGrantScope::Once),
                APPROVE_FILE => Some(ApprovalGrantScope::File),
                APPROVE_DIR => Some(ApprovalGrantScope::Directory),
                APPROVE_SESSION => Some(ApprovalGrantScope::ToolSession),
                REMEMBER => Some(ApprovalGrantScope::Persistent),
                _ => None,
            };
            let Some(scope) = chosen.filter(|s| allowed.contains(s)) else {
                return Ok(ApprovalDecision::Deny);
            };
            if scope == ApprovalGrantScope::ToolSession && !plain_only {
                let mut state = core.lock();
                let next = state
                    .grants
                    .get(&request.tool_name)
                    .map_or(req_tier, |prev| prev.stronger(req_tier));
                state.grants.insert(request.tool_name.clone(), next);
            }
            Ok(ApprovalDecision::Approve { scope })
        })
    }
}

/// The `ask` handler behind an RPC session.
pub(crate) struct Asker {
    core: Arc<Core>,
}

impl Asker {
    pub(crate) fn new(core: Arc<Core>) -> Self {
        Self { core }
    }
}

impl AskHandler for Asker {
    fn ask(&self, request: AskRequest, cancel: CancelFlag) -> BoxFuture<Result<Submission>> {
        let core = Arc::clone(&self.core);
        Box::pin(async move {
            let frame = json!({
                "type": "extension_ui_request",
                "method": "form",
                "html": form_io::to_html(&request.form),
            });
            let reply = ask_host(&core, frame, &cancel)
                .await
                // Nothing can answer any more: treat it as a cancellation.
                .unwrap_or_else(|| json!({ "cancelled": true }));
            if reply.get("cancelled").and_then(Value::as_bool) == Some(true) {
                if reply.get("timedOut").and_then(Value::as_bool) == Some(true) {
                    return Ok(Submission {
                        values: request.form.values(&request.form.defaults()),
                        timed_out: true,
                    });
                }
                return Err(HrnError::Tool("the user cancelled the form".to_owned()));
            }
            let data = reply.get("data").cloned().unwrap_or(Value::Null);
            form_io::submission(&request.form, &data)
                .map_err(|problem| HrnError::Tool(format!("invalid form answer: {problem}")))
        })
    }
}
