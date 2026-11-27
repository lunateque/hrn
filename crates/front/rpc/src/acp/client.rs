//! Requests the agent sends to the client: permission cards, `ask` elicitation, file writes,
//! and their cancellation.

use super::{AcpSession, Conn};
use crate::form_io;
use crate::wire::id_key;
use crate::wire::put;
use hrn_kernel::{
    AgentEvent, ApprovalDecision, ApprovalGate, ApprovalGrantScope, ApprovalRequest, AskHandler,
    AskRequest, BoxFuture, CancelFlag, EventSink, HrnError, PermissionMode, Result, Submission,
    ToolTier,
};

use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// How often a pending client request re-checks its turn's cancellation.
const POLL: Duration = Duration::from_millis(100);

/// JSON-RPC codes the agent reports to the client.
pub(crate) const PARSE_ERROR: i64 = -32700;
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const INTERNAL_ERROR: i64 = -32603;

impl Conn {
    /// Send `frame` with `id` and wait for the client's answer; `None` when the connection is
    /// closing or `cancel` fired first (the request is cancelled on the client).
    pub(crate) async fn request(
        self: &Arc<Self>,
        mut frame: Value,
        cancel: &CancelFlag,
    ) -> Option<Value> {
        let (id, answer) = self.pending.register("req-");
        put(&mut frame, "id", json!(id));
        self.send(&frame).await;
        let mut answer = std::pin::pin!(answer);
        loop {
            match tokio::time::timeout(POLL, answer.as_mut()).await {
                Ok(Ok(reply)) => return Some(reply),
                Ok(Err(_)) => return None,
                Err(_) => {
                    if cancel.is_cancelled() {
                        self.pending.forget(&id);
                        self.send(&json!({
                            "jsonrpc": "2.0",
                            "method": "$/cancel_request",
                            "params": { "requestId": id },
                        }))
                        .await;
                        return None;
                    }
                }
            }
        }
    }

    /// Whether the client advertised `clientCapabilities.<path>`, deeply.
    pub(crate) fn client_has(&self, path: &[&str]) -> bool {
        let caps = Conn::lock(&self.client_caps);
        let mut node: &Value = &caps;
        for key in path {
            node = match node.get(*key) {
                Some(next) => next,
                None => return false,
            };
        }
        matches!(node, Value::Bool(true) | Value::Object(_))
    }

    /// Hand the client's reply to whoever is waiting on `id`.
    pub(crate) fn resolve(&self, id: &Value, answer: Value) -> bool {
        self.pending.resolve(&id_key(id), answer)
    }

    /// `fs/write_text_file`: ask the client to replace a whole file. `Ok(false)` when the client
    /// cannot or will not write it.
    pub(crate) async fn write_text_file(
        self: &Arc<Self>,
        session_id: &str,
        path: &str,
        content: &str,
        cancel: &CancelFlag,
    ) -> Option<Value> {
        self.request(
            json!({
                "jsonrpc": "2.0",
                "method": "fs/write_text_file",
                "params": { "sessionId": session_id, "path": path, "content": content },
            }),
            cancel,
        )
        .await
    }
}

/// The approval gate of an ACP session.
pub(crate) struct Gate {
    conn: Arc<Conn>,
    session: String,
}

impl Gate {
    pub(crate) fn new(conn: Arc<Conn>, session: String) -> Self {
        Self { conn, session }
    }
}

fn allowed_scopes(request: &ApprovalRequest, mode: PermissionMode) -> Vec<ApprovalGrantScope> {
    if !request.choices.is_empty() {
        let mut out = Vec::new();
        for &scope in &request.choices {
            if !out.contains(&scope) {
                out.push(scope);
            }
        }
        return out;
    }
    if mode == PermissionMode::AskCached {
        let mut s = vec![ApprovalGrantScope::Once];
        if !request.targets.is_empty() && request.tool_name != "bash" {
            s.push(ApprovalGrantScope::File);
            s.push(ApprovalGrantScope::Directory);
        }
        s.push(ApprovalGrantScope::ToolSession);
        s.push(ApprovalGrantScope::Persistent);
        s
    } else {
        vec![ApprovalGrantScope::Once, ApprovalGrantScope::ToolSession]
    }
}

fn acp_options(scopes: &[ApprovalGrantScope]) -> Vec<Value> {
    let mut out: Vec<Value> = scopes
        .iter()
        .map(|scope| match scope {
            ApprovalGrantScope::Once => {
                json!({ "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" })
            }
            ApprovalGrantScope::File => {
                json!({ "optionId": "allow_file", "name": "Allow this file", "kind": "allow_always" })
            }
            ApprovalGrantScope::Directory => {
                json!({ "optionId": "allow_directory", "name": "Allow this directory", "kind": "allow_always" })
            }
            ApprovalGrantScope::ToolSession => {
                json!({ "optionId": "allow_always", "name": "Always allow", "kind": "allow_always" })
            }
            ApprovalGrantScope::Persistent => {
                json!({ "optionId": "allow_remember", "name": "Remember across sessions", "kind": "allow_always" })
            }
        })
        .collect();
    out.push(json!({ "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }));
    out
}

impl ApprovalGate for Gate {
    fn await_decision(
        &self,
        request: ApprovalRequest,
        cancel: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>> {
        let conn = Arc::clone(&self.conn);
        let session = self.session.clone();
        Box::pin(async move {
            let plain_only = request.choices.as_slice() == [ApprovalGrantScope::Once];
            let req_tier = request.tier.unwrap_or(ToolTier::Read);
            let mode = {
                let sessions = Conn::lock(&conn.sessions);
                let state = sessions.get(&session);
                if !plain_only
                    && state.is_some_and(|s: &AcpSession| {
                        s.grants
                            .get(&request.tool_name)
                            .is_some_and(|granted| granted.covers(req_tier))
                    })
                {
                    return Ok(ApprovalDecision::tool_session());
                }
                state.map_or(PermissionMode::Default, |s| s.mode)
            };
            let allowed = allowed_scopes(&request, mode);
            let title = match &request.reason {
                Some(reason) => format!("{} {} ({reason})", request.tool_name, request.summary),
                None => format!("{} {}", request.tool_name, request.summary),
            };
            let mut update = json!({
                "toolCallId": request.id,
                "title": title,
                "kind": crate::acp::update::tool_kind(&request.tool_name),
                "status": "pending",
            });
            if let Some(diff) = &request.diff {
                put(
                    &mut update,
                    "content",
                    json!([{ "type": "content", "content": { "type": "text", "text": diff } }]),
                );
            }
            let frame = json!({
                "jsonrpc": "2.0",
                "method": "session/request_permission",
                "params": {
                    "sessionId": session,
                    "toolCall": update,
                    "options": acp_options(&allowed),
                },
            });
            let Some(reply) = Conn::request(&conn, frame, &cancel).await else {
                return Ok(ApprovalDecision::Deny);
            };
            let outcome = reply.get("outcome").cloned().unwrap_or(Value::Null);
            if outcome.get("outcome").and_then(Value::as_str) == Some("cancelled") {
                return Ok(ApprovalDecision::Deny);
            }
            let selected = outcome
                .get("optionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let chosen = match selected {
                "allow_once" => Some(ApprovalGrantScope::Once),
                "allow_file" => Some(ApprovalGrantScope::File),
                "allow_directory" => Some(ApprovalGrantScope::Directory),
                "allow_always" => Some(ApprovalGrantScope::ToolSession),
                "allow_remember" => Some(ApprovalGrantScope::Persistent),
                _ => None,
            };
            let Some(scope) = chosen.filter(|s| allowed.contains(s)) else {
                return Ok(ApprovalDecision::Deny);
            };
            if scope == ApprovalGrantScope::ToolSession
                && !plain_only
                && let Some(state) = Conn::lock(&conn.sessions).get_mut(&session)
            {
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

/// The `ask` handler of an ACP session, answered through client elicitation.
pub(crate) struct Asker {
    conn: Arc<Conn>,
    session: String,
}

impl Asker {
    pub(crate) fn new(conn: Arc<Conn>, session: String) -> Self {
        Self { conn, session }
    }
}

impl AskHandler for Asker {
    fn ask(&self, request: AskRequest, cancel: CancelFlag) -> BoxFuture<Result<Submission>> {
        let conn = Arc::clone(&self.conn);
        let session = self.session.clone();
        Box::pin(async move {
            if !conn.client_has(&["elicitation", "form"]) {
                return Err(HrnError::Tool(
                    "this ACP client does not support elicitation, so no one can answer the form"
                        .to_owned(),
                ));
            }
            let frame = json!({
                "jsonrpc": "2.0",
                "method": "elicitation/create",
                "params": {
                    "mode": "form",
                    "sessionId": session,
                    "message": form_io::context(&request.form),
                    "requestedSchema": form_io::to_schema(&request.form),
                },
            });
            let Some(reply) = Conn::request(&conn, frame, &cancel).await else {
                return Err(HrnError::Cancelled);
            };
            match reply.get("action").and_then(Value::as_str) {
                Some("accept") => {
                    let content = reply.get("content").cloned().unwrap_or(Value::Null);
                    form_io::submission(&request.form, &content).map_err(|problem| {
                        HrnError::Tool(format!("invalid elicitation answer: {problem}"))
                    })
                }
                Some("decline") => Err(HrnError::Tool("the user declined the form".to_owned())),
                _ => Err(HrnError::Tool("the user cancelled the form".to_owned())),
            }
        })
    }
}

/// The sink of a live ACP prompt turn: translated events go out as `session/update`.
struct Updates {
    conn: Arc<Conn>,
    session: String,
}

impl EventSink for Updates {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        let conn = Arc::clone(&self.conn);
        let session = self.session.clone();
        Box::pin(async move {
            let frames = {
                let mut sessions = Conn::lock(&conn.sessions);
                match sessions.get_mut(&session) {
                    Some(state) => state
                        .translator
                        .handle(&event)
                        .into_iter()
                        .map(|update| crate::acp::update::frame(&session, &update))
                        .collect(),
                    None => Vec::new(),
                }
            };
            for frame in frames {
                conn.send(&frame).await;
            }
            true
        })
    }
}

impl Conn {
    /// The event sink of `session`.
    pub(crate) fn sink(self: &Arc<Self>, session: &str) -> Arc<dyn EventSink> {
        Arc::new(Updates {
            conn: Arc::clone(self),
            session: session.to_owned(),
        })
    }
}
