//! The ACP request loop: method dispatch, session bookkeeping and the prompt turn.

use super::client::{INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND, PARSE_ERROR};
use super::fs_hook::WriteThrough;
use super::shape::{join_blocks, stop_reason};
use super::update::Update;
use super::{AcpSession, Conn, PROTOCOL_VERSION};
use crate::acp::client::Gate;
use crate::backend::{NewSession, TurnRequest};
use hrn_kernel::{
    ApprovalGate, AskHandler, CancelFlag, HrnError, PermissionMode, Result, ThinkingLevel,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The frame version every reply carries.
const JSONRPC: &str = "2.0";

/// The ACP session id of a `sessionId` parameter.
fn session_id(params: &Value) -> Option<String> {
    params
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The `cwd` parameter as a path.
fn cwd_of(params: &Value) -> Option<PathBuf> {
    params
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// The ACP mode id a permission mode is advertised under.
const fn mode_id(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Plan => "plan",
        PermissionMode::Default => "default",
        PermissionMode::AskCached => "askCached",
        PermissionMode::AutoEdit => "acceptEdits",
        PermissionMode::Yolo => "yolo",
    }
}

/// The permission mode behind an ACP mode id.
fn mode_of(id: &str) -> Option<PermissionMode> {
    match id {
        "plan" => Some(PermissionMode::Plan),
        "default" => Some(PermissionMode::Default),
        "askCached" | "always-ask-cached" => Some(PermissionMode::AskCached),
        "acceptEdits" => Some(PermissionMode::AutoEdit),
        "yolo" => Some(PermissionMode::Yolo),
        _ => None,
    }
}

/// The `modes` block of a session response.
fn modes(current: PermissionMode) -> Value {
    json!({
        "currentModeId": mode_id(current),
        "availableModes": [
            { "id": "default", "name": "Default", "description": "Ask before mutating tools run" },
            { "id": "askCached", "name": "Always ask (cached)", "description": "Prompt reads/writes/execs with rememberable grants" },
            { "id": "acceptEdits", "name": "Accept edits", "description": "File edits run without asking" },
            { "id": "plan", "name": "Plan", "description": "Read-only: mutating tools are refused" },
            { "id": "yolo", "name": "YOLO", "description": "Everything runs without asking" },
        ],
    })
}

/// The `configOptions` block: model and thinking level selects.
fn config_options(conn: &Conn, model: &str, thinking: ThinkingLevel) -> Value {
    let models: Vec<Value> = conn
        .backend
        .models()
        .iter()
        .map(|m| json!({ "value": m.alias, "name": m.name }))
        .collect();
    let levels: Vec<Value> = [
        ThinkingLevel::Off,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
    ]
    .iter()
    .map(|l| json!({ "value": l.as_str(), "name": l.as_str() }))
    .collect();
    json!([
        { "id": "model", "name": "Model", "category": "model", "type": "select",
          "currentValue": model, "options": models },
        { "id": "thought_level", "name": "Thought level", "category": "thought_level",
          "type": "select", "currentValue": thinking.as_str(), "options": levels },
    ])
}

/// The `available_commands_update` params for `cwd`'s slash commands.
fn command_rows(conn: &Conn, cwd: &Path) -> Value {
    json!({
        "availableCommands": conn.backend.commands(cwd).iter().map(|c| json!({
            "name": c.name,
            "description": c.description,
            "input": { "hint": "[args]" },
        })).collect::<Vec<_>>(),
    })
}

/// `initialize`: the agent's capabilities.
fn initialize(conn: &Conn, params: &Value) -> Value {
    *Conn::lock(&conn.client_caps) = params
        .get("clientCapabilities")
        .cloned()
        .unwrap_or(Value::Null);
    *Conn::lock(&conn.initialized) = true;
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": { "image": true, "audio": false, "embeddedContext": true },
            "mcpCapabilities": { "http": false, "sse": false },
            "sessionCapabilities": { "list": {}, "close": {} },
        },
        "agentInfo": { "name": "hrn", "version": env!("CARGO_PKG_VERSION") },
        "authMethods": [],
    })
}

/// The settings of one ACP session.
fn settings_of(
    meta: &hrn_kernel::SessionMeta,
    fallback_model: String,
) -> (String, ThinkingLevel, PermissionMode) {
    (
        meta.model.clone().unwrap_or(fallback_model),
        meta.thinking,
        meta.permission_mode,
    )
}

impl Conn {
    /// The session `id` if this connection knows it.
    fn known(&self, id: &str) -> bool {
        Conn::lock(&self.sessions).contains_key(id)
    }

    /// One session's settings; `None` when the client named a session we do not have.
    fn settings(&self, id: &str) -> Option<(String, ThinkingLevel, PermissionMode, PathBuf)> {
        let sessions = Conn::lock(&self.sessions);
        sessions
            .get(id)
            .map(|s| (s.model.clone(), s.thinking, s.mode, s.cwd.clone()))
    }

    /// Apply `patch` to the ACP session's own view of it.
    fn apply(
        &self,
        id: &str,
        model: Option<String>,
        thinking: Option<ThinkingLevel>,
        mode: Option<PermissionMode>,
    ) {
        if let Some(state) = Conn::lock(&self.sessions).get_mut(id) {
            if let Some(model) = model {
                state.model = model;
            }
            if let Some(thinking) = thinking {
                state.thinking = thinking;
            }
            if let Some(mode) = mode {
                state.mode = mode;
            }
        }
    }

    /// The response body every session-shaped reply shares.
    fn session_body(&self, id: &str) -> Value {
        let (model, thinking, mode, _) = self.settings(id).unwrap_or_default();
        json!({
            "sessionId": id,
            "modes": modes(mode),
            "configOptions": config_options(self, &model, thinking),
        })
    }

    /// `session/new`, `session/load`, `session/resume`: create or adopt a conversation.
    async fn open_session(self: &Arc<Self>, method: &str, params: &Value) -> Result<Value> {
        let (default_model, default_thinking, default_mode) = self.startup_defaults();
        let cwd = cwd_of(params);
        let wanted = session_id(params);
        let (meta, replay) = match (method, wanted.as_deref()) {
            ("session/new", _) => (
                self.backend.create_session(NewSession {
                    cwd: cwd.clone().unwrap_or_else(|| self.opts.cwd.clone()),
                    title: String::new(),
                    model: default_model.clone(),
                    thinking: default_thinking,
                    mode: default_mode,
                })?,
                Vec::new(),
            ),
            ("session/load" | "session/resume", Some(id)) => {
                let meta = self.backend.find_session(id)?;
                let nodes = self.backend.history(&meta.id)?;
                (meta, nodes)
            }
            _ => {
                return Err(HrnError::Config(
                    "session/load needs a `sessionId`".to_owned(),
                ));
            }
        };
        let cwd = cwd.unwrap_or_else(|| PathBuf::from(&meta.cwd));
        let (model, thinking, mode) = settings_of(&meta, default_model);
        let session = AcpSession {
            cwd: cwd.clone(),
            model,
            thinking,
            mode,
            cancel: None,
            grants: std::collections::BTreeMap::new(),
            translator: Update::default(),
        };
        Conn::lock(&self.sessions).insert(meta.id.clone(), session);
        for frame in super::update::replay(&meta.id, &replay) {
            self.send(&frame).await;
        }
        self.send(&json!({
            "jsonrpc": JSONRPC,
            "method": "session/update",
            "params": {
                "sessionId": meta.id,
                "update": command_rows(self, &cwd),
            },
        }))
        .await;
        Ok(self.session_body(&meta.id))
    }

    /// The settings a new session starts with.
    fn startup_defaults(&self) -> (String, ThinkingLevel, PermissionMode) {
        let model = self
            .backend
            .default_model()
            .map(|m| m.alias)
            .unwrap_or_default();
        let mode = if self.opts.auto_approve {
            PermissionMode::Yolo
        } else {
            self.backend.default_mode()
        };
        (model, self.backend.default_thinking(), mode)
    }

    /// `session/prompt`: run one turn, streaming updates, answering with a `stopReason`.
    async fn prompt(self: &Arc<Self>, params: &Value) -> Result<Value> {
        let Some(id) = session_id(params) else {
            return Err(HrnError::Config("`sessionId` is required".to_owned()));
        };
        if !self.known(&id) {
            return Err(HrnError::Config(format!("unknown session `{id}`")));
        }
        if Conn::lock(&self.sessions)
            .get(&id)
            .is_some_and(|s| s.cancel.is_some())
        {
            return Err(HrnError::Config(
                "the session is already processing a prompt".to_owned(),
            ));
        }
        let blocks = params
            .get("prompt")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let prompt = join_blocks(&blocks);
        if prompt.trim().is_empty() {
            return Err(HrnError::Config("`prompt` must carry text".to_owned()));
        }
        let images: Vec<hrn_kernel::ImageAttachment> = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
            .map(|block| hrn_kernel::ImageAttachment {
                media_type: block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png")
                    .to_owned(),
                data_base64: block
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                label: None,
                width: None,
                height: None,
            })
            .collect();
        self.sync(&id)?;
        let cancel = CancelFlag::new();
        let cwd = self
            .settings(&id)
            .map_or_else(|| self.opts.cwd.clone(), |s| s.3);
        {
            let mut sessions = Conn::lock(&self.sessions);
            if let Some(state) = sessions.get_mut(&id) {
                state.cancel = Some(cancel.clone());
                state.translator = Update::default();
            }
        }
        // Attach the permission gate & ask handler only when interactive UI is enabled (`!no_ui`);
        // headless `hrn acp --no-ui` passes `None` so gated calls fail closed cleanly.
        let approval: Option<Arc<dyn ApprovalGate>> = self
            .opts
            .approvals
            .then(|| Arc::new(Gate::new(Arc::clone(self), id.clone())) as Arc<_>);
        let ask: Option<Arc<dyn AskHandler>> = self
            .opts
            .approvals
            .then(|| Arc::new(super::client::Asker::new(Arc::clone(self), id.clone())) as Arc<_>);
        let hooks = vec![Arc::new(WriteThrough::new(
            Arc::clone(self),
            id.clone(),
            cwd,
            cancel.clone(),
        )) as Arc<_>];
        let outcome = self
            .backend
            .run_turn(TurnRequest {
                session_id: id.clone(),
                prompt,
                images,
                cancel,
                events: self.sink(&id),
                approval,
                ask,
                hooks,
            })
            .await;
        if let Some(state) = Conn::lock(&self.sessions).get_mut(&id) {
            state.cancel = None;
        }
        match outcome {
            Ok(summary) => Ok(json!({ "stopReason": stop_reason(&summary) })),
            Err(HrnError::Cancelled) => Ok(json!({ "stopReason": "cancelled" })),
            Err(err) => Err(err),
        }
    }

    /// The stored session's settings follow the ACP session's own view of them.
    fn sync(&self, id: &str) -> Result {
        let Some((model, thinking, mode, _)) = self.settings(id) else {
            return Ok(());
        };
        self.backend.patch_session(
            id,
            crate::backend::SessionPatch {
                model: Some(model),
                thinking: Some(thinking),
                mode: Some(mode),
                title: None,
            },
        )?;
        Ok(())
    }

    /// `session/cancel`: stop the live turn of `sessionId`.
    fn cancel(&self, params: &Value) {
        if let Some(id) = session_id(params) {
            if let Some(cancel) = Conn::lock(&self.sessions)
                .get(&id)
                .and_then(|s| s.cancel.clone())
            {
                cancel.cancel();
            }
            self.backend.interrupt(&id);
        }
    }

    /// `session/set_mode` and `session/set_config_option`.
    fn set_option(&self, method: &str, params: &Value) -> Result<Value> {
        let Some(id) = session_id(params) else {
            return Err(HrnError::Config("`sessionId` is required".to_owned()));
        };
        if !self.known(&id) {
            return Err(HrnError::Config(format!("unknown session `{id}`")));
        }
        let mut patch = crate::backend::SessionPatch::default();
        match method {
            "session/set_mode" => {
                let wanted = params
                    .get("modeId")
                    .and_then(Value::as_str)
                    .and_then(mode_of)
                    .ok_or_else(|| HrnError::Config("`modeId` is unknown".to_owned()))?;
                self.apply(&id, None, None, Some(wanted));
                patch.mode = Some(wanted);
            }
            "session/set_config_option" => {
                let config_id = params.get("configId").and_then(Value::as_str).unwrap_or("");
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| HrnError::Config("`value` must be a string".to_owned()))?;
                match config_id {
                    "model" => {
                        let model = self
                            .backend
                            .find_model(value)
                            .map_err(|_| HrnError::Config(format!("Model not found: {value}")))?;
                        self.apply(&id, Some(model.alias.clone()), None, None);
                        patch.model = Some(model.alias);
                    }
                    "thought_level" => {
                        let level = ThinkingLevel::parse(value).ok_or_else(|| {
                            HrnError::Config(format!("`{value}` is not a thinking level"))
                        })?;
                        self.apply(&id, None, Some(level), None);
                        patch.thinking = Some(level);
                    }
                    other => {
                        return Err(HrnError::Config(format!(
                            "`{other}` is not a configurable option of this agent"
                        )));
                    }
                }
            }
            _ => return Err(HrnError::Config("unknown option method".to_owned())),
        }
        self.backend.patch_session(&id, patch)?;
        Ok(self.session_body(&id))
    }

    /// `session/list`: hrn sessions, newest first, optionally limited to one directory.
    fn list_sessions(&self, params: &Value) -> Result<Value> {
        let cwd = cwd_of(params);
        let sessions: Vec<Value> = self
            .backend
            .list_sessions()?
            .into_iter()
            .filter(|s| {
                cwd.as_ref()
                    .is_none_or(|want| Path::new(&s.cwd) == want.as_path())
            })
            .map(|s| {
                json!({
                    "sessionId": s.id,
                    "cwd": s.cwd,
                    "title": s.title,
                    "updatedAt": format!("{:013}", s.updated_at_ms),
                })
            })
            .collect();
        Ok(json!({ "sessions": sessions, "nextCursor": Value::Null }))
    }

    /// `session/close`: cancel the live turn and forget the ACP handle; the transcript stays.
    fn close_session(&self, params: &Value) {
        if let Some(id) = session_id(params) {
            if let Some(cancel) = Conn::lock(&self.sessions)
                .get(&id)
                .and_then(|s| s.cancel.clone())
            {
                cancel.cancel();
            }
            Conn::lock(&self.sessions).remove(&id);
        }
    }

    /// `session/set_session_info`: rename the conversation.
    async fn rename(&self, params: &Value) -> Result<Value> {
        let Some(id) = session_id(params) else {
            return Err(HrnError::Config("`sessionId` is required".to_owned()));
        };
        let title = params
            .get("title")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(str::trim)
            .ok_or_else(|| HrnError::Config("`title` must be a non-empty string".to_owned()))?;
        self.backend.patch_session(
            &id,
            crate::backend::SessionPatch {
                title: Some(title.to_owned()),
                ..Default::default()
            },
        )?;
        let frame = json!({
            "jsonrpc": JSONRPC,
            "method": "session/update",
            "params": {
                "sessionId": id,
                "update": { "sessionUpdate": "session_info_update", "title": title },
            },
        });
        self.send(&frame).await;
        Ok(json!({}))
    }
}
/// One inbound line: a request, a reply to one of our requests, or a notification.
pub(crate) async fn handle_line(conn: &Arc<Conn>, line: &str) {
    let frame: Value = match serde_json::from_str::<Value>(line) {
        Ok(frame) if frame.is_object() => frame,
        Ok(_) => {
            conn.reply_err(None, PARSE_ERROR, "expected a JSON object")
                .await;
            return;
        }
        Err(err) => {
            conn.reply_err(None, PARSE_ERROR, &format!("invalid JSON: {err}"))
                .await;
            return;
        }
    };
    let id = frame.get("id").filter(|v| !v.is_null()).cloned();
    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
    let params = frame.get("params").cloned().unwrap_or(Value::Null);
    // A response to a request we sent: hand it to its waiter, no reply of its own.
    if method.is_empty() {
        if let Some(id) = id.as_ref() {
            conn.resolve(id, frame);
        } else {
            conn.reply_err(None, INVALID_PARAMS, "a frame needs `method` or `id`")
                .await;
        }
        return;
    }
    let result = match method {
        "initialize" => Ok(initialize(conn, &params)),
        "session/new" | "session/load" | "session/resume" => {
            conn.open_session(method, &params).await
        }
        "session/prompt" => conn.prompt(&params).await,
        "session/list" => conn.list_sessions(&params),
        "session/set_mode" | "session/set_config_option" => conn.set_option(method, &params),
        "session/set_session_info" => Conn::rename(conn, &params).await,
        "session/close" => {
            conn.close_session(&params);
            Ok(json!({}))
        }
        "session/forget_approvals" | "forget_approvals" => {
            for state in Conn::lock(&conn.sessions).values_mut() {
                state.grants.clear();
            }
            conn.backend.forget_approvals().map(|()| json!({}))
        }
        "session/cancel" | "didChange" | "logging/setLevel" => {
            if method == "session/cancel" {
                conn.cancel(&params);
            }
            Ok(Value::Null)
        }
        unknown => {
            if let Some(id) = id {
                conn.reply_err(
                    Some(id),
                    METHOD_NOT_FOUND,
                    &format!("unknown method `{unknown}`"),
                )
                .await;
            }
            return;
        }
    };
    let (value, Some(id)) = (result, id) else {
        return;
    };
    match value {
        Ok(value) => conn.reply(id, value).await,
        Err(err) => {
            conn.reply_err(Some(id), INTERNAL_ERROR, &err.to_string())
                .await;
        }
    }
}
