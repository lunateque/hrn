//! End-to-end tests of the `find` tool over a loopback stub router: the tree is scanned, the
//! excerpts reach a model as one request per batch, and the reply's scores order the hits.

use hrn_edge_http::HttpClient;
use hrn_edge_llm::{LlmConfig, LlmRouter, ModelPreset, ProviderEntry, ProviderKind};
use hrn_edge_oauth::OAuthManager;
use hrn_edge_tools::Chunk;
use hrn_kernel::serde_json::from_str;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::testing::{TempDir, tool_context};
use hrn_kernel::{Host, Registry, Shared, Tool};

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// `<id> <score>` lines for one request: 0.9 for the excerpts of `best`, 0.2 for the rest. The
/// request body is JSON, so it is decoded back to the text the model was sent.
fn reply_for(body: &str, best: &str) -> String {
    let text: Value = from_str(body).expect("a JSON request body");
    let text = text
        .pointer("/messages/1/content")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_owned();
    reply_for_text(&text, best)
}

fn reply_for_text(text: &str, best: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for at in text.match_indices("\n[").map(|(at, _)| at) {
        let head = text
            .get(at + 1..)
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or("");
        let id: String = head
            .chars()
            .skip(1)
            .take_while(char::is_ascii_digit)
            .collect();
        let score = if head.contains(best) { "0.90" } else { "0.20" };
        let _ = writeln!(out, "{id} {score}");
    }
    out
}

/// The streamed answer a `scout` model would give: one delta chunk, the stop chunk, `[DONE]`.
fn completion(content: &str) -> String {
    let delta = json!({
        "id": "cmpl", "object": "chat.completion.chunk", "model": "scout",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": content}}]
    });
    let stop = json!({
        "id": "cmpl", "object": "chat.completion.chunk", "model": "scout",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    format!("data: {delta}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
}

/// Read the request line and headers, then exactly `content_length` body bytes.
fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    let mut body_start = None;
    let mut length = 0usize;
    while stream.read(&mut byte)? == 1 {
        buf.push(byte[0]);
        if body_start.is_none() && buf.ends_with(b"\r\n\r\n") {
            body_start = Some(buf.len());
            length = String::from_utf8_lossy(&buf)
                .to_lowercase()
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
        }
        if let Some(start) = body_start
            && buf.len() >= start + length
        {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(text
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, body)| body.to_owned()))
}

/// A router on a loopback stub. `best` scores the excerpts of that file high; `None` drops every
/// connection without answering. Every request body is recorded.
fn stub_router(best: Option<&'static str>) -> (LlmRouter, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let base_url = format!("http://{}/v1", listener.local_addr().expect("addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let _thread: JoinHandle<()> = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let Some(best) = best else { continue };
            let mut stream = stream;
            let Ok(body) = read_request(&mut stream) else {
                continue;
            };
            let payload = completion(&reply_for(&body, best));
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.flush();
            recorded.lock().expect("slot").push(body);
        }
    });
    let router = LlmRouter::try_new(
        LlmConfig {
            providers: BTreeMap::from([(
                "stub".to_owned(),
                ProviderEntry {
                    kind: ProviderKind::OpenaiChat,
                    base_url: Some(base_url),
                    ..Default::default()
                },
            )]),
            presets: BTreeMap::from([(
                "scout".to_owned(),
                ModelPreset {
                    provider: "stub".to_owned(),
                    id: "scout".to_owned(),
                    ..Default::default()
                },
            )]),
            roles: BTreeMap::from([("find".to_owned(), "scout".to_owned())]),
            ..LlmConfig::default()
        },
        HttpClient::new().expect("http"),
        OAuthManager::default(),
    )
    .expect("router");
    (router, seen)
}

fn find_tool(dir: &TempDir, llm: LlmRouter, config: Value) -> Arc<dyn Tool> {
    let registry = Registry::new(
        Host {
            cwd: dir.path().to_path_buf(),
            data_dir: dir.path().to_path_buf(),
        },
        BTreeMap::from([("find".to_owned(), config)]),
        Arc::new(Shared::new()),
    )
    .with_llm(Arc::new(llm));
    registry
        .module("find", crate::register)
        .and_then(hrn_kernel::Registry::finish)
        .expect("find registers")
        .tools
        .into_iter()
        .find(|t| t.spec().name == "find")
        .expect("find tool")
}

#[tokio::test]
async fn the_model_scores_the_excerpts_and_they_come_back_ranked() {
    let dir = TempDir::new("find-e2e");
    dir.write("notes.md", "an unrelated inbox note\n");
    dir.write(
        "relay.rs",
        "// noise\nfn relay_to_owner() {}\nstruct Mailbox;\n",
    );
    let (router, seen) = stub_router(Some("relay.rs"));
    let tool = find_tool(
        &dir,
        router,
        json!({"batch_chunks": 2, "concurrency": 2, "max_chunk_lines": 12}),
    );
    let ctx = tool_context(dir.path().to_path_buf());
    let args = json!({
        "query": "where are job results handed to the owner",
        "grep_keywords": ["mailbox", "relay", "owner"],
        "path": "."
    });
    let out = tool.execute(&ctx, args).await.expect("find runs");
    assert_eq!(
        out.content.lines().next().unwrap_or_default(),
        "relay.rs:1-3 0.90 fn relay_to_owner() {}",
        "{:?}",
        out.content
    );
    assert!(out.content.contains("[scored "), "{}", out.content);
    let bodies = seen.lock().expect("bodies");
    assert_eq!(bodies.len(), 1, "both excerpts fit one batch: {bodies:?}");
    let asked = bodies.first().expect("a request");
    assert!(asked.contains("relay.rs:1-3"), "{asked}");
    assert!(asked.contains("Query: where are job"), "{asked}");
}

#[tokio::test]
async fn a_model_that_never_answers_fails_loudly() {
    let dir = TempDir::new("find-e2e-dead");
    dir.write("relay.rs", "fn relay_to_owner() {}\n");
    let (router, _seen) = stub_router(None);
    let tool = find_tool(&dir, router, json!({}));
    let ctx = tool_context(dir.path().to_path_buf());
    let args = json!({"query": "relay", "grep_keywords": ["relay"], "path": "."});
    let err = tool
        .execute(&ctx, args)
        .await
        .expect_err("a dead model is an error");
    assert!(err.to_string().contains("scoring model failed"), "{err}");
}

#[test]
fn an_excerpt_head_line_is_scored_by_its_number() {
    let chunk = Chunk {
        path: "a.rs".to_owned(),
        start: 4,
        end: 5,
        text: "4: x\n5: y".to_owned(),
        hint: "x".to_owned(),
    };
    // The request body is JSON; the model itself sees this text.
    let text = format!(
        "\n[1] {}:{}-{}\n{}\n",
        chunk.path, chunk.start, chunk.end, chunk.text
    );
    assert_eq!(reply_for_text(&text, "a.rs"), "1 0.90\n");
    assert_eq!(reply_for_text(&text, "b.rs"), "1 0.20\n");
}
