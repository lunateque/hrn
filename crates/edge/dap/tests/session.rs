//! Session behaviour against the mock adapter (`mock-adapter.rs`): the launch handshake, the
//! breakpoint list replayed to a child session the adapter asked for, resume outcomes,
//! `runInTerminal`, output capture and shutdown.

// A test crate is compiled without `cfg(test)` for clippy's in-tests allowances, so the value
// panics that name what failed are allowed here explicitly.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a test panic names the failing value"
)]

use hrn_edge_dap::breakpoints::{Breakpoint, Edit, Kind};
use hrn_edge_dap::catalog::{Catalog, Connect, ProgramKind, Selection};
use hrn_edge_dap::client::{DapClient, Inbound};
use hrn_edge_dap::session::{Outcome, Resume, RunState, Session, StartRequest, StartSpec};
use hrn_kernel::serde_json::{Map, Value, json};
use hrn_kernel::testing::TempDir;
use hrn_kernel::{BoxFuture, HrnError, Result as HrnResult};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The mock answers in milliseconds; this bound only fires when something really hangs.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Long enough for the "still running" path, short enough to keep the suite quick.
const FAST: Duration = Duration::from_millis(800);

fn exe() -> String {
    Path::new(env!("CARGO_BIN_EXE_mock-adapter"))
        .to_string_lossy()
        .into_owned()
}

fn switches(stop_on_entry: bool, extra: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if stop_on_entry {
        out.insert("stopOnEntry".into(), json!(true));
    }
    for (key, value) in extra.as_object().into_iter().flatten() {
        out.insert(key.clone(), value.clone());
    }
    out
}

/// A built-in-shaped descriptor slot pointed at the mock: `debugpy` speaks stdio, `dlv` speaks TCP
/// on a port the client reserves (so the mock can be asked for child sessions).
fn mock_catalog(id: &str, connect: &str, port: Option<u16>, extra: &Value) -> Catalog {
    let mut descriptor = json!({
        "command": [exe()],
        "args": [],
        "python_module": null,
        "launch": switches(true, extra),
        "attach": switches(false, extra),
    });
    if connect != "stdio" {
        port.expect("a tcp mock names its port");
        let map = descriptor.as_object_mut().expect("descriptor object");
        map.insert("connect".into(), json!(connect));
        map.insert("args".into(), json!(["--listen", "${port}"]));
    }
    Catalog::load(&BTreeMap::from([(id.to_owned(), descriptor)])).expect("catalog")
}

/// Launch the mock and wait for the entry stop it always produces.
async fn start(
    dir: &Path,
    catalog: &Catalog,
    id: &str,
) -> std::result::Result<(Arc<Session>, Outcome), HrnError> {
    let Selection::Adapter(mut adapter) = catalog
        .select_launch(&dir.join("mock.py"), dir, Some(id), ProgramKind::File)
        .await
    else {
        panic!("the mock adapter must resolve for `{id}`");
    };
    adapter.env.insert(
        "HRN_MOCK_LOG".into(),
        dir.join("requests.json").to_string_lossy().into_owned(),
    );
    let mut launch = adapter.launch.clone();
    launch.insert("name".into(), json!("debug-1"));
    launch.insert(
        "program".into(),
        json!(dir.join("mock.py").to_string_lossy()),
    );
    launch.insert("cwd".into(), json!(dir.to_string_lossy()));
    Session::start(StartSpec {
        id: "debug-1".to_owned(),
        adapter: *adapter,
        cwd: dir.to_path_buf(),
        program: Some(dir.join("mock.py").to_string_lossy().into_owned()),
        request: StartRequest::Launch(launch),
        timeout: TIMEOUT,
    })
    .await
}

async fn launch(dir: &Path, catalog: &Catalog, id: &str) -> Arc<Session> {
    let (session, outcome) = start(dir, catalog, id).await.expect("launch");
    assert!(
        !outcome.summary.needs_configuration_done,
        "configurationDone belongs to the handshake"
    );
    session
}

/// Poll `f` until it yields a value (the adapter's request reaches us asynchronously).
async fn loop_until<T>(
    f: impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<T>> + Send>>,
) -> T {
    for _ in 0..100 {
        if let Some(value) = f().await {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the condition never became true");
}

fn reserved_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn logged(dir: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(dir.join("requests.json")).unwrap_or_default();
    text.lines().filter_map(|line| line.parse().ok()).collect()
}

fn sent_to(entries: &[Value], target: &str) -> Vec<String> {
    entries
        .iter()
        .filter(|m| m.get("target").and_then(Value::as_str) == Some(target))
        .map(|m| m["command"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn a_launch_hands_the_adapter_over_and_stops_on_entry() {
    let dir = TempDir::new("dap-launch");
    let catalog = mock_catalog("debugpy", "stdio", None, &json!({}));
    let (session, outcome) = start(dir.path(), &catalog, "debugpy")
        .await
        .expect("launch");
    assert_eq!(outcome.state, RunState::Stopped, "the mock stops on entry");
    let summary = session.summary();
    assert_eq!(summary.id, "debug-1");
    assert_eq!(summary.adapter, "debugpy");
    assert_eq!(summary.status, "stopped");
    assert_eq!(summary.stop_reason.as_deref(), Some("entry"));
    // The stop location came from the adapter's own stack trace.
    assert_eq!(summary.source_path.as_deref(), Some("mock.py"));
    assert_eq!(summary.line, Some(3));
    assert_eq!(outcome.summary.status, "stopped");
    // The program's printout reached the capture as `output` events.
    assert!(session.output().contains("hello from mock"));

    // And the log shows the handshake in the order DAP requires. The first connection logs
    // under `conn-<id>` until the launch names the session, so `initialize` sits there.
    let entries = logged(dir.path());
    let commands: Vec<String> = entries
        .iter()
        .map(|m| m["command"].as_str().unwrap_or_default().to_owned())
        .collect();
    let at = |name: &str| commands.iter().position(|c| c == name);
    assert!(
        at("initialize").is_some_and(|i| at("launch").is_some_and(|l| i < l))
            && at("launch").is_some_and(|l| at("configurationDone").is_some_and(|c| l < c))
            && at("configurationDone").is_some_and(|c| at("stackTrace").is_some_and(|s| c < s)),
        "handshake order: {commands:?}"
    );
    session.terminate().await;
}

#[tokio::test]
async fn a_rejected_launch_fails_and_leaves_nothing_behind() {
    let dir = TempDir::new("dap-bad-launch");
    let catalog = mock_catalog("debugpy", "stdio", None, &json!({"__bad": true}));
    let refused = start(dir.path(), &catalog, "debugpy").await;
    match refused {
        Err(err) => assert!(err.to_string().contains("Program not found"), "{err}"),
        Ok(_) => panic!("the mock refuses the launch"),
    }
}

#[tokio::test]
async fn breakpoints_are_kept_and_replayed_to_a_child_the_adapter_starts() {
    let dir = TempDir::new("dap-tree");
    let port = reserved_port();
    let catalog = mock_catalog("dlv", "tcp", Some(port), &json!({}));
    let session = launch(dir.path(), &catalog, "dlv").await;
    let file = dir.path().join("mock.py");
    let added = session
        .edit_breakpoints(
            Kind::Source(file.to_string_lossy().into_owned()),
            Edit::Set(Box::new(Breakpoint {
                line: Some(7),
                condition: Some("i > 1".to_owned()),
                ..Default::default()
            })),
            TIMEOUT,
        )
        .await
        .expect("set_breakpoint");
    assert_eq!(added.breakpoints.len(), 1);
    assert!(added.breakpoints[0].verified);
    assert!(added.warnings.is_empty(), "{:?}", added.warnings);

    // Ask the adapter for a child session; the client serves it on the adapter's own port.
    session
        .request("__child", Value::Null, TIMEOUT)
        .await
        .expect("the mock starts a child");
    let targets = loop_until(|| {
        let session = Arc::clone(&session);
        Box::pin(async move {
            let targets = session.summaries();
            let child_ready = targets.get(1).is_some_and(|t| {
                matches!(t.status, "running" | "stopped") && !t.needs_configuration_done
            });
            (targets.len() >= 2 && child_ready).then_some(targets)
        })
    })
    .await;
    assert_eq!(targets.len(), 2, "expected a child target: {targets:?}");
    assert_eq!(targets[1].id, "debug-1.1");

    // Threads are aggregated over the tree; ids repeat per target, so each is tagged.
    let (threads, warnings) = session.threads(TIMEOUT).await.expect("threads");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(threads.len(), 2, "{threads:?}");
    assert!(
        threads
            .iter()
            .all(|t| t.get("target").and_then(Value::as_str).is_some())
    );

    // The child was configured, and it took the session's breakpoint list first.
    let entries = logged(dir.path());
    let child = sent_to(&entries, "debug-1.1");
    let bound = child
        .iter()
        .position(|c| c == "setBreakpoints")
        .unwrap_or_else(|| panic!("child request order: {child:?}"));
    let configured = child
        .iter()
        .position(|c| c == "configurationDone")
        .expect("child configurationDone");
    assert!(
        bound < configured,
        "breakpoints must precede configurationDone: {child:?}"
    );
    let request = entries
        .iter()
        .find(|m| m["target"] == json!("debug-1.1") && m["command"] == json!("setBreakpoints"))
        .expect("child setBreakpoints");
    assert_eq!(
        request
            .pointer("/arguments/breakpoints/0/condition")
            .and_then(Value::as_str),
        Some("i > 1")
    );
    session.terminate().await;
}

#[tokio::test]
async fn a_resume_reports_the_next_stop_and_the_end_of_the_program() {
    let dir = TempDir::new("dap-resume");
    let catalog = mock_catalog("debugpy", "stdio", None, &json!({}));
    let session = launch(dir.path(), &catalog, "debugpy").await;
    let stepped = session.resume(Resume::Over, TIMEOUT).await.expect("step");
    assert_eq!(stepped.state, RunState::Stopped);
    assert_eq!(stepped.summary.stop_reason.as_deref(), Some("breakpoint"));
    // The new stop location is known without a second round trip.
    assert_eq!(stepped.summary.line, Some(3));
    session.terminate().await;

    // A second session whose program ends on the next resume.
    let dir = TempDir::new("dap-exit");
    let catalog = mock_catalog("debugpy", "stdio", None, &json!({"__die": true}));
    let session = launch(dir.path(), &catalog, "debugpy").await;
    let finished = session
        .resume(Resume::Continue, TIMEOUT)
        .await
        .expect("continue");
    assert_eq!(finished.state, RunState::Terminated);
    assert_eq!(finished.summary.exit_code, Some(0));
    // With every target ended, the session is dead to the manager.
    assert!(!session.is_live());
    assert_eq!(session.summary().status, "terminated");
}

#[tokio::test]
async fn a_program_that_keeps_running_is_reported_as_running() {
    let dir = TempDir::new("dap-hang");
    let catalog = mock_catalog("debugpy", "stdio", None, &json!({"__hang": true}));
    let session = launch(dir.path(), &catalog, "debugpy").await;
    let outcome = session.resume(Resume::In, FAST).await.expect("step in");
    assert_eq!(outcome.state, RunState::Running);
    assert!(outcome.timed_out);
    let paused = session.pause(FAST).await.expect("pause");
    assert_eq!(paused.state, RunState::Stopped);
    assert_eq!(paused.summary.stop_reason.as_deref(), Some("pause"));
    session.terminate().await;
}

#[tokio::test]
async fn a_terminal_the_adapter_asks_for_has_its_output_captured() {
    let dir = TempDir::new("dap-terminal");
    let catalog = mock_catalog("debugpy", "stdio", None, &json!({"__term": true}));
    let session = launch(dir.path(), &catalog, "debugpy").await;
    let body = session
        .request(
            "evaluate",
            json!({"expression": "run it", "context": "repl"}),
            TIMEOUT,
        )
        .await
        .expect("evaluate through runInTerminal");
    assert!(
        body.get("result")
            .and_then(Value::as_str)
            .is_some_and(|r| r.starts_with("ran ")),
        "{body}"
    );
    // The command prints once and leaves; its stdout lands in the capture.
    let mut seen = String::new();
    for _ in 0..40 {
        seen = session.output();
        if seen.contains("terminal output") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seen.contains("terminal output"), "{seen}");
    session.terminate().await;
}

#[tokio::test]
async fn terminate_ends_the_debuggee_and_empties_the_breakpoint_list() {
    let dir = TempDir::new("dap-terminate");
    let catalog = mock_catalog("debugpy", "stdio", None, &json!({}));
    let session = launch(dir.path(), &catalog, "debugpy").await;
    let file = dir.path().join("mock.py").to_string_lossy().into_owned();
    let kind = Kind::Source(file);
    session
        .edit_breakpoints(
            kind.clone(),
            Edit::Set(Box::new(Breakpoint {
                line: Some(3),
                ..Default::default()
            })),
            TIMEOUT,
        )
        .await
        .expect("set");
    session
        .edit_breakpoints(
            Kind::Function,
            Edit::Set(Box::new(Breakpoint {
                name: Some("main".to_owned()),
                ..Default::default()
            })),
            TIMEOUT,
        )
        .await
        .expect("function breakpoint");
    let removed = session
        .edit_breakpoints(kind, Edit::Remove(Box::new(|b| b.line == Some(3))), TIMEOUT)
        .await
        .expect("remove");
    assert!(removed.breakpoints.is_empty());

    let (_, notes) = session.terminate().await;
    assert!(notes.is_empty(), "{notes:?}");
    assert!(!session.is_live());
    let entries = logged(dir.path());
    let last = entries.last().expect("requests logged");
    assert_eq!(last["command"], json!("disconnect"));
    assert_eq!(last["arguments"]["terminateDebuggee"], json!(true));
    // The capture survives the session, so `output` can still be read.
    assert!(session.output().contains("hello from mock"));
}

#[tokio::test]
async fn tcp_session_covers_launch_breakpoints_evaluate_step_attach_and_disconnect() {
    let dir = TempDir::new("dap-tcp-e2e");
    let catalog = mock_catalog("dlv", "tcp", Some(reserved_port()), &json!({}));
    let (session, outcome) = start(dir.path(), &catalog, "dlv")
        .await
        .expect("tcp launch");
    assert_eq!(session.connect(), Connect::Tcp);
    assert_eq!(outcome.state, RunState::Stopped);
    assert_eq!(outcome.summary.stop_reason.as_deref(), Some("entry"));

    let file = dir.path().join("mock.py").to_string_lossy().into_owned();
    let bp = session
        .edit_breakpoints(
            Kind::Source(file),
            Edit::Set(Box::new(Breakpoint {
                line: Some(3),
                ..Default::default()
            })),
            TIMEOUT,
        )
        .await
        .expect("tcp setBreakpoints");
    assert_eq!(bp.breakpoints.len(), 1);
    assert!(bp.breakpoints[0].verified);

    let frames = session.stack_trace(Some(5), TIMEOUT).await.expect("stack");
    assert_eq!(frames.len(), 1);
    let eval = session
        .request(
            "evaluate",
            json!({"expression": "2+2", "context": "repl"}),
            TIMEOUT,
        )
        .await
        .expect("tcp evaluate");
    assert_eq!(eval["result"], json!("mock(2+2)"));

    let stepped = session
        .resume(Resume::Over, TIMEOUT)
        .await
        .expect("tcp step");
    assert_eq!(stepped.state, RunState::Stopped);
    assert_eq!(stepped.summary.stop_reason.as_deref(), Some("breakpoint"));

    let (_, notes) = session.terminate().await;
    assert!(notes.is_empty(), "{notes:?}");

    // Exercise StartRequest::Attach over Connect::Tcp and verify terminateDebuggee = false.
    let dir_attach = TempDir::new("dap-tcp-attach");
    let Selection::Adapter(mut adapter) = catalog
        .select_launch(
            &dir_attach.path().join("mock.py"),
            dir_attach.path(),
            Some("dlv"),
            ProgramKind::File,
        )
        .await
    else {
        panic!("dlv mock must resolve");
    };
    adapter.env.insert(
        "HRN_MOCK_LOG".into(),
        dir_attach
            .path()
            .join("requests.json")
            .to_string_lossy()
            .into_owned(),
    );
    let mut attach_args = adapter.attach.clone();
    attach_args.insert("name".into(), json!("debug-attach"));
    let (attached, outcome) = Session::start(StartSpec {
        id: "debug-attach".to_owned(),
        adapter: *adapter,
        cwd: dir_attach.path().to_path_buf(),
        program: None,
        request: StartRequest::Attach(attach_args),
        timeout: TIMEOUT,
    })
    .await
    .expect("tcp attach");
    assert_eq!(attached.connect(), Connect::Tcp);
    assert_eq!(outcome.state, RunState::Stopped);
    let (_, notes) = attached.terminate().await;
    assert!(notes.is_empty(), "{notes:?}");
    let entries = logged(dir_attach.path());
    assert!(entries.iter().any(|e| e["command"] == json!("attach")));
    let last = entries.last().expect("attach log");
    assert_eq!(last["command"], json!("disconnect"));
    assert_eq!(last["arguments"]["terminateDebuggee"], json!(false));
}

struct EventRecorder {
    initialized: tokio::sync::Notify,
}

impl Inbound for EventRecorder {
    fn event(&self, name: &str, _body: Value) {
        if name == "initialized" {
            self.initialized.notify_waiters();
        }
    }

    fn reverse_request(&self, command: String, _args: Value) -> BoxFuture<HrnResult<Value>> {
        Box::pin(async move { Err(HrnError::Tool(format!("unexpected reverse {command}"))) })
    }

    fn closed(&self) {}
}

#[tokio::test]
async fn connect_tcp_attaches_directly_to_an_already_listening_adapter() {
    use std::process::{Command, Stdio};

    let port = reserved_port();
    let mut child = Command::new(exe())
        .args(["--listen", &port.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn listening mock-adapter");

    let recorder = Arc::new(EventRecorder {
        initialized: tokio::sync::Notify::new(),
    });
    let mut client = None;
    for _ in 0..40 {
        match DapClient::connect_tcp(
            "127.0.0.1",
            port,
            FAST,
            Arc::clone(&recorder) as Arc<dyn Inbound>,
        )
        .await
        {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let client = client.expect("connect_tcp to listening adapter");
    assert_eq!(client.port(), Some(port));

    let caps = client
        .request("initialize", json!({"adapterID": "tcp-direct"}), TIMEOUT)
        .await
        .expect("initialize");
    assert_eq!(caps["supportsConfigurationDoneRequest"], json!(true));

    let init_wait = recorder.initialized.notified();
    let attach = client
        .send("attach", json!({"name": "direct"}))
        .await
        .expect("send attach");
    tokio::time::timeout(TIMEOUT, init_wait)
        .await
        .expect("initialized event over TCP");
    client
        .request("configurationDone", json!({}), TIMEOUT)
        .await
        .expect("configurationDone");
    attach.wait(TIMEOUT).await.expect("attach response");

    let eval = client
        .request("evaluate", json!({"expression": "tcp_ok"}), TIMEOUT)
        .await
        .expect("evaluate over direct TCP");
    assert_eq!(eval["result"], json!("mock(tcp_ok)"));

    let _ = client
        .request("disconnect", json!({"terminateDebuggee": false}), TIMEOUT)
        .await;
    drop(client);
    let _ = child.kill();
    let _ = child.wait();
}
