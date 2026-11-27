//! The debug sessions of the process: what a `launch` creates outlives the tool call that made it
//! and shows up in `proc://`.

use hrn_edge_dap::session::{Outcome, Session, StartRequest, StartSpec, Summary};
use hrn_edge_dap::{AttachTarget, Catalog, ProgramKind, Selection};
use hrn_kernel::serde_json::{Map, Value};
use hrn_kernel::{Clock, HrnError, ProcEntry, ProcSource, Result, SystemClock};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// How long a session that ended on its own stays readable before it is dropped.
const IDLE_SWEEP_MS: i64 = 600_000;
/// Owner of a session, so `proc://` can scope it to a session tree.
#[derive(Clone, Debug)]
pub struct Owner {
    /// Root session that started it.
    pub root: String,
    /// Session whose tool calls drive it.
    pub owner: String,
}

/// What `launch` was asked for.
pub struct LaunchOptions<'a> {
    /// Who started it.
    pub owner: Owner,
    /// Debug target, already absolute.
    pub program: &'a Path,
    /// Adapter id; otherwise chosen from the target.
    pub adapter: Option<&'a str>,
    /// Working directory of the adapter and the debuggee.
    pub cwd: &'a Path,
    /// DAP `launch` arguments, merged over the adapter's defaults.
    pub arguments: Map<String, Value>,
    /// Per-request timeout in seconds.
    pub timeout_secs: u64,
}

/// What `attach` was asked for.
pub struct AttachOptions<'a> {
    /// Who started it.
    pub owner: Owner,
    /// Process id.
    pub pid: Option<u32>,
    /// Remote debug port.
    pub port: Option<u16>,
    /// Remote host.
    pub host: Option<&'a str>,
    /// Adapter id; otherwise chosen from what is installed.
    pub adapter: Option<&'a str>,
    /// Working directory of the adapter.
    pub cwd: &'a Path,
    /// DAP `attach` arguments, merged over the adapter's defaults.
    pub arguments: Map<String, Value>,
    /// Per-request timeout in seconds.
    pub timeout_secs: u64,
}

struct Entry {
    owner: Owner,
    session: std::sync::Arc<Session>,
    last_used_ms: i64,
}

/// The sessions started so far; at most one runs at a time.
pub struct DebugManager {
    catalog: std::sync::Arc<Catalog>,
    entries: Mutex<Vec<Entry>>,
    next_id: AtomicU64,
}

fn now() -> i64 {
    SystemClock.now_ms()
}

fn lock(entries: &Mutex<Vec<Entry>>) -> std::sync::MutexGuard<'_, Vec<Entry>> {
    entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn refusal(selection: &Selection) -> String {
    match selection {
        Selection::Unavailable { reason, .. } => reason.clone(),
        Selection::None(names) if names.is_empty() => {
            "no debugger adapter is available on this machine".to_owned()
        }
        Selection::None(names) => {
            format!(
                "no adapter matches the target; installed: {}",
                names.join(", ")
            )
        }
        Selection::Adapter(_) => String::new(),
    }
}

impl DebugManager {
    /// A manager over `catalog`.
    pub fn new(catalog: std::sync::Arc<Catalog>) -> Self {
        Self {
            catalog,
            entries: Mutex::default(),
            next_id: AtomicU64::new(0),
        }
    }

    /// The adapter descriptors.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Start a session, refusing while another one still runs.
    ///
    /// # Errors
    ///
    /// A live session, an adapter that cannot run here, or the adapter refusing the request.
    pub async fn launch(
        &self,
        options: LaunchOptions<'_>,
    ) -> Result<(std::sync::Arc<Session>, Outcome)> {
        let kind = if options.program.is_dir() {
            ProgramKind::Directory
        } else if options.program.is_file() {
            ProgramKind::File
        } else {
            ProgramKind::Missing
        };
        let adapter = match self
            .catalog
            .select_launch(options.program, options.cwd, options.adapter, kind)
            .await
        {
            Selection::Adapter(adapter) => *adapter,
            other => return Err(HrnError::Tool(refusal(&other))),
        };
        if kind == ProgramKind::Directory && !adapter.accepts_directory {
            return Err(HrnError::Tool(format!(
                "launch program `{}` is a directory; pass a file, or an adapter that debugs \
                 directories (for Go: adapter `dlv`)",
                options.program.display()
            )));
        }
        let mut arguments = adapter.launch_arguments(options.program, kind);
        arguments.extend(options.arguments);
        // The tool resolved the target itself: its absolute path wins over anything `json` put
        // under `program`, and adapters reject a launch that carries none.
        arguments.insert(
            "program".to_owned(),
            Value::String(options.program.to_string_lossy().into_owned()),
        );
        arguments
            .entry("cwd")
            .or_insert_with(|| Value::String(options.cwd.to_string_lossy().into_owned()));
        let program = options.program.to_string_lossy().into_owned();
        self.begin(
            &options.owner,
            options.cwd,
            options.timeout_secs,
            adapter,
            Some(program),
            StartRequest::Launch(arguments),
        )
        .await
    }

    /// Attach to a running process or a listening debug adapter.
    ///
    /// # Errors
    ///
    /// A live session, an adapter that cannot run here, or the adapter refusing the request.
    pub async fn attach(
        &self,
        options: AttachOptions<'_>,
    ) -> Result<(std::sync::Arc<Session>, Outcome)> {
        let adapter = match self
            .catalog
            .select_attach(
                options.cwd,
                options.adapter,
                &AttachTarget {
                    pid: options.pid,
                    port: options.port,
                },
            )
            .await
        {
            Selection::Adapter(adapter) => *adapter,
            other => return Err(HrnError::Tool(refusal(&other))),
        };
        let mut arguments = adapter.attach.clone();
        arguments.extend(options.arguments);
        if let Some(port) = options.port {
            let host = options.host.unwrap_or("127.0.0.1").to_owned();
            if adapter.attach_connect {
                arguments.insert(
                    "connect".to_owned(),
                    hrn_kernel::serde_json::json!({"host": host, "port": port}),
                );
            } else {
                arguments.insert("host".to_owned(), Value::String(host));
                arguments.insert("port".to_owned(), Value::from(port));
            }
        }
        if let Some(pid) = options.pid {
            arguments.insert("pid".to_owned(), Value::from(pid));
            if adapter.attach_connect {
                arguments.insert("processId".to_owned(), Value::from(pid));
            }
        }
        arguments
            .entry("cwd")
            .or_insert_with(|| Value::String(options.cwd.to_string_lossy().into_owned()));
        self.begin(
            &options.owner,
            options.cwd,
            options.timeout_secs,
            adapter,
            None,
            StartRequest::Attach(arguments),
        )
        .await
    }

    async fn begin(
        &self,
        owner: &Owner,
        cwd: &Path,
        timeout_secs: u64,
        adapter: hrn_edge_dap::Resolved,
        program: Option<String>,
        request: StartRequest,
    ) -> Result<(std::sync::Arc<Session>, Outcome)> {
        self.sweep();
        if let Some(live) = lock(&self.entries).iter().find(|e| e.session.is_live()) {
            return Err(HrnError::Tool(format!(
                "debug session {} is still active. Terminate it before launching another.",
                live.session.id()
            )));
        }
        let id = format!("debug-{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let (session, outcome) = Session::start(StartSpec {
            id,
            adapter,
            cwd: cwd.to_path_buf(),
            program,
            request,
            timeout: std::time::Duration::from_secs(timeout_secs),
        })
        .await?;
        lock(&self.entries).push(Entry {
            owner: owner.clone(),
            session: std::sync::Arc::clone(&session),
            last_used_ms: now(),
        });
        Ok((session, outcome))
    }

    /// Forget sessions that ended a while ago and are no longer running.
    fn sweep(&self) {
        let cutoff = now() - IDLE_SWEEP_MS;
        lock(&self.entries).retain(|e| e.session.is_live() || e.last_used_ms > cutoff);
    }

    /// The session an action without `session` applies to: the last one that stopped, else the
    /// last one still running, else the newest ended one whose output can still be read.
    ///
    /// # Errors
    ///
    /// When no session has been started.
    pub fn active(&self) -> Result<std::sync::Arc<Session>> {
        self.sweep();
        let mut entries = lock(&self.entries);
        let preferred = entries
            .iter()
            .filter(|e| e.session.is_live())
            .max_by_key(|e| u8::from(e.session.summary().status == "stopped"))
            .or_else(|| entries.last())
            .map(|e| e.session.id().to_owned());
        let chosen = entries
            .iter_mut()
            .find(|e| Some(e.session.id()) == preferred.as_deref())
            .map(|e| {
                e.last_used_ms = now();
                std::sync::Arc::clone(&e.session)
            });
        chosen.ok_or_else(|| {
            HrnError::Tool("No active debug session. Launch or attach first.".into())
        })
    }

    /// Every target of every session, live ones first.
    pub fn list(&self) -> Vec<(Owner, Summary)> {
        self.sweep();
        let mut out: Vec<(Owner, Summary)> = Vec::new();
        for entry in lock(&self.entries).iter() {
            for summary in entry.session.summaries() {
                if summary.status != "terminated" {
                    out.push((entry.owner.clone(), summary));
                }
            }
        }
        for entry in lock(&self.entries).iter() {
            for summary in entry.session.summaries() {
                if summary.status == "terminated" {
                    out.push((entry.owner.clone(), summary));
                }
            }
        }
        out
    }

    /// Terminate the session of target `id` (default: the active one). `None` when there is none.
    pub async fn terminate(
        &self,
        id: Option<&str>,
    ) -> Option<(Summary, Vec<String>, std::sync::Arc<Session>)> {
        let session = match id {
            None => self.active().ok()?,
            Some(id) => self.target_session(id).ok()?,
        };
        let (summary, notes) = session.terminate().await;
        Some((summary, notes, session))
    }

    fn target_session(&self, id: &str) -> Result<std::sync::Arc<Session>> {
        let root = id.split('.').next().unwrap_or(id);
        lock(&self.entries)
            .iter()
            .find(|e| e.session.id() == root)
            .map(|e| std::sync::Arc::clone(&e.session))
            .filter(|s| s.summaries().iter().any(|t| t.id == id))
            .ok_or_else(|| {
                HrnError::Tool(format!(
                    "no debug target `{id}`; list them with action `sessions`"
                ))
            })
    }

    /// The session owning target `id`, defaulting to the active one.
    ///
    /// # Errors
    ///
    /// When `id` names no target of a known session.
    pub fn session_for(&self, id: Option<&str>) -> Result<std::sync::Arc<Session>> {
        match id {
            None => self.active(),
            Some(id) => self.target_session(id),
        }
    }

    /// End every session now (process teardown).
    pub fn dispose_all(&self) {
        let sessions: Vec<std::sync::Arc<Session>> = lock(&self.entries)
            .iter()
            .map(|e| std::sync::Arc::clone(&e.session))
            .collect();
        for session in sessions {
            session.dispose();
        }
    }
}

fn elapsed(session: &Session) -> u64 {
    let ended = session.finished_ms();
    let until = if ended == 0 { now() } else { ended };
    u64::try_from((until - session.started_ms()).max(0) / 1000).unwrap_or(0)
}

fn line(session: &Session, summary: &Summary) -> String {
    let location = match (&summary.source_path, summary.line) {
        (Some(path), Some(line)) => format!(" · {path}:{line}"),
        _ => String::new(),
    };
    format!(
        "{} [debug] {} · {}s · adapter {} · {}{location}",
        summary.id,
        summary.status,
        elapsed(session),
        session.adapter_name(),
        summary.program.as_deref().unwrap_or("(attach)"),
    )
}

impl ProcSource for DebugManager {
    fn entries(&self) -> Vec<ProcEntry> {
        self.sweep();
        let mut out = Vec::new();
        for entry in lock(&self.entries).iter() {
            for summary in entry.session.summaries() {
                out.push(ProcEntry {
                    id: summary.id.clone(),
                    root: entry.owner.root.clone(),
                    owner: entry.owner.owner.clone(),
                    running: summary.status != "terminated",
                    // A debug session is inspected, never waited on: it ends when told to.
                    waitable: false,
                    started_ms: entry.session.started_ms(),
                    finished_ms: entry.session.finished_ms(),
                    line: line(&entry.session, &summary),
                });
            }
        }
        out
    }

    fn detail(&self, id: &str) -> Option<String> {
        let session = self.target_session(id).ok()?;
        let summary = session.summaries().into_iter().find(|t| t.id == id)?;
        let mut text = format!(
            "{}\ncwd: {}\nprogram: {}\nstatus: {}",
            line(&session, &summary),
            summary.cwd,
            summary.program.as_deref().unwrap_or("(attach)"),
            summary.status,
        );
        if summary.needs_configuration_done {
            text.push_str(
                "\nconfiguration: pending configurationDone; set breakpoints, then continue.",
            );
        }
        if let Some(reason) = &summary.stop_reason {
            let _ = writeln!(text, "\nstopped: {reason}");
        }
        if let (Some(path), Some(line)) = (&summary.source_path, summary.line) {
            let _ = writeln!(text, "\nlocation: {path}:{line}");
        }
        if let Some(code) = summary.exit_code {
            let _ = writeln!(text, "\nexit code: {code}");
        }
        let output = session.output();
        text.push_str("\n\n");
        text.push_str(if output.is_empty() {
            "(no output)"
        } else {
            &output
        });
        Some(text)
    }

    fn streams(&self, id: &str) -> Option<String> {
        Some(self.target_session(id).ok()?.output())
    }

    fn kill(&self, id: &str) -> Option<Result> {
        let session = self.target_session(id).ok()?;
        if !session.is_live() {
            return Some(Err(HrnError::Tool(format!(
                "debug session {id} is already terminated"
            ))));
        }
        // `kill` runs without a runtime to await on; dispose ends the debuggee and its processes.
        session.dispose();
        Some(Ok(()))
    }
}
