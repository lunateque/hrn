//! RPC acceptance tests for `set_fast_mode`, `set_slow_mode`, and `get_state` tier fields.

use super::*;
use crate::test::Mock;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, duplex};

fn opts() -> Options {
    Options {
        cwd: PathBuf::from("."),
        tool_ui: true,
        approvals: true,
        auto_approve: false,
    }
}

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

    async fn call(&mut self, line: &str) -> Value {
        self.input
            .write_all(line.as_bytes())
            .await
            .expect("stdin write");
        self.input.write_all(b"\n").await.expect("stdin write");
        self.frame().await
    }

    async fn frame(&mut self) -> Value {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), self.output.read_line(&mut line))
            .await
            .expect("frame timeout")
            .expect("read line");
        assert!(n > 0, "unexpected EOF");
        serde_json::from_str(&line).expect("json frame")
    }

    async fn close(self) {
        drop(self.input);
        self.task.await.expect("join").expect("serve ok");
    }
}

#[tokio::test]
async fn rpc_set_fast_mode_real() {
    let mock = Arc::new(Mock::default());
    mock.set_fast_supported(true);
    mock.set_slow_supported(true);
    let mut conn = Conn::start(Arc::clone(&mock)).await;

    let res = conn
        .call(r#"{"id":"1","type":"set_fast_mode","enabled":true}"#)
        .await;
    assert_eq!(res["success"], json!(true), "{res}");
    assert_eq!(res["data"], json!({"enabled": true, "active": true}));
    assert!(mock.fast_mode_enabled());

    let state = conn.call(r#"{"id":"2","type":"get_state"}"#).await;
    assert_eq!(state["data"]["fastModeEnabled"], json!(true));
    assert_eq!(state["data"]["fastModeActive"], json!(true));
    assert_eq!(state["data"]["slowModeSupported"], json!(true));
    assert_eq!(state["data"]["slowModeEnabled"], json!(false));

    let slow = conn
        .call(r#"{"id":"3","type":"set_slow_mode","enabled":true}"#)
        .await;
    assert_eq!(slow["success"], json!(true), "{slow}");
    assert_eq!(slow["data"], json!({"enabled": true, "active": true}));
    assert!(mock.slow_mode_enabled());

    let off = conn
        .call(r#"{"id":"4","type":"set_fast_mode","enabled":false}"#)
        .await;
    assert_eq!(off["success"], json!(true), "{off}");
    assert_eq!(off["data"], json!({"enabled": false, "active": false}));

    // When the current model lacks service-tier support, enabling fails with the exact message.
    mock.set_fast_supported(false);
    let unavail = conn
        .call(r#"{"id":"5","type":"set_fast_mode","enabled":true}"#)
        .await;
    assert_eq!(unavail["success"], json!(false), "{unavail}");
    assert_eq!(
        unavail["error"],
        json!("Fast mode is unavailable for the current model.")
    );
    conn.close().await;
}
