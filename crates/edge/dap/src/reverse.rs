//! What a session does with an adapter's events and with its requests to the client
//! (`runInTerminal`, `startDebugging`).

use crate::client::{DapClient, Inbound};
use crate::session::Session;
use crate::state::{Shared, Target};
use hrn_edge_proc::build_stdio_command;
use hrn_kernel::{BoxFuture, HrnError, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, Weak};
use tokio::io::{AsyncRead, AsyncReadExt};

/// The [`Inbound`] of one target. It holds the session weakly: a connection must not keep a
/// dropped session (and its adapter process) alive.
pub(crate) struct Handler {
    shared: Weak<Shared>,
    session: Weak<Session>,
    target: String,
}

impl Handler {
    pub(crate) fn arc(session: &Arc<Session>, target: &str) -> Arc<dyn Inbound> {
        Arc::new(Self {
            shared: Arc::downgrade(&session.shared),
            session: Arc::downgrade(session),
            target: target.to_owned(),
        })
    }
}

impl Inbound for Handler {
    fn event(&self, name: &str, body: Value) {
        if let Some(shared) = self.shared.upgrade() {
            shared.with(|s| s.on_event(&self.target, name, &body));
        }
    }

    fn closed(&self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.with(|s| s.end(&self.target));
        }
    }

    fn reverse_request(&self, command: String, args: Value) -> BoxFuture<Result<Value>> {
        let session = self.session.upgrade();
        let target = self.target.clone();
        Box::pin(async move {
            let session =
                session.ok_or_else(|| HrnError::Tool("the debug session has ended".into()))?;
            match command.as_str() {
                "runInTerminal" => session.run_in_terminal(&args),
                "startDebugging" => session.start_child(&target, &args).await,
                other => Err(HrnError::Tool(format!(
                    "unsupported request `{other}` from the adapter"
                ))),
            }
        })
    }
}

/// Copy a pipe into the session output until it closes.
fn pump(mut pipe: impl AsyncRead + Unpin + Send + 'static, shared: Weak<Shared>) {
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut buf).await {
            let Some(chunk) = buf.get(..n).filter(|c| !c.is_empty()) else {
                break;
            };
            let Some(shared) = shared.upgrade() else {
                break;
            };
            shared.with(|s| s.output.push(&String::from_utf8_lossy(chunk)));
        }
    });
}

impl Session {
    /// `runInTerminal`: run the debuggee the adapter describes, capturing its output.
    fn run_in_terminal(&self, request: &Value) -> Result<Value> {
        let argv: Vec<String> = request
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| HrnError::Tool("runInTerminal did not include a command".into()))?;
        let cwd = match request.get("cwd").and_then(Value::as_str) {
            Some(dir) if Path::new(dir).is_absolute() => PathBuf::from(dir),
            Some(dir) => self.cwd.join(dir),
            None => self.cwd.clone(),
        };
        let env: BTreeMap<String, String> = request
            .get("env")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let mut child = build_stdio_command(program, rest, Some(&cwd), &env)
            .spawn()
            .map_err(|e| HrnError::Tool(format!("failed to start `{program}`: {e}")))?;
        if let Some(out) = child.stdout().take() {
            pump(out, Arc::downgrade(&self.shared));
        }
        if let Some(err) = child.stderr().take() {
            pump(err, Arc::downgrade(&self.shared));
        }
        let pid = child.id();
        self.terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(child);
        Ok(json!({"processId": pid}))
    }

    /// `startDebugging`: open a child target on the adapter's server port and start it the way
    /// the root was started.
    async fn start_child(self: &Arc<Self>, parent: &str, args: &Value) -> Result<Value> {
        let request = match args.get("request").and_then(Value::as_str) {
            Some("attach") => "attach",
            _ => "launch",
        };
        let mut configuration: BTreeMap<String, Value> = args
            .get("configuration")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let (port, parent_cwd, parent_program) = self
            .shared
            .read(|s| {
                s.target(parent).map(|t| {
                    (
                        t.client.as_ref().and_then(|c| c.port()),
                        t.cwd.clone(),
                        t.program.clone(),
                    )
                })
            })
            .ok_or_else(|| HrnError::Tool(format!("unknown debug target {parent}")))?;
        let port = port.ok_or_else(|| {
            HrnError::Tool(format!(
                "adapter {} cannot accept child session connections",
                self.adapter.name
            ))
        })?;
        let cwd: PathBuf = match configuration.get("cwd").and_then(Value::as_str) {
            Some(dir) if Path::new(dir).is_absolute() => PathBuf::from(dir),
            Some(dir) => parent_cwd.join(dir),
            None => parent_cwd,
        };
        configuration.insert("cwd".into(), json!(cwd.to_string_lossy()));
        // A child that names no program of its own debugs the parent's: adapters ask for extra
        // sessions inside one program (a worker, a `source` file) rather than a new target.
        if configuration
            .get("program")
            .and_then(Value::as_str)
            .is_none()
        {
            let inherited = parent_program.as_deref().or_else(|| {
                configuration
                    .get("source")
                    .and_then(Value::as_object)
                    .and_then(|s| s.get("path"))
                    .and_then(Value::as_str)
            });
            if let Some(program) = inherited {
                configuration.insert("program".into(), json!(program));
            }
        }
        let program = configuration
            .get("program")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = format!(
            "{}.{}",
            self.id,
            self.next_child.fetch_add(1, Ordering::SeqCst) + 1
        );
        // An adapter that multiplexes sessions on one port addresses them by `name`; keep the
        // adapter's own when it sent one, else pass the target id on.
        configuration
            .entry("name".to_owned())
            .or_insert_with(|| json!(&id));
        self.shared.with(|s| {
            s.targets.push(Target::new(
                id.clone(),
                Some(parent.to_owned()),
                program,
                cwd,
            ));
        });
        let started = async {
            let client =
                DapClient::connect_tcp("127.0.0.1", port, self.timeout, Handler::arc(self, &id))
                    .await?;
            self.shared.with(|s| {
                if let Some(t) = s.target_mut(&id) {
                    t.client = Some(Arc::new(client));
                }
            });
            let arguments = Value::Object(
                configuration
                    .into_iter()
                    .collect::<serde_json::Map<String, Value>>(),
            );
            self.begin(&id, request, arguments).await
        };
        if let Err(e) = started.await {
            self.shared.with(|s| s.targets.retain(|t| t.id != id));
            return Err(e);
        }
        Ok(Value::Object(serde_json::Map::new()))
    }
}
