//! RPC connection tests: framing, the prompt lifecycle, queues and dialogs, driven over a duplex
//! pipe exactly like a host drives the real stdin.

use super::*;
use crate::test::{Mock, Script, answer, approval, done};
use hrn_kernel::{ApprovalGate, ApprovalRequest, AskRequest, Form};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::io::duplex;

/// Options a test connection starts with.
fn opts() -> Options {
    Options {
        cwd: PathBuf::from("."),
        tool_ui: true,
        approvals: true,
        auto_approve: false,
    }
}

/// One scripted answer turn.
fn events_script(text: &str) -> Vec<Script> {
    vec![Script::Events {
        approvals: Vec::new(),
        forms: Vec::new(),
        events: answer(text),
        summary: done(text),
    }]
}

/// A live connection plus the test's ends of its pipes.
struct Conn {
    input: tokio::io::DuplexStream,
    output: BufReader<tokio::io::DuplexStream>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Conn {
    async fn start(backend: Arc<Mock>) -> Self {
        let (mine, theirs) = duplex(64 * 1024);
        let (ours, readers) = duplex(64 * 1024);
        let task = tokio::spawn(serve(backend as Arc<dyn Backend>, opts(), theirs, ours));
        let mut conn = Self {
            input: mine,
            output: BufReader::new(readers),
            task,
        };
        let ready = conn.frame().await;
        assert_eq!(ready["type"], json!("ready"));
        let commands = conn.frame().await;
        assert_eq!(commands["type"], json!("available_commands_update"));
        conn
    }

    /// Send one command line.
    async fn send(&mut self, line: &str) {
        self.input
            .write_all(line.as_bytes())
            .await
            .expect("stdin write");
        self.input.write_all(b"\n").await.expect("stdin write");
    }

    /// The next frame the connection wrote.
    async fn frame(&mut self) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(5), self.output.read_line(&mut line))
            .await
            .expect("a frame in time")
            .expect("stdout readable");
        assert!(read > 0, "the connection closed its output");
        serde_json::from_str(&line).expect("a JSON frame")
    }

    /// Frames until one of `type` arrives.
    async fn until(&mut self, want: &str) -> Vec<Value> {
        let mut seen = Vec::new();
        for _ in 0..60 {
            let frame = self.frame().await;
            let done = frame["type"].as_str() == Some(want);
            seen.push(frame);
            if done {
                return seen;
            }
        }
        panic!("no `{want}` frame: {seen:?}");
    }

    /// The next `response` frame for command `command`.
    async fn response(&mut self, command: &str) -> Value {
        for _ in 0..60 {
            let frame = self.frame().await;
            if frame["type"] == "response" && frame["command"] == command {
                return frame;
            }
        }
        panic!("no `{command}` response");
    }
}

#[tokio::test]
async fn a_good_turn_streams_events_and_reports_its_result() {
    let mut conn = Conn::start(Arc::new(Mock::new(events_script("hello")))).await;
    conn.send(r#"{"type":"prompt","id":"1","message":"hi"}"#)
        .await;
    let ack = conn.response("prompt").await;
    assert_eq!(ack["success"], json!(true));
    let frames = conn.until("session_settled").await;
    let kinds: Vec<&str> = frames.iter().filter_map(|f| f["type"].as_str()).collect();
    for want in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_update",
        "message_end",
        "turn_end",
        "agent_end",
        "prompt_result",
        "session_settled",
    ] {
        assert!(kinds.contains(&want), "missing {want} in {kinds:?}");
    }
    let result = frames
        .iter()
        .find(|f| f["type"] == "prompt_result")
        .expect("a result");
    assert_eq!(result["status"], json!("completed"));
    assert_eq!(result["sessionSettled"], json!(true));
    conn.send(r#"{"type":"get_state","id":"2"}"#).await;
    let state = conn.response("get_state").await;
    assert_eq!(state["data"]["isStreaming"], json!(false));
    assert_eq!(state["data"]["model"]["id"], json!("m1"));
    assert!(state["data"].get("sessionId").is_some());
}

#[tokio::test]
async fn a_broken_line_is_reported_and_the_connection_lives() {
    let mut conn = Conn::start(Arc::new(Mock::default())).await;
    conn.send("{oops").await;
    let parse = conn.frame().await;
    assert_eq!(parse["command"], json!("parse"));
    assert_eq!(parse["success"], json!(false));
    conn.send(r#"{"type":"nonsense"}"#).await;
    let unknown = conn.frame().await;
    assert_eq!(unknown["error"], json!("Unknown command: nonsense"));
    conn.send(r#"{"type":"get_state"}"#).await;
    let state = conn.response("get_state").await;
    assert_eq!(state["success"], json!(true));
}

#[tokio::test]
async fn a_prompt_met_by_a_live_run_must_name_its_behavior() {
    let backend = Arc::new(Mock::new(vec![
        Script::Hold {
            approvals: Vec::new(),
            forms: Vec::new(),
            events: answer("first"),
        },
        Script::Done(done("second")),
    ]));
    let mut conn = Conn::start(Arc::clone(&backend)).await;
    conn.send(r#"{"type":"prompt","id":"1","message":"one"}"#)
        .await;
    let _ = conn.response("prompt").await;
    backend.wait_prompts(1).await;
    conn.send(r#"{"type":"prompt","id":"2","message":"two"}"#)
        .await;
    let busy = conn.response("prompt").await;
    assert_eq!(busy["success"], json!(false));
    assert!(
        busy["error"]
            .as_str()
            .is_some_and(|e| e.contains("already processing")),
        "{busy}"
    );
    conn.send(r#"{"type":"prompt","id":"3","message":"later","streamingBehavior":"followUp"}"#)
        .await;
    let queued = conn.response("prompt").await;
    assert_eq!(queued["success"], json!(true));
    let queue = conn.until("queue_update").await;
    let last = queue.last().expect("a queue frame");
    assert_eq!(last["followUp"], json!(["later"]));
    // The follow-up runs as its own turn; both prompts are answered.
    backend.release_held(done("first"));
    let results = conn.until("session_settled").await;
    let ids: Vec<&str> = results
        .iter()
        .filter(|f| f["type"] == "prompt_result")
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert_eq!(ids, ["1", "3"]);
}

#[tokio::test]
async fn an_approval_round_trips_and_remembers_a_session_grant() {
    let request = approval("call-1", "write");
    let backend = Arc::new(Mock::new(vec![
        Script::Events {
            approvals: vec![request.clone()],
            forms: Vec::new(),
            events: answer("written"),
            summary: done("written"),
        },
        Script::Events {
            approvals: vec![request.clone()],
            forms: Vec::new(),
            events: answer("again"),
            summary: done("again"),
        },
    ]));
    let mut conn = Conn::start(Arc::clone(&backend)).await;
    conn.send(r#"{"type":"prompt","id":"1","message":"go"}"#)
        .await;
    let dialog = conn.until("extension_ui_request").await;
    let ask = dialog.last().expect("a dialog");
    assert_eq!(ask["method"], json!("select"));
    assert_eq!(ask["options"][0], json!("Approve"));
    let id = ask["id"].clone();
    conn.send(&format!(
        r#"{{"type":"extension_ui_response","id":{id},"value":"Approve for this session"}}"#
    ))
    .await;
    conn.until("session_settled").await;
    // The grant holds: the second run asks its gate and no dialog goes out.
    conn.send(r#"{"type":"prompt","id":"2","message":"go again"}"#)
        .await;
    let frames = conn.until("session_settled").await;
    assert!(
        !frames
            .iter()
            .any(|f| f["type"] == "extension_ui_request" && f["method"] == "select"),
        "the granted tool asked again: {frames:?}"
    );
    let decisions = backend.with_calls(|calls| calls.decisions.clone());
    assert_eq!(decisions, vec![true, true]);
}

#[tokio::test]
async fn a_denied_approval_reaches_the_runtime_as_a_refusal() {
    let backend = Arc::new(Mock::new(vec![Script::Events {
        approvals: vec![approval("call-1", "bash")],
        forms: Vec::new(),
        events: answer("no"),
        summary: done("no"),
    }]));
    let mut conn = Conn::start(Arc::clone(&backend)).await;
    conn.send(r#"{"type":"prompt","id":"1","message":"run it"}"#)
        .await;
    let frames = conn.until("extension_ui_request").await;
    let id = frames.last().expect("a dialog")["id"].clone();
    conn.send(&format!(
        r#"{{"type":"extension_ui_response","id":{id},"value":"Deny"}}"#
    ))
    .await;
    conn.until("session_settled").await;
    let decisions = backend.with_calls(|calls| calls.decisions.clone());
    assert_eq!(decisions, vec![false]);
}

#[tokio::test]
async fn a_form_answer_becomes_the_submission_the_model_sees() {
    let form = Form::parse(
        "<form><fieldset><legend>Pick</legend>\
         <label><input type=radio name=a value=x checked> X</label>\
         <label><input type=radio name=a value=y> Y</label></fieldset></form>",
    )
    .expect("a form");
    let backend = Arc::new(Mock::new(vec![Script::Events {
        approvals: Vec::new(),
        forms: vec![AskRequest {
            id: "ask-1".to_owned(),
            session_id: "session-1".to_owned(),
            form,
        }],
        events: answer("picked"),
        summary: done("picked"),
    }]));
    let mut conn = Conn::start(Arc::clone(&backend)).await;
    conn.send(r#"{"type":"prompt","id":"1","message":"ask"}"#)
        .await;
    let frames = conn.until("extension_ui_request").await;
    let ask = frames.last().expect("a dialog");
    assert_eq!(ask["method"], json!("form"));
    assert!(
        ask["html"]
            .as_str()
            .is_some_and(|h| h.contains("name=\"a\"") && h.contains("value=\"y\"")),
        "{ask}"
    );
    let id = ask["id"].clone();
    conn.send(&format!(
        r#"{{"type":"extension_ui_response","id":{id},"data":{{"a":"y"}}}}"#
    ))
    .await;
    conn.until("session_settled").await;
    let submissions = backend.with_calls(|calls| calls.submissions.clone());
    assert_eq!(submissions, vec![json!({"a": "y"})]);
}

#[tokio::test]
async fn an_abort_stops_the_run_and_reports_its_prompts_aborted() {
    let backend = Arc::new(Mock::new(vec![Script::Hold {
        approvals: Vec::new(),
        forms: Vec::new(),
        events: answer("long"),
    }]));
    let mut conn = Conn::start(Arc::clone(&backend)).await;
    conn.send(r#"{"type":"prompt","id":"1","message":"long"}"#)
        .await;
    let _ = conn.response("prompt").await;
    backend.wait_prompts(1).await;
    conn.send(r#"{"type":"abort","id":"2"}"#).await;
    let result = conn.until("prompt_result").await;
    let aborted = result
        .iter()
        .find(|f| f["type"] == "prompt_result")
        .expect("a result");
    assert_eq!(aborted["status"], json!("aborted"));
    let state = conn.response("abort").await;
    assert_eq!(state["success"], json!(true));
    assert_eq!(backend.with_calls(|calls| calls.held_done), 1);
}

#[tokio::test]
async fn a_failing_run_ends_settled_with_its_error_reported() {
    let backend = Arc::new(Mock::new(vec![Script::Fail("boom".to_owned())]));
    let mut conn = Conn::start(Arc::clone(&backend)).await;
    conn.send(r#"{"type":"prompt","id":"1","message":"go"}"#)
        .await;
    conn.response("prompt").await;
    let frames = conn.until("session_settled").await;
    let result = frames
        .iter()
        .find(|f| f["type"] == "prompt_result")
        .expect("a result");
    assert_eq!(result["status"], json!("error"));
    assert!(
        result["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("boom")),
        "{result}"
    );
    let failed = frames.iter().rev().find_map(|f| {
        (f["type"] == "message_end")
            .then(|| f["message"]["stopReason"].as_str())
            .flatten()
    });
    assert_eq!(failed, Some("error"));
    // the run is over, so the connection is settled again
    conn.send(r#"{"type":"get_state","id":"2"}"#).await;
    let state = conn.response("get_state").await;
    assert_eq!(state["data"]["isSettled"], json!(true));
    // background work keeps the session unsettled until it is gone
    backend.set_pending_async(true);
    conn.send(r#"{"type":"get_state","id":"3"}"#).await;
    let busy = conn.response("get_state").await;
    assert_eq!(busy["data"]["hasPendingAsyncWork"], json!(true));
    assert_eq!(busy["data"]["isSettled"], json!(false));
    backend.set_pending_async(false);
}

#[tokio::test]
async fn set_model_and_compact_answer_with_the_data_hosts_read() {
    let mut conn = Conn::start(Arc::new(Mock::new(events_script("ok")))).await;
    conn.send(r#"{"type":"set_model","id":"1","provider":"mock","modelId":"m1"}"#)
        .await;
    let set = conn.response("set_model").await;
    assert_eq!(set["data"]["id"], json!("m1"));
    assert_eq!(set["data"]["current"], json!(true));
    conn.send(r#"{"type":"set_model","id":"2","provider":"mock","modelId":"nope"}"#)
        .await;
    let miss = conn.response("set_model").await;
    assert_eq!(miss["success"], json!(false));
    assert!(
        miss["error"]
            .as_str()
            .is_some_and(|e| e.contains("Model not found: mock/nope")),
        "{miss}"
    );
    conn.send(r#"{"type":"compact","id":"3"}"#).await;
    let events = conn.until("manual_compaction_end").await;
    assert!(
        events
            .iter()
            .any(|f| f["type"] == "manual_compaction_start")
    );
    let end = events.last().expect("an end frame");
    assert_eq!(end["result"]["firstKeptEntryId"], json!("n1"));
    let response = conn.response("compact").await;
    assert_eq!(response["success"], json!(true));
    assert_eq!(response["data"]["summary"], json!("summary"));
}

#[tokio::test]
async fn an_event_filter_chooses_which_frames_arrive() {
    let mut conn = Conn::start(Arc::new(Mock::new(events_script("hi")))).await;
    conn.send(r#"{"type":"set_event_filter","events":["agent_start","agent_end"]}"#)
        .await;
    let filter = conn.response("set_event_filter").await;
    assert_eq!(
        filter["data"]["events"],
        json!(["agent_start", "agent_end"])
    );
    conn.send(r#"{"type":"prompt","id":"1","message":"hi"}"#)
        .await;
    let frames = conn.until("session_settled").await;
    let kinds: Vec<&str> = frames
        .iter()
        .filter(|f| {
            !["response", "prompt_result", "session_settled"]
                .contains(&f["type"].as_str().unwrap_or(""))
        })
        .filter_map(|f| f["type"].as_str())
        .collect();
    assert_eq!(kinds, ["agent_start", "agent_end"]);
}

#[tokio::test]
async fn stdin_close_lets_a_live_run_finish_and_exits_clean() {
    let backend = Arc::new(Mock::new(vec![Script::Hold {
        approvals: Vec::new(),
        forms: Vec::new(),
        events: answer("long"),
    }]));
    let mut conn = Conn::start(Arc::clone(&backend)).await;
    conn.send(r#"{"type":"prompt","id":"1","message":"long"}"#)
        .await;
    let _ = conn.response("prompt").await;
    backend.wait_prompts(1).await;
    drop(std::mem::replace(&mut conn.input, duplex(1).0));
    backend.release_held(done("finished"));
    let result = tokio::time::timeout(Duration::from_secs(5), &mut conn.task)
        .await
        .expect("the server exits")
        .expect("no panic");
    assert!(result.is_ok());
}

/// The approval dialog's shape when a test builds a gate directly.
#[tokio::test]
async fn an_unanswered_approval_is_refused_once_the_dialog_dies() {
    let core = Arc::new(Core {
        backend: Arc::new(Mock::default()) as Arc<dyn Backend>,
        out: Out::spawn(tokio::io::sink(), None).0,
        pending: Pending::default(),
        opts: opts(),
        idle: Notify::new(),
        state: Mutex::new(initial_state(&Mock::default(), &opts()).expect("defaults")),
    });
    let gate = ui::Gate::new(Arc::clone(&core));
    let request = ApprovalRequest {
        id: "c".to_owned(),
        session_id: "s".to_owned(),
        tool_name: "write".to_owned(),
        summary: "s".to_owned(),
        diff: None,
        reason: None,
        targets: Vec::new(),
        choices: Vec::new(),
        tier: None,
    };
    let cancel = hrn_kernel::CancelFlag::new();
    let wait = gate.await_decision(request, cancel.clone());
    cancel.cancel();
    drop(core);
    assert!(!wait.await.expect("a decision").is_approved());
}

#[tokio::test]
async fn approval_exposes_all_grant_choices_and_forget_approvals() {
    let mut full = approval("call-all", "write");
    full.targets = vec!["src/lib.rs".to_owned()];
    full.choices = vec![
        hrn_kernel::ApprovalGrantScope::Once,
        hrn_kernel::ApprovalGrantScope::File,
        hrn_kernel::ApprovalGrantScope::Directory,
        hrn_kernel::ApprovalGrantScope::ToolSession,
        hrn_kernel::ApprovalGrantScope::Persistent,
    ];
    let mut plain = approval("call-restricted", "read");
    plain.reason = Some("target is inside restricted folder `~/.ssh`".to_owned());
    plain.choices = vec![hrn_kernel::ApprovalGrantScope::Once];

    let backend = Arc::new(Mock::new(vec![Script::Events {
        approvals: vec![full, plain],
        forms: Vec::new(),
        events: answer("done"),
        summary: done("done"),
    }]));
    let mut conn = Conn::start(Arc::clone(&backend)).await;
    conn.send(r#"{"type":"prompt","id":"1","message":"go"}"#)
        .await;
    let first_frames = conn.until("extension_ui_request").await;
    let first = first_frames.last().expect("first dialog");
    assert_eq!(
        first["options"],
        json!([
            "Approve",
            "Approve for this file",
            "Approve for this directory",
            "Approve for this session",
            "Remember across sessions",
            "Deny",
        ])
    );
    let id1 = first["id"].clone();
    conn.send(&format!(
        r#"{{"type":"extension_ui_response","id":{id1},"value":"Remember across sessions"}}"#
    ))
    .await;

    let second_frames = conn.until("extension_ui_request").await;
    let second = second_frames.last().expect("restricted dialog");
    assert_eq!(second["options"], json!(["Approve", "Deny"]));
    let id2 = second["id"].clone();
    conn.send(&format!(
        r#"{{"type":"extension_ui_response","id":{id2},"value":"Approve"}}"#
    ))
    .await;
    conn.until("session_settled").await;

    conn.send(r#"{"type":"forget_approvals","id":"2"}"#).await;
    let cleared = conn.response("forget_approvals").await;
    assert_eq!(cleared["success"], json!(true));
}

#[tokio::test]
async fn rpc_rejects_forged_scope_and_enforces_tier_safe_grants() {
    let mut restricted = approval("call-r", "read");
    restricted.choices = vec![hrn_kernel::ApprovalGrantScope::Once];
    restricted.tier = Some(hrn_kernel::ToolTier::Read);

    let mut desk_read = approval("call-dr", "desktop");
    desk_read.tier = Some(hrn_kernel::ToolTier::Read);

    let mut desk_exec = approval("call-de", "desktop");
    desk_exec.tier = Some(hrn_kernel::ToolTier::Exec);

    let backend = Arc::new(Mock::new(vec![Script::Events {
        approvals: vec![restricted, desk_read, desk_exec],
        forms: Vec::new(),
        events: answer("ok"),
        summary: done("ok"),
    }]));
    let mut conn = Conn::start(Arc::clone(&backend)).await;
    conn.send(r#"{"type":"prompt","id":"1","message":"go"}"#)
        .await;

    // 1. Forging "Approve for this session" on a plain `[Once]` prompt is DENIED.
    let f1 = conn.until("extension_ui_request").await;
    let id1 = f1.last().expect("dialog 1")["id"].clone();
    conn.send(&format!(
        r#"{{"type":"extension_ui_response","id":{id1},"value":"Approve for this session"}}"#
    ))
    .await;

    // 2. Granting `desktop` at `Read` tier for this session...
    let f2 = conn.until("extension_ui_request").await;
    let id2 = f2.last().expect("dialog 2")["id"].clone();
    conn.send(&format!(
        r#"{{"type":"extension_ui_response","id":{id2},"value":"Approve for this session"}}"#
    ))
    .await;

    // 3. ...does NOT auto-approve `desktop` at `Exec` tier (must prompt again!).
    let f3 = conn.until("extension_ui_request").await;
    let id3 = f3.last().expect("dialog 3")["id"].clone();
    conn.send(&format!(
        r#"{{"type":"extension_ui_response","id":{id3},"value":"Approve"}}"#
    ))
    .await;
    conn.until("session_settled").await;

    let decisions = backend.with_calls(|calls| calls.decisions.clone());
    assert_eq!(decisions, vec![false, true, true]);
}
