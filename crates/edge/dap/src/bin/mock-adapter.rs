//! A stand-in DAP adapter for the session tests. It speaks the protocol on stdin/stdout and, when
//! started with `--listen <port>`, also serves further connections on that port: after its own
//! `startDebugging` request the client opens one per child session, exactly as it does against
//! vscode-js-debug. Every request it handles is appended to the file named by `HRN_MOCK_LOG` as
//! one JSON line (`{"target", "command", "arguments"}`), so a test can read the order things
//! happened in. `target` is the `name` from the start request: the root sends its session id, and
//! the client passes that name on to the children it starts.
//!
//! Behaviour switches come from the first start request and apply to every connection:
//!
//! - `__hang`        never stop after `configurationDone` (the program keeps running)
//! - `__tree`        a `__child` request makes the adapter ask for a child session
//!   (`startDebugging`), which the client serves on our listener
//! - `__bad`         reject the start request
//! - `__die`         terminate after a resume
//! - `__term`        answer `evaluate` by asking the client to run a command (`runInTerminal`)

// A test helper binary: loud panics, direct indexing and a single long request match are the
// point here, not library polish.
#![expect(
    clippy::assigning_clones,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "test helper: clarity beats lint-cleanliness"
)]

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::thread;

/// The id of the stdin/stdout connection.
const ROOT: i64 = 1;

static OUT: OnceLock<Mutex<HashMap<i64, Box<dyn Write + Send>>>> = OnceLock::new();
static SEQ: AtomicI64 = AtomicI64::new(1);
static NEXT_ID: AtomicI64 = AtomicI64::new(100);
/// The switches of the first start request, shared by every connection.
static ARGS: OnceLock<Mutex<Option<Value>>> = OnceLock::new();
/// The file every connection appends its request lines to.
static LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();
/// `request_seq` of a `runInTerminal` we sent, mapped to the `evaluate` it answers.
static AWAITING: LazyLock<Mutex<HashMap<i64, (i64, i64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Whether a connection was the first one (the session the client itself started).
static ROOT_CLAIMED: AtomicBool = AtomicBool::new(false);
/// Number of children already asked for, to name them.
static CHILDREN: AtomicI64 = AtomicI64::new(0);

fn claim_root() -> bool {
    !ROOT_CLAIMED.swap(true, Ordering::SeqCst)
}

fn args() -> &'static Mutex<Option<Value>> {
    ARGS.get_or_init(|| Mutex::new(None))
}

fn main() {
    // A copy the client runs for us through `runInTerminal`: print and leave.
    if std::env::args().nth(1).is_some_and(|a| a == "--print-only") {
        let _ = writeln!(io::stdout(), "terminal output");
        return;
    }
    let stdout: Box<dyn Write + Send> = Box::new(io::stdout());
    if OUT
        .set(Mutex::new(HashMap::from([(ROOT, stdout)])))
        .is_err()
    {
        return;
    }
    if let Some(path) = std::env::var_os("HRN_MOCK_LOG") {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("log file");
        let _ = LOG_FILE.set(Mutex::new(file));
    }
    let stdio_root = claim_root();
    if let Some(port) = std::env::args()
        .position(|a| a == "--listen")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|p| p.parse::<u16>().ok())
    {
        let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) else {
            return;
        };
        thread::spawn(move || {
            for stream in listener.incoming().filter_map(Result::ok) {
                let Ok(writer) = stream.try_clone() else {
                    continue;
                };
                let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
                register(id, Box::new(writer));
                let root = claim_root();
                thread::spawn(move || serve(id, stream, root));
            }
        });
    }
    serve(ROOT, io::stdin(), stdio_root);
}

fn register(id: i64, writer: Box<dyn Write + Send>) {
    if let Some(out) = OUT.get()
        && let Ok(mut conns) = out.lock()
    {
        conns.insert(id, writer);
    }
}

fn send(id: i64, message: Value) {
    let Some(out) = OUT.get() else { return };
    let Ok(mut conns) = out.lock() else { return };
    let Some(writer) = conns.get_mut(&id) else {
        return;
    };
    let Ok(payload) = serde_json::to_vec(&message) else {
        return;
    };
    let _ = write!(writer, "Content-Length: {}\r\n\r\n", payload.len());
    let _ = writer.write_all(&payload);
    let _ = writer.flush();
}

fn event(id: i64, name: &str, body: Value) {
    send(
        id,
        json!({"seq": next_seq(), "type": "event", "event": name, "body": body}),
    );
}

fn reply(id: i64, request_seq: i64, command: &str, success: bool, body: Value) {
    send(
        id,
        json!({"seq": next_seq(), "type": "response", "request_seq": request_seq,
                           "command": command, "success": success, "body": body}),
    );
}

fn next_seq() -> i64 {
    SEQ.fetch_add(1, Ordering::SeqCst)
}

fn flag(name: &str) -> bool {
    args()
        .lock()
        .expect("lock")
        .as_ref()
        .is_some_and(|a| a.get(name) == Some(&Value::Bool(true)))
}

fn log(target: &str, command: &str, arguments: &Value) {
    let Some(logger) = LOG_FILE.get() else { return };
    let Ok(line) = serde_json::to_string(&json!(
        {"target": target, "command": command, "arguments": arguments}
    )) else {
        return;
    };
    if let Ok(mut file) = logger.lock() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

fn read_msg<R: BufRead>(reader: &mut R) -> Option<Value> {
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.strip_prefix("Content-Length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Answer the requests of one connection until its peer disconnects.
fn serve<R: Read>(id: i64, stream: R, root: bool) {
    let mut reader = io::BufReader::new(stream);
    let mut target = format!("conn-{id}");
    // Start requests answered only once `configurationDone` arrives, as real adapters do.
    let mut waiting: Vec<(i64, String)> = Vec::new();
    while let Some(message) = read_msg(&mut reader) {
        if message.get("type").and_then(Value::as_str) != Some("request") {
            // The client's answer to a request we sent.
            if message.get("command").and_then(Value::as_str) == Some("runInTerminal") {
                answer_run_in_terminal(&message);
            }
            continue;
        }
        let command = message
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let seq = message.get("seq").and_then(Value::as_i64).unwrap_or(0);
        let empty = json!({});
        let arguments = message.get("arguments").unwrap_or(&empty);
        if let Some(name) = arguments.get("name").and_then(Value::as_str) {
            target = name.to_owned();
        }
        log(&target, &command, arguments);
        if handle(id, &target, &command, seq, arguments, root, &mut waiting) {
            break;
        }
    }
}

/// Turn the client's `runInTerminal` answer into the `evaluate` result it was asked for.
fn answer_run_in_terminal(message: &Value) {
    let seq = message
        .get("request_seq")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let Some((conn, evaluate_seq)) = AWAITING.lock().expect("lock").remove(&seq) else {
        return;
    };
    let pid = message
        .pointer("/body/processId")
        .cloned()
        .unwrap_or(Value::Null);
    let body = json!({"result": format!("ran {pid}")});
    reply(conn, evaluate_seq, "evaluate", true, body);
}

/// Answer one request; `true` when the connection should close afterwards.
fn handle(
    id: i64,
    target: &str,
    command: &str,
    seq: i64,
    arguments: &Value,
    root: bool,
    waiting: &mut Vec<(i64, String)>,
) -> bool {
    match command {
        "initialize" => reply(
            id,
            seq,
            command,
            true,
            json!({"supportsConfigurationDoneRequest": true,
                   "supportsTerminateRequest": true,
                   "supportsFunctionBreakpoints": true,
                   "supportsDataBreakpoints": true,
                   "supportsInstructionBreakpoints": true,
                   "supportsConditionalBreakpoints": true}),
        ),
        "launch" | "attach" => {
            if arguments.get("__bad") == Some(&Value::Bool(true)) {
                send(
                    id,
                    json!({"seq": next_seq(), "type": "response", "request_seq": seq,
                           "command": command, "success": false, "message": "Program not found"}),
                );
                return true;
            }
            let mut saved = args().lock().expect("lock");
            if saved.is_none() {
                *saved = Some(arguments.clone());
            }
            drop(saved);
            if arguments.get("skipAttachRequest") == Some(&Value::Bool(true)) {
                reply(id, seq, command, true, json!({}));
            } else {
                waiting.push((seq, command.to_owned()));
            }
            let _ = root;
            event(id, "initialized", json!({}));
        }
        "configurationDone" => {
            reply(id, seq, command, true, json!({}));
            for (start_seq, start_command) in waiting.drain(..) {
                reply(id, start_seq, &start_command, true, json!({}));
            }
            if !flag("__hang") {
                event(
                    id,
                    "output",
                    json!({"category": "stdout", "output": "hello from mock\n"}),
                );
                event(id, "stopped", json!({"reason": "entry", "threadId": 1}));
            }
        }
        "setBreakpoints"
        | "setFunctionBreakpoints"
        | "setInstructionBreakpoints"
        | "setDataBreakpoints" => {
            let entries: Vec<Value> = arguments
                .pointer("/breakpoints")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let breakpoints: Vec<Value> = entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let mut out = json!({"verified": true, "id": index + 1});
                    for key in ["line", "instructionReference"] {
                        if let Some(value) = entry.get(key) {
                            out[key] = value.clone();
                        }
                    }
                    out
                })
                .collect();
            reply(id, seq, command, true, json!({"breakpoints": breakpoints}));
        }
        "threads" => {
            if flag("__noThreads") {
                send(
                    id,
                    json!({"seq": next_seq(), "type": "response", "request_seq": seq,
                           "command": command, "success": false, "message": "no threads"}),
                );
            } else {
                reply(
                    id,
                    seq,
                    command,
                    true,
                    json!({"threads": [{"id": 1, "name": "main"}]}),
                );
            }
        }
        "stackTrace" => reply(
            id,
            seq,
            command,
            true,
            json!({"stackFrames": [{"id": 0, "name": "main", "line": 3, "column": 1,
                                    "source": {"path": "mock.py"}}],
                   "totalFrames": 1}),
        ),
        "scopes" => reply(
            id,
            seq,
            command,
            true,
            json!({"scopes": [{"name": "Local", "variablesReference": 1000}]}),
        ),
        "variables" => reply(
            id,
            seq,
            command,
            true,
            json!({"variables": [{"name": "total", "value": "5", "variablesReference": 0}]}),
        ),
        "evaluate" => {
            if flag("__term") {
                let exe = std::env::current_exe().expect("exe");
                let request_seq = next_seq();
                AWAITING
                    .lock()
                    .expect("lock")
                    .insert(request_seq, (id, seq));
                send(
                    id,
                    json!({"seq": request_seq, "type": "request", "command": "runInTerminal",
                           "arguments": {"kind": "runInTerminal",
                                         "args": [exe.to_string_lossy(), "--print-only"],
                                         "env": {}}}),
                );
                return false;
            }
            let expression = arguments
                .get("expression")
                .and_then(Value::as_str)
                .unwrap_or("");
            reply(
                id,
                seq,
                command,
                true,
                json!({"result": format!("mock({expression})"), "variablesReference": 0}),
            );
        }
        "continue" | "next" | "stepIn" | "stepOut" => {
            reply(id, seq, command, true, json!({"allThreadsContinued": true}));
            if flag("__hang") {
                return false;
            }
            if flag("__die") {
                event(id, "exited", json!({"exitCode": 0}));
                event(id, "terminated", json!({}));
            } else {
                event(
                    id,
                    "stopped",
                    json!({"reason": "breakpoint", "threadId": 1}),
                );
            }
        }
        "pause" => {
            reply(id, seq, command, true, json!({}));
            event(id, "stopped", json!({"reason": "pause", "threadId": 1}));
        }
        "terminate" => {
            reply(id, seq, command, true, json!({}));
            event(id, "exited", json!({"exitCode": 0}));
            event(id, "terminated", json!({}));
        }
        "disconnect" => {
            reply(id, seq, command, true, json!({}));
            if arguments.get("terminateDebuggee") == Some(&Value::Bool(true)) {
                event(id, "exited", json!({"exitCode": 0}));
                event(id, "terminated", json!({}));
            }
            if root {
                return true;
            }
        }
        "__child" => {
            reply(id, seq, command, true, json!({}));
            let child = CHILDREN.fetch_add(1, Ordering::SeqCst) + 1;
            let request_seq = next_seq();
            send(
                id,
                json!({"seq": request_seq, "type": "request", "command": "startDebugging",
                       "arguments": {"request": "launch",
                                     "configuration": {"name": format!("{target}.{child}"),
                                                       "stopOnEntry": true}}}),
            );
        }
        other => reply(id, seq, other, false, json!({})),
    }
    false
}
