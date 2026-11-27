use super::*;
use hrn_edge_ipc::{MAX_NATIVE_WRITE_BYTES, read_native_message, write_native_message};
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{AgentEvent, BoxFuture, CancelFlag, EventSink, PermissionMode, ToolContext};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

fn make_ctx() -> ToolContext {
    ToolContext {
        session_id: "s".to_owned(),
        turn_id: "t".to_owned(),
        call_id: "c".to_owned(),
        cwd: std::env::temp_dir(),
        permission_mode: PermissionMode::Yolo,
        cancel: CancelFlag::new(),
        events: Arc::new(NullSink),
        ask: None,
        approval: None,
        urls: Arc::default(),
        root_session_id: String::new(),
        inbox: Arc::default(),
        caller: None,
        todos: Arc::new(hrn_kernel::todo::TodoBoards::new()),
        yield_data: Arc::default(),
        is_subagent: false,
    }
}

#[tokio::test]
async fn relay_handles_large_reply_over_1mib_and_keepalive_ping() {
    let dir = std::env::temp_dir().join(format!("hrn-relay-large-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    let (host_in, mut ext_out) = tokio::io::duplex(2 * 1024 * 1024);
    let (mut ext_in, host_out) = tokio::io::duplex(2 * 1024 * 1024);

    let big_value = "Z".repeat(1_200_000);
    let big_clone = big_value.clone();
    let ext_task = tokio::spawn(async move {
        // Send an extension-originated keepalive ping first and expect a pong back.
        write_native_message(&mut ext_out, &json!({ "ping": true }))
            .await
            .expect("send ping");
        let pong = read_native_message(&mut ext_in)
            .await
            .expect("read pong")
            .expect("some pong");
        assert_eq!(pong.get("pong").and_then(Value::as_bool), Some(true));

        let mut chunk_bufs: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        while let Ok(Some(raw)) = read_native_message(&mut ext_in).await {
            let msg = if raw.get("cmd").and_then(Value::as_str) == Some("chunk") {
                let cid = raw.get("chunkId").and_then(Value::as_u64).unwrap_or(0);
                let data = raw
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let last = raw.get("last").and_then(Value::as_bool).unwrap_or(false);
                let entry = chunk_bufs.entry(cid).or_default();
                entry.push(data);
                if !last {
                    continue;
                }
                let joined = chunk_bufs.remove(&cid).unwrap_or_default().concat();
                serde_json::from_str::<Value>(&joined).expect("reassembled json")
            } else {
                raw
            };

            let id = msg.get("id").and_then(Value::as_i64).unwrap_or(0);
            let cmd = msg.get("cmd").and_then(Value::as_str).unwrap_or("");
            let reply = match cmd {
                "open_tab" => {
                    json!({ "id": id, "ok": true, "result": { "tabId": 55, "url": "https://example.test", "title": "Big" } })
                }
                "cdp" if msg.get("method").and_then(Value::as_str) == Some("Runtime.evaluate") => {
                    let expr = msg
                        .pointer("/params/expression")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if expr.len() > 1_000_000 {
                        json!({ "id": id, "ok": true, "result": { "result": { "value": format!("len={}", expr.len()) } } })
                    } else {
                        json!({ "id": id, "ok": true, "result": { "result": { "value": big_clone } } })
                    }
                }
                _ => json!({ "id": id, "ok": true, "result": {} }),
            };
            let bytes = serde_json::to_vec(&reply).expect("encode");
            let len = u32::try_from(bytes.len()).expect("u32");
            if ext_out.write_all(&len.to_le_bytes()).await.is_err()
                || ext_out.write_all(&bytes).await.is_err()
                || ext_out.flush().await.is_err()
            {
                break;
            }
        }
    });

    let cancel = CancelFlag::new();
    let handle = start_relay_over_io(&dir, host_in, host_out, cancel)
        .await
        .expect("start relay");

    let tool = browser_tool(BrowserConfig {
        relay: true,
        data_dir: dir.clone(),
        ..BrowserConfig::default()
    });
    let ctx = make_ctx();

    // 1) Extension -> Host reply > 1 MiB succeeds without hitting the 1 MiB ceiling.
    let res = tool
        .execute(&ctx, json!({ "action": "eval", "expression": "big()" }))
        .await
        .expect("eval >1MiB reply");
    assert!(res.content.len() > MAX_NATIVE_WRITE_BYTES);
    assert_eq!(res.content.len(), 1_200_000);

    // 2) Host -> Extension request > 1 MiB chunks automatically and reassembles cleanly.
    let huge_expr = "a".repeat(1_050_000);
    let res2 = tool
        .execute(&ctx, json!({ "action": "eval", "expression": huge_expr }))
        .await
        .expect("eval >1MiB request");
    assert_eq!(res2.content, "len=1050000");

    handle.shutdown().await;
    ext_task.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
