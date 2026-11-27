//! ACP framing tests: a malformed line still gets a JSON-RPC error on stdin, and a session
//! opens with the client's `cwd`.

use super::*;
use crate::test::Mock;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

/// One ACP exchange over a duplex pipe.
struct Conn2 {
    input: tokio::io::DuplexStream,
    output: BufReader<tokio::io::DuplexStream>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Conn2 {
    fn start() -> Self {
        Self::start_with(
            std::sync::Arc::new(Mock::default()),
            Options {
                cwd: PathBuf::from("."),
                approvals: true,
                auto_approve: true,
            },
        )
    }

    fn start_with(backend: std::sync::Arc<Mock>, opts: Options) -> Self {
        let (mine, theirs) = duplex(64 * 1024);
        let (ours, readers) = duplex(64 * 1024);
        let task = tokio::spawn(serve(
            backend as std::sync::Arc<dyn Backend>,
            opts,
            theirs,
            ours,
        ));
        Self {
            input: mine,
            output: BufReader::new(readers),
            task,
        }
    }

    async fn send(&mut self, line: &str) {
        self.input.write_all(line.as_bytes()).await.expect("stdin");
        self.input.write_all(b"\n").await.expect("stdin");
    }

    async fn frame(&mut self) -> serde_json::Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), self.output.read_line(&mut line))
            .await
            .expect("a frame in time")
            .expect("stdout readable");
        serde_json::from_str(&line).expect("a JSON frame")
    }

    /// Frames until one answers request `id`.
    async fn response(&mut self, id: i64) -> serde_json::Value {
        for _ in 0..30 {
            let frame = self.frame().await;
            if frame["id"] == serde_json::json!(id) {
                return frame;
            }
        }
        panic!("no response to id={id}");
    }
}

#[tokio::test]
async fn a_malformed_line_answers_with_a_jsonrpc_parse_error() {
    let mut conn = Conn2::start();
    conn.send("{oops").await;
    let err = conn.frame().await;
    assert_eq!(err["error"]["code"], serde_json::json!(-32700));
    // the connection recovers and answers the next request normally
    conn.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#,
    )
    .await;
    let init = conn.response(1).await;
    assert_eq!(init["result"]["protocolVersion"], serde_json::json!(1));
    conn.input.write_all(b"").await.ok();
    drop(conn.input);
    conn.task.await.expect("no panic").expect("clean exit");
}

#[tokio::test]
async fn session_new_answers_with_a_session_id() {
    let mut conn = Conn2::start();
    conn.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":".","mcpServers":[]}}"#,
    )
    .await;
    let opened = conn.response(2).await;
    assert!(
        opened["result"]["sessionId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{opened}"
    );
    let mode_ids: Vec<&str> = opened["result"]["modes"]["availableModes"]
        .as_array()
        .expect("availableModes")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(mode_ids.contains(&"askCached"), "{mode_ids:?}");
}

#[tokio::test]
async fn acp_no_ui_omits_approval_gate_and_forgets_approvals() {
    let backend = std::sync::Arc::new(Mock::new(vec![crate::test::Script::Events {
        approvals: vec![crate::test::approval("c1", "bash")],
        forms: Vec::new(),
        events: crate::test::answer("ok"),
        summary: crate::test::done("ok"),
    }]));
    let mut conn = Conn2::start_with(
        std::sync::Arc::clone(&backend),
        Options {
            cwd: PathBuf::from("."),
            approvals: false,
            auto_approve: false,
        },
    );
    conn.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":".","mcpServers":[]}}"#,
    )
    .await;
    let opened = conn.response(1).await;
    let sid = opened["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_owned();
    conn.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{{"sessionId":"{sid}","prompt":[{{"type":"text","text":"run"}}]}}}}"#
    ))
    .await;
    let done = conn.response(2).await;
    assert_eq!(done["result"]["stopReason"], serde_json::json!("end_turn"));
    // With `approvals: false` (`--no-ui`), no `ApprovalGate` was passed to the backend so the
    // scripted approval had no gate to approve it.
    assert!(backend.with_calls(|c| c.decisions.is_empty()));

    conn.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/forget_approvals","params":{}}"#)
        .await;
    let forgot = conn.response(3).await;
    assert!(forgot.get("result").is_some(), "{forgot}");
}
