//! Per-kernel loopback HTTP/1.1 bridge (`127.0.0.1:0`) authenticated with a 256-bit OS CSPRNG
//! bearer token compared in constant time.
//!
//! Endpoints:
//! - `POST /tool` (`{ name, args }`)
//! - `POST /agent/spawn` (`{ prompt, opts }`)
//! - `POST /agent/wait` (`{ ids, timeout_ms }`)
//! - `POST /complete` (`{ prompt, role }`)
//! - `POST /register_tool` (`{ name, description, parameters }`)
//! - `POST /watchdog/pause`
//! - `POST /watchdog/resume`

use crate::bridge_http::{HttpResponse, read_request, write_response};
use crate::dyn_tools::KernelToolStore;
use crate::lang::Lang;
use crate::pool::WeakKernelSlot;
use hrn_edge_ipc::generate_token;
use hrn_edge_rt::tokio;
use hrn_edge_tools::EvalRuntimeHints;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{EvalHost, HrnError, ImageAttachment, Result, ToolContext, ToolOutput, ToolSpec};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinHandle;

/// Maximum concurrent in-flight bridge connections per kernel.
const MAX_CONCURRENT_CONNS: usize = 64;

/// Refcounted pause state for a kernel's cell timeout watchdog.
#[derive(Default)]
pub(crate) struct WatchdogState {
    depth: Mutex<usize>,
    notify: Notify,
}

impl WatchdogState {
    /// Increment pause refcount and notify any active cell timer.
    pub(crate) fn pause(&self) -> usize {
        let mut depth = self.depth.lock().unwrap_or_else(PoisonError::into_inner);
        *depth = depth.saturating_add(1);
        let cur = *depth;
        drop(depth);
        self.notify.notify_waiters();
        cur
    }

    /// Decrement pause refcount and notify any active cell timer.
    pub(crate) fn resume(&self) -> usize {
        let mut depth = self.depth.lock().unwrap_or_else(PoisonError::into_inner);
        *depth = depth.saturating_sub(1);
        let cur = *depth;
        drop(depth);
        self.notify.notify_waiters();
        cur
    }

    /// Reset pause refcount to 0 between cells.
    pub(crate) fn reset(&self) {
        let mut depth = self.depth.lock().unwrap_or_else(PoisonError::into_inner);
        *depth = 0;
        drop(depth);
        self.notify.notify_waiters();
    }

    /// Whether the watchdog is currently paused.
    pub(crate) fn is_paused(&self) -> bool {
        *self.depth.lock().unwrap_or_else(PoisonError::into_inner) > 0
    }

    /// Future resolving when pause state changes.
    pub(crate) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    /// RAII pause guard that resumes on drop.
    pub(crate) fn pause_guard(self: &Arc<Self>) -> WatchdogPauseGuard {
        let _ = self.pause();
        WatchdogPauseGuard {
            state: Arc::clone(self),
        }
    }
}

/// RAII guard resuming [`WatchdogState`] on drop.
pub(crate) struct WatchdogPauseGuard {
    state: Arc<WatchdogState>,
}

impl Drop for WatchdogPauseGuard {
    fn drop(&mut self) {
        let _ = self.state.resume();
    }
}

struct ActiveCall {
    ctx: ToolContext,
    images: Vec<ImageAttachment>,
}

/// RAII scope installing the active cell's [`ToolContext`] on a kernel bridge.
pub(crate) struct ActiveCallGuard {
    shared: Arc<BridgeShared>,
    finished: bool,
}

impl ActiveCallGuard {
    /// End the active cell scope and return images collected from `tool.*` / `computer.*` calls.
    pub(crate) fn finish(mut self) -> Vec<ImageAttachment> {
        self.finished = true;
        self.shared.watchdog.reset();
        self.shared
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .map_or_else(Vec::new, |call| call.images)
    }
}

impl Drop for ActiveCallGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.shared.watchdog.reset();
            let _ = self
                .shared
                .active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
        }
    }
}

/// Parameters required to start a per-kernel [`KernelBridge`].
pub(crate) struct BridgeInit {
    pub eval_host: Option<Arc<dyn EvalHost>>,
    pub dyn_tools: Arc<KernelToolStore>,
    pub hints: Arc<EvalRuntimeHints>,
    pub tools_enabled: bool,
    pub slot: WeakKernelSlot,
    pub lang: Lang,
}

struct BridgeShared {
    token: String,
    eval_host: Option<Arc<dyn EvalHost>>,
    dyn_tools: Arc<KernelToolStore>,
    hints: Arc<EvalRuntimeHints>,
    tools_enabled: bool,
    slot: WeakKernelSlot,
    lang: Lang,
    watchdog: Arc<WatchdogState>,
    active: Mutex<Option<ActiveCall>>,
    conn_sem: Arc<Semaphore>,
}

/// Per-kernel authenticated loopback HTTP bridge. Aborts its accept loop when dropped.
pub(crate) struct KernelBridge {
    addr: SocketAddr,
    token: String,
    shared: Arc<BridgeShared>,
    server_task: JoinHandle<()>,
}

impl KernelBridge {
    /// Bind `127.0.0.1:0` and launch the per-kernel HTTP/1.1 bridge task.
    pub(crate) async fn start(init: BridgeInit) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| {
            HrnError::Io(format!("eval bridge: failed to bind loopback socket: {e}"))
        })?;
        let addr = listener.local_addr().map_err(|e| {
            HrnError::Io(format!("eval bridge: failed to inspect bound address: {e}"))
        })?;
        if !addr.ip().is_loopback() {
            return Err(HrnError::Auth(format!(
                "eval bridge refused non-loopback address `{addr}`"
            )));
        }
        let token = generate_token()?;
        let shared = Arc::new(BridgeShared {
            token: token.clone(),
            eval_host: init.eval_host,
            dyn_tools: init.dyn_tools,
            hints: init.hints,
            tools_enabled: init.tools_enabled,
            slot: init.slot,
            lang: init.lang,
            watchdog: Arc::new(WatchdogState::default()),
            active: Mutex::new(None),
            conn_sem: Arc::new(Semaphore::new(MAX_CONCURRENT_CONNS)),
        });
        let loop_shared = Arc::clone(&shared);
        let server_task = tokio::spawn(async move {
            accept_loop(listener, loop_shared).await;
        });
        Ok(Self {
            addr,
            token,
            shared,
            server_task,
        })
    }

    /// Loopback base URL (`http://127.0.0.1:<port>`).
    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Ephemeral 256-bit bearer token (ONLY for child process env injection and output redaction).
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    /// Watchdog pause state shared between the bridge endpoints and `Kernel::call`.
    pub(crate) fn watchdog(&self) -> &Arc<WatchdogState> {
        &self.shared.watchdog
    }

    /// Bind `ctx` as the active cell context for the duration of one cell or `@tool` execution.
    pub(crate) fn begin_call(&self, ctx: ToolContext) -> ActiveCallGuard {
        self.shared.watchdog.reset();
        *self
            .shared
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(ActiveCall {
            ctx,
            images: Vec::new(),
        });
        ActiveCallGuard {
            shared: Arc::clone(&self.shared),
            finished: false,
        }
    }
}

impl Drop for KernelBridge {
    fn drop(&mut self) {
        self.server_task.abort();
    }
}

/// Whether `name` is explicitly refused via `tool.<name>` inside eval cells.
pub(crate) fn is_refused_eval_tool(name: &str) -> bool {
    matches!(name, "checkpoint" | "rewind" | "yield" | "eval")
}

/// Canonical omp-style refusal text for restricted session-control and recursive eval tools.
pub(crate) fn refused_tool_message(name: &str) -> String {
    if name == "eval" {
        return "tool `eval` cannot be called recursively from inside eval".to_owned();
    }
    format!(
        "tool `{name}` cannot be called from eval: session-control tools (`checkpoint`, `rewind`, \
         `yield`) are restricted to direct agent calls"
    )
}

async fn accept_loop(listener: TcpListener, shared: Arc<BridgeShared>) {
    loop {
        let Ok((stream, _peer)) = listener.accept().await else {
            return;
        };
        let Ok(permit) = Arc::clone(&shared.conn_sem).acquire_owned().await else {
            return;
        };
        let conn_shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let _permit = permit;
            serve_connection(stream, &conn_shared).await;
        });
    }
}

async fn serve_connection(mut stream: TcpStream, shared: &BridgeShared) {
    let response = match read_request(&mut stream, &shared.token).await {
        Ok(req) => dispatch_route(shared, &req.path, req.body).await,
        Err(resp) => resp,
    };
    write_response(&mut stream, &response).await;
}

fn active_ctx(shared: &BridgeShared) -> std::result::Result<ToolContext, HttpResponse> {
    shared
        .active
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|a| a.ctx.clone())
        .ok_or_else(|| HttpResponse::err(409, "Conflict", "no active eval cell"))
}

fn record_images(shared: &BridgeShared, images: &[ImageAttachment]) {
    if images.is_empty() {
        return;
    }
    if let Some(active) = shared
        .active
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
    {
        active.images.extend(images.iter().cloned());
    }
}

async fn dispatch_route(shared: &BridgeShared, path: &str, body: Value) -> HttpResponse {
    match path {
        "/tool" => handle_tool(shared, body).await,
        "/agent/spawn" => handle_agent_spawn(shared, body).await,
        "/agent/wait" => handle_agent_wait(shared, &body).await,
        "/complete" => handle_complete(shared, &body).await,
        "/register_tool" => handle_register_tool(shared, &body),
        "/watchdog/pause" => {
            let depth = shared.watchdog.pause();
            HttpResponse::ok(&json!({ "ok": true, "paused": true, "depth": depth }))
        }
        "/watchdog/resume" => {
            let depth = shared.watchdog.resume();
            HttpResponse::ok(&json!({ "ok": true, "paused": depth > 0, "depth": depth }))
        }
        _ => HttpResponse::err(404, "Not Found", "unknown bridge endpoint"),
    }
}

async fn handle_tool(shared: &BridgeShared, body: Value) -> HttpResponse {
    let ctx = match active_ctx(shared) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let Some(name) = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return HttpResponse::err(400, "Bad Request", "missing string `name`");
    };
    if is_refused_eval_tool(name) {
        return HttpResponse::ok(&json!({
            "ok": false,
            "is_error": true,
            "error": refused_tool_message(name),
        }));
    }
    let args = body
        .get("args")
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!({}));
    let _pause = (name == "task").then(|| shared.watchdog.pause_guard());
    match invoke_host_tool(shared, &ctx, name, args).await {
        Ok(output) => {
            record_images(shared, &output.images);
            HttpResponse::ok(&json!({
                "ok": !output.is_error,
                "content": output.content,
                "is_error": output.is_error,
                "error": output.is_error.then_some(&output.content),
                "meta": output.meta,
                "images_count": output.images.len(),
            }))
        }
        Err(err) => HttpResponse::ok(&json!({
            "ok": false,
            "is_error": true,
            "error": err.to_string(),
        })),
    }
}

async fn invoke_host_tool(
    shared: &BridgeShared,
    ctx: &ToolContext,
    name: &str,
    args: Value,
) -> Result<ToolOutput> {
    if let Some(host) = &shared.eval_host {
        return host.call_tool(ctx, name, args).await;
    }
    if let Some(caller) = &ctx.caller {
        if caller.spec(name).is_none() {
            let mut valid = caller.tool_names();
            valid.retain(|n| !is_refused_eval_tool(n));
            valid.sort();
            return Ok(ToolOutput::err(format!(
                "unknown tool `{name}` (valid tools: {})",
                valid.join(", ")
            )));
        }
        return caller
            .call(name, args, ctx.cwd.clone(), ctx.cancel.clone())
            .await;
    }
    Err(HrnError::Tool(
        "eval bridge: no EvalHost is wired in this runtime".to_owned(),
    ))
}

async fn handle_agent_spawn(shared: &BridgeShared, body: Value) -> HttpResponse {
    let ctx = match active_ctx(shared) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let Some(host) = &shared.eval_host else {
        return HttpResponse::ok(&json!({
            "ok": false,
            "error": "eval bridge: no EvalHost is wired for agent()",
        }));
    };
    match host.spawn_agent(&ctx, body).await {
        Ok(agent) => HttpResponse::ok(&json!({ "ok": true, "agent": agent })),
        Err(err) => HttpResponse::ok(&json!({ "ok": false, "error": err.to_string() })),
    }
}

async fn handle_agent_wait(shared: &BridgeShared, body: &Value) -> HttpResponse {
    let ctx = match active_ctx(shared) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let Some(host) = &shared.eval_host else {
        return HttpResponse::ok(&json!({
            "ok": false,
            "error": "eval bridge: no EvalHost is wired for wait()",
        }));
    };
    let Some(raw_ids) = body.get("ids").and_then(Value::as_array) else {
        return HttpResponse::err(400, "Bad Request", "missing array `ids`");
    };
    let mut ids = Vec::with_capacity(raw_ids.len());
    for entry in raw_ids {
        let Some(id) = entry.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            return HttpResponse::err(
                400,
                "Bad Request",
                "`ids` entries must be non-empty strings",
            );
        };
        ids.push(id.to_owned());
    }
    let timeout_ms = body.get("timeout_ms").and_then(Value::as_u64);
    let _pause = shared.watchdog.pause_guard();
    match host.wait_agents(&ctx, ids, timeout_ms).await {
        Ok(results) => HttpResponse::ok(&json!({ "ok": true, "results": results })),
        Err(err) => HttpResponse::ok(&json!({ "ok": false, "error": err.to_string() })),
    }
}

async fn handle_complete(shared: &BridgeShared, body: &Value) -> HttpResponse {
    let ctx = match active_ctx(shared) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let Some(host) = &shared.eval_host else {
        return HttpResponse::ok(&json!({
            "ok": false,
            "error": "eval bridge: no EvalHost is wired for complete()",
        }));
    };
    let Some(prompt) = body.get("prompt").and_then(Value::as_str) else {
        return HttpResponse::err(400, "Bad Request", "missing string `prompt`");
    };
    let role = body
        .get("role")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let _pause = shared.watchdog.pause_guard();
    match host.complete(&ctx, prompt.to_owned(), role).await {
        Ok(text) => HttpResponse::ok(&json!({ "ok": true, "text": text })),
        Err(err) => HttpResponse::ok(&json!({ "ok": false, "error": err.to_string() })),
    }
}

fn handle_register_tool(shared: &BridgeShared, body: &Value) -> HttpResponse {
    let ctx = match active_ctx(shared) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    if !shared.tools_enabled {
        return HttpResponse::ok(&json!({
            "ok": false,
            "error": "eval kernel tools are disabled ([eval] tools_enabled = false)",
        }));
    }
    let Some(name) = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| {
            is_valid_dynamic_tool_name(s)
                && !shared.hints.is_registered_tool(s)
                && ctx.caller.as_ref().is_none_or(|c| c.spec(s).is_none())
        })
    else {
        return HttpResponse::err(
            400,
            "Bad Request",
            "invalid, reserved, or shadowing tool `name` for @tool registration",
        );
    };
    let description = body
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let parameters = body
        .get("parameters")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    let spec = ToolSpec {
        name: name.to_owned(),
        description,
        parameters,
    };
    shared
        .dyn_tools
        .register(&ctx.session_id, shared.lang, spec, shared.slot.clone());
    HttpResponse::ok(&json!({ "ok": true, "name": name }))
}

fn is_valid_dynamic_tool_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || name.starts_with("mcp__") {
        return false;
    }
    if matches!(
        name,
        "checkpoint"
            | "rewind"
            | "yield"
            | "eval"
            | "task"
            | "read"
            | "write"
            | "edit"
            | "apply_patch"
            | "bash"
            | "grep"
            | "glob"
            | "ls"
            | "wait"
            | "tool_search"
            | "desktop"
            | "browser"
            | "ask"
            | "todo"
            | "goal"
            | "think"
            | "find"
            | "web_fetch"
            | "web_search"
            | "ast_grep"
            | "ast_edit"
            | "lsp"
            | "debug"
            | "delete_file"
            | "rename_file"
            | "resolve"
    ) {
        return false;
    }
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    first_ok && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}
