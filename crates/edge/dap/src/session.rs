//! A debug session: the adapter connection(s), the launch/attach handshake and every operation
//! that follows it.

use crate::breakpoints::Breakpoints;
use crate::catalog::{Connect, Resolved};
use crate::client::{DapClient, SpawnSpec};
use crate::reverse::Handler;
use crate::state::{Shared, State, Status, Target};
use hrn_edge_proc::ChildWrapper;
use hrn_kernel::{Clock, HrnError, Result, SystemClock};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

/// How long after a launch/attach, a resume or a pause we look for the stop it causes before
/// reporting the program as running, at most. debugpy needs this much to attach and pause the
/// debuggee; a program that runs or exits sooner answers through the same watch.
const STOP_CAPTURE: Duration = Duration::from_secs(10);
/// Longest wait for the stack frame of a stop and for each step of a shutdown.
const SHORT_REQUEST: Duration = Duration::from_secs(5);

/// How a session begins.
pub enum StartRequest {
    /// `launch` with these arguments (program, cwd and args already in).
    Launch(Map<String, Value>),
    /// `attach` with these arguments; `skipAttachRequest: true` only runs the handshake.
    Attach(Map<String, Value>),
}

/// What [`Session::start`] needs.
pub struct StartSpec {
    /// The session id (`debug-1`).
    pub id: String,
    /// The adapter to run.
    pub adapter: Resolved,
    /// Working directory of the adapter and the base of relative paths.
    pub cwd: PathBuf,
    /// The launch target, for display.
    pub program: Option<String>,
    /// Launch or attach.
    pub request: StartRequest,
    /// Per-request timeout (also bounds the handshake).
    pub timeout: Duration,
}

/// How a resume ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    /// Stopped at a breakpoint, step end or pause.
    Stopped,
    /// The program ended.
    Terminated,
    /// Still running.
    Running,
}

/// The resuming requests.
#[derive(Clone, Copy, Debug)]
pub enum Resume {
    /// `continue`.
    Continue,
    /// `next`.
    Over,
    /// `stepIn`.
    In,
    /// `stepOut`.
    Out,
}

impl Resume {
    pub(crate) const fn command(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Over => "next",
            Self::In => "stepIn",
            Self::Out => "stepOut",
        }
    }
}

/// A target's state as the model sees it.
#[derive(Clone, Debug)]
pub struct Summary {
    /// The session id (a child target: `debug-1.1`).
    pub id: String,
    /// Adapter id.
    pub adapter: String,
    /// `launching`, `configuring`, `running`, `stopped` or `terminated`.
    pub status: &'static str,
    /// Working directory.
    pub cwd: String,
    /// Launch target.
    pub program: Option<String>,
    /// Why it stopped.
    pub stop_reason: Option<String>,
    /// Top frame id.
    pub frame_id: Option<i64>,
    /// Top frame name.
    pub frame_name: Option<String>,
    /// Instruction pointer reference of the top frame.
    pub instruction_pointer: Option<String>,
    /// Source file of the top frame.
    pub source_path: Option<String>,
    /// Line of the top frame.
    pub line: Option<i64>,
    /// Column of the top frame.
    pub column: Option<i64>,
    /// `configurationDone` has not been sent yet.
    pub needs_configuration_done: bool,
    /// Exit code of the program.
    pub exit_code: Option<i64>,
}

/// The result of an operation that can change what the program is doing.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The focused target afterwards.
    pub summary: Summary,
    /// Stopped, terminated or running.
    pub state: RunState,
    /// The wait for a stop ran out with the program still running.
    pub timed_out: bool,
    /// Things that went wrong without failing the operation.
    pub notes: Vec<String>,
}

/// One debug session (possibly several adapter connections), persisting between tool calls.
pub struct Session {
    pub(crate) id: String,
    pub(crate) adapter: Resolved,
    pub(crate) cwd: PathBuf,
    program: Option<String>,
    started_ms: i64,
    pub(crate) launched: bool,
    pub(crate) timeout: Duration,
    pub(crate) shared: Arc<Shared>,
    pub(crate) bps: AsyncMutex<Breakpoints>,
    pub(crate) terminals: Mutex<Vec<Box<dyn ChildWrapper>>>,
    pub(crate) next_child: AtomicU64,
}

fn flatten(joined: std::result::Result<Result<Value>, tokio::task::JoinError>) -> Result<Value> {
    joined.map_err(|e| HrnError::Internal(format!("DAP start task: {e}")))?
}

impl Session {
    /// Start the adapter, run the handshake and report where the program is.
    ///
    /// # Errors
    ///
    /// The adapter cannot start, rejects the request, or the handshake times out.
    pub async fn start(spec: StartSpec) -> Result<(Arc<Self>, Outcome)> {
        let launched = matches!(spec.request, StartRequest::Launch(_));
        let session = Arc::new(Self {
            id: spec.id.clone(),
            adapter: spec.adapter,
            cwd: spec.cwd.clone(),
            program: spec.program.clone(),
            started_ms: SystemClock.now_ms(),
            launched,
            timeout: spec.timeout,
            shared: Arc::new(Shared::new()),
            bps: AsyncMutex::default(),
            terminals: Mutex::default(),
            next_child: AtomicU64::new(0),
        });
        session.shared.with(|s| {
            s.targets
                .push(Target::new(spec.id.clone(), None, spec.program, spec.cwd));
            s.active = Some(spec.id.clone());
        });
        match session.open_root(spec.request).await {
            Ok(outcome) => Ok((session, outcome)),
            Err(e) => {
                session.dispose();
                Err(e)
            }
        }
    }

    async fn open_root(self: &Arc<Self>, request: StartRequest) -> Result<Outcome> {
        let process = SpawnSpec {
            program: self.adapter.program.clone(),
            args: self.adapter.args.clone(),
            cwd: self.cwd.clone(),
            env: self.adapter.env.clone(),
        };
        let handler = Handler::arc(self, &self.id);
        let client = match self.adapter.connect {
            Connect::Stdio => DapClient::spawn_stdio(&process, handler)?,
            Connect::Tcp => DapClient::spawn_tcp(&process, self.timeout, handler).await?,
        };
        let id = self.id.clone();
        self.shared
            .with(|s| s.target_mut(&id).map(|t| t.client = Some(Arc::new(client))));
        let (command, args) = match request {
            StartRequest::Launch(args) => ("launch", args),
            StartRequest::Attach(args) => ("attach", args),
        };
        self.begin(&id, command, Value::Object(args)).await?;
        self.shared
            .wait_for(STOP_CAPTURE.min(self.timeout), |s| {
                (s.any_stopped() || s.all_terminated()).then_some(())
            })
            .await;
        Ok(self.outcome(false).await)
    }

    /// `initialize`, the start request and the `initialized` → `configurationDone` exchange. An
    /// `attach` argument set carrying `skipAttachRequest` only runs the handshake.
    pub(crate) async fn begin(
        self: &Arc<Self>,
        id: &str,
        command: &str,
        mut args: Value,
    ) -> Result {
        let skip_request = args
            .as_object_mut()
            .and_then(|m| m.remove("skipAttachRequest"))
            == Some(Value::Bool(true));
        let timeout = self.timeout;
        let client = self.client_of(id)?;
        let caps = client
            .request(
                "initialize",
                json!({
                    "clientID": "hrn", "clientName": "hrn", "adapterID": self.adapter.name,
                    "locale": "en-US", "linesStartAt1": true, "columnsStartAt1": true,
                    "pathFormat": "path", "supportsRunInTerminalRequest": true,
                    "supportsStartDebuggingRequest": true, "supportsMemoryReferences": true,
                    "supportsVariableType": true, "supportsInvalidatedEvent": true,
                }),
                timeout,
            )
            .await?;
        self.shared.with(|s| {
            if let Some(t) = s.target_mut(id) {
                t.needs_cfg = caps
                    .get("supportsConfigurationDoneRequest")
                    .and_then(Value::as_bool)
                    == Some(true);
                t.caps = caps;
            }
        });
        // Most adapters answer the start request only after `configurationDone`, so it is sent
        // without waiting and its answer collected at the end.
        let mut start = (!skip_request).then(|| {
            let (client, command) = (Arc::clone(&client), command.to_owned());
            tokio::spawn(async move { client.request(&command, args, timeout).await })
        });
        let result = self.handshake(id, &client, &mut start).await;
        if result.is_err()
            && let Some(task) = &start
        {
            task.abort();
        }
        result
    }

    async fn handshake(
        self: &Arc<Self>,
        id: &str,
        client: &Arc<DapClient>,
        start: &mut Option<tokio::task::JoinHandle<Result<Value>>>,
    ) -> Result {
        let timeout = self.timeout;
        let ready = |s: &State| {
            s.target(id)
                .is_some_and(|t| t.initialized || t.status == Status::Terminated)
                .then_some(())
        };
        let mut early = None;
        if !self
            .shared
            .read(|s| s.target(id).is_some_and(|t| t.initialized))
        {
            if let Some(task) = start.as_mut() {
                tokio::select! {
                    done = task => early = Some(flatten(done)),
                    _ = self.shared.wait_for(timeout, ready) => {}
                }
            } else {
                self.shared.wait_for(timeout, ready).await;
            }
        }
        if let Some(Err(e)) = early {
            return Err(e);
        }
        let (needs_cfg, initialized, child) = self.shared.read(|s| {
            s.target(id).map_or((false, false, false), |t| {
                (
                    t.needs_cfg && !t.cfg_sent,
                    t.initialized,
                    t.parent.is_some(),
                )
            })
        });
        if early.is_some() && needs_cfg && !initialized {
            self.shared
                .wait_for(SHORT_REQUEST.min(timeout), ready)
                .await;
        }
        let initialized = self
            .shared
            .read(|s| s.target(id).is_some_and(|t| t.initialized));
        if child {
            self.apply_breakpoints(id, client).await;
        }
        if needs_cfg && initialized {
            client
                .request("configurationDone", json!({}), timeout)
                .await?;
            self.shared.with(|s| {
                if let Some(t) = s.target_mut(id) {
                    t.cfg_sent = true;
                }
            });
        }
        self.shared.with(|s| {
            if let Some(t) = s.target_mut(id)
                && matches!(t.status, Status::Launching | Status::Configuring)
                && (t.cfg_sent || !t.needs_cfg)
            {
                t.status = Status::Running;
            }
        });
        if early.is_none()
            && let Some(task) = start.take()
        {
            flatten(task.await)?;
        }
        Ok(())
    }

    pub(crate) fn client_of(&self, id: &str) -> Result<Arc<DapClient>> {
        self.shared
            .read(|s| s.target(id).and_then(|t| t.client.clone()))
            .ok_or_else(|| HrnError::Tool(format!("debug target {id} has no adapter connection")))
    }

    /// The focused target and its connection; a connection found dead ends its target first.
    pub(crate) fn active(&self) -> Result<(String, Arc<DapClient>)> {
        self.shared.with(|s| {
            let dead: Vec<String> = s
                .targets
                .iter()
                .filter(|t| {
                    t.status != Status::Terminated
                        && t.client.as_ref().is_some_and(|c| !c.is_alive())
                })
                .map(|t| t.id.clone())
                .collect();
            for id in dead {
                s.end(&id);
            }
        });
        self.shared
            .read(|s| {
                s.active_target()
                    .and_then(|t| t.client.clone().map(|c| (t.id.clone(), c)))
            })
            .ok_or_else(|| {
                HrnError::Tool("No active debug session. Launch or attach first.".into())
            })
    }

    /// Send `configurationDone` if the handshake left it pending.
    pub(crate) async fn ensure_configured(
        &self,
        id: &str,
        client: &DapClient,
        timeout: Duration,
    ) -> Result {
        let pending = self
            .shared
            .read(|s| s.target(id).is_some_and(|t| t.needs_cfg && !t.cfg_sent));
        if !pending {
            return Ok(());
        }
        client
            .request("configurationDone", json!({}), timeout)
            .await?;
        self.shared.with(|s| {
            if let Some(t) = s.target_mut(id) {
                t.cfg_sent = true;
                if t.status == Status::Configuring {
                    t.status = Status::Running;
                }
            }
        });
        Ok(())
    }

    /// The session id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Working directory of the adapter and the base of relative paths.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Whether the focused target's adapter reports `capability` as true.
    pub fn supports(&self, capability: &str) -> bool {
        self.shared
            .read(|s| s.active_target().is_some_and(|t| t.supports(capability)))
    }

    /// The adapter id.
    pub fn adapter_name(&self) -> &str {
        &self.adapter.name
    }

    /// How the client reaches the adapter.
    pub fn connect(&self) -> Connect {
        self.adapter.connect
    }

    /// The launch target.
    pub fn program(&self) -> Option<&str> {
        self.program.as_deref()
    }

    /// Start time (ms since epoch).
    pub fn started_ms(&self) -> i64 {
        self.started_ms
    }

    /// End time (ms since epoch); `0` while a target lives.
    pub fn finished_ms(&self) -> i64 {
        self.shared.read(|s| s.finished_ms)
    }

    /// Whether any target is still alive.
    pub fn is_live(&self) -> bool {
        let _ = self.active();
        self.shared
            .read(|s| s.targets.iter().any(|t| t.status != Status::Terminated))
    }

    fn summary_of(&self, s: &State, t: &Target) -> Summary {
        Summary {
            id: t.id.clone(),
            adapter: self.adapter.name.clone(),
            status: t.status.as_str(),
            cwd: t.cwd.to_string_lossy().into_owned(),
            program: t.program.clone(),
            stop_reason: t.stop.reason.clone(),
            frame_id: t.stop.frame_id,
            frame_name: t.stop.frame_name.clone(),
            instruction_pointer: t.stop.instruction_pointer.clone(),
            source_path: t.stop.source_path.clone(),
            line: t.stop.line,
            column: t.stop.column,
            needs_configuration_done: t.needs_cfg && !t.cfg_sent,
            exit_code: t.exit_code.or_else(|| {
                // The root's exit code stands for the session once a child is focused.
                s.targets.first().and_then(|r| r.exit_code)
            }),
        }
    }

    /// The focused target's state.
    pub fn summary(&self) -> Summary {
        let _ = self.active();
        self.shared.read(|s| {
            let target = s.active_target().or_else(|| s.targets.first());
            target.map_or_else(
                || Summary {
                    id: self.id.clone(),
                    adapter: self.adapter.name.clone(),
                    status: Status::Terminated.as_str(),
                    cwd: self.cwd.to_string_lossy().into_owned(),
                    program: self.program.clone(),
                    stop_reason: None,
                    frame_id: None,
                    frame_name: None,
                    instruction_pointer: None,
                    source_path: None,
                    line: None,
                    column: None,
                    needs_configuration_done: false,
                    exit_code: None,
                },
                |t| self.summary_of(s, t),
            )
        })
    }

    /// Every target of the session (root and children).
    pub fn summaries(&self) -> Vec<Summary> {
        let _ = self.active();
        self.shared
            .read(|s| s.targets.iter().map(|t| self.summary_of(s, t)).collect())
    }

    /// What the program and adapter printed (the newest 128 KiB).
    pub fn output(&self) -> String {
        self.shared.read(|s| s.output.text().to_owned())
    }

    /// The focused adapter's capabilities.
    pub fn capabilities(&self) -> Value {
        self.shared
            .read(|s| s.active_target().map_or(Value::Null, |t| t.caps.clone()))
    }
}
