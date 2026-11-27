//! Helpers shared by the wire adapters: error constructors, strict event parsing, cancellable SSE
//! reads and the streaming tool-call assembler.

use hrn_edge_http::{SseEvent, SseStream};
use hrn_kernel::{CancelFlag, HrnError, Result, ToolCall};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Cancellation poll period while waiting for the next SSE event.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// A provider error that is not worth retrying (protocol violation, rejected request).
pub(crate) fn fatal(message: impl Into<String>) -> HrnError {
    HrnError::Provider {
        message: message.into(),
        retryable: false,
        retry_after_secs: None,
    }
}

/// The stream ended without the provider's terminal marker: a dropped connection, not a
/// complete answer. Retryable before any token reached the user.
pub(crate) fn truncated_stream(provider: &str, expected: &str) -> HrnError {
    HrnError::Provider {
        message: format!("{provider} stream ended before `{expected}` (connection dropped)"),
        retryable: true,
        retry_after_secs: None,
    }
}

/// Parse one SSE `data:` payload; malformed JSON is a protocol error, never silently skipped
/// (a skipped delta would corrupt tool arguments or text without any signal).
pub(crate) fn parse_event(provider: &str, data: &str) -> Result<Value> {
    serde_json::from_str(data).map_err(|e| {
        let head: String = data.chars().take(120).collect();
        fatal(format!("{provider}: malformed stream event ({e}): {head}"))
    })
}

/// Read the next SSE event, returning [`HrnError::Cancelled`] promptly when `cancel` trips even
/// if the connection is stalled (the idle timeout inside the stream keeps running meanwhile).
pub(crate) async fn next_event(
    sse: &mut SseStream,
    cancel: &CancelFlag,
) -> Result<Option<SseEvent>> {
    if cancel.is_cancelled() {
        return Err(HrnError::Cancelled);
    }
    let next = sse.next_event();
    tokio::pin!(next);
    let mut ticker = tokio::time::interval(CANCEL_POLL);
    loop {
        tokio::select! {
            event = &mut next => return event,
            _ = ticker.tick() => {
                if cancel.is_cancelled() {
                    return Err(HrnError::Cancelled);
                }
            }
        }
    }
}

static SYNTHETIC_CALL_ID: AtomicU64 = AtomicU64::new(1);

/// Process-unique tool call id for providers that do not supply one.
pub(crate) fn synthetic_call_id() -> String {
    format!(
        "call_hrn_{}",
        SYNTHETIC_CALL_ID.fetch_add(1, Ordering::Relaxed)
    )
}

/// One tool call being assembled from stream deltas.
#[derive(Default, Debug)]
pub(crate) struct PartialCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) args: String,
    pub(crate) signature: Option<String>,
}

/// Assembles tool calls from interleaved provider deltas.
///
/// Calls keep first-seen order. Two keying schemes are supported: [`Self::by_key`] (Responses API
/// `item_id`) and [`Self::by_index`] (chat-completions `index`, where a second distinct non-empty
/// `id` on an already-used index starts a new call — some gateways reuse index 0 for every call).
#[derive(Default)]
pub(crate) struct ToolCallAssembler {
    calls: Vec<PartialCall>,
    keys: HashMap<String, usize>,
}

impl ToolCallAssembler {
    /// Call addressed by an opaque stable key.
    pub(crate) fn by_key(&mut self, key: &str) -> &mut PartialCall {
        let pos = match self.keys.get(key) {
            Some(&pos) => pos,
            None => self.push_new(key.to_owned()),
        };
        self.slot(pos)
    }

    /// Call addressed by a streamed `index`, split into a fresh call when `id` changes.
    pub(crate) fn by_index(&mut self, index: u64, id: Option<&str>) -> &mut PartialCall {
        let key = format!("#{index}");
        let id = id.filter(|s| !s.is_empty());
        let mut pos = self.keys.get(&key).copied();
        if let (Some(p), Some(new_id)) = (pos, id)
            && self
                .calls
                .get(p)
                .is_some_and(|c| !c.id.is_empty() && c.id != new_id)
        {
            pos = None;
        }
        let pos = match pos {
            Some(p) => p,
            None => self.push_new(key),
        };
        let slot = self.slot(pos);
        if slot.id.is_empty()
            && let Some(id) = id
        {
            id.clone_into(&mut slot.id);
        }
        slot
    }

    fn push_new(&mut self, key: String) -> usize {
        self.calls.push(PartialCall::default());
        let pos = self.calls.len() - 1;
        self.keys.insert(key, pos);
        pos
    }

    fn slot(&mut self, pos: usize) -> &mut PartialCall {
        // `pos` always comes from `keys`/`push_new`, so it is in range.
        if pos >= self.calls.len() {
            self.calls.resize_with(pos + 1, PartialCall::default);
        }
        self.calls.get_mut(pos).unwrap_or_else(|| unreachable!())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Finalize: a call without a name is a protocol error; a missing id gets a synthetic one;
    /// empty arguments become `{}`. Syntactically invalid arguments are kept verbatim so the tool
    /// layer can report them back to the model.
    pub(crate) fn finish(self, provider: &str) -> Result<Vec<ToolCall>> {
        self.calls
            .into_iter()
            .map(|c| {
                if c.name.trim().is_empty() {
                    return Err(fatal(format!("{provider}: tool call without a name")));
                }
                Ok(ToolCall {
                    id: if c.id.is_empty() {
                        synthetic_call_id()
                    } else {
                        c.id
                    },
                    name: c.name,
                    arguments: if c.args.trim().is_empty() {
                        "{}".to_owned()
                    } else {
                        c.args
                    },
                    signature: c.signature,
                })
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use hrn_kernel::{BoxFuture, ChunkSink, LlmChunk};
    use std::sync::Mutex;

    /// Collects every chunk delivered through a [`ChunkSink`].
    #[derive(Default)]
    pub(crate) struct VecSink {
        chunks: Mutex<Vec<LlmChunk>>,
    }

    impl VecSink {
        pub(crate) fn content(&self) -> String {
            self.joined(|c| match c {
                LlmChunk::ContentDelta(t) => Some(t.clone()),
                LlmChunk::ReasoningDelta(_) => None,
            })
        }

        pub(crate) fn reasoning(&self) -> String {
            self.joined(|c| match c {
                LlmChunk::ReasoningDelta(t) => Some(t.clone()),
                LlmChunk::ContentDelta(_) => None,
            })
        }

        fn joined(&self, pick: impl Fn(&LlmChunk) -> Option<String>) -> String {
            let guard = self.chunks.lock().expect("sink lock");
            guard.iter().filter_map(pick).collect()
        }
    }

    impl ChunkSink for VecSink {
        fn on_chunk(&self, chunk: LlmChunk) -> BoxFuture<()> {
            self.chunks.lock().expect("sink lock").push(chunk);
            Box::pin(async {})
        }
    }

    /// Request heads seen by a [`serve`] loop, one per connection.
    pub(crate) type SeenHeads = std::sync::Arc<Mutex<Vec<String>>>;

    /// Serve `replies` loopback connections, one request each; `reply(i, head)` returns the raw
    /// HTTP response to request `i`. Returns the base URL and the recorded heads.
    pub(crate) async fn serve(
        replies: usize,
        reply: impl Fn(usize, &str) -> String + Send + 'static,
    ) -> (String, SeenHeads) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = SeenHeads::default();
        let log = seen.clone();
        tokio::spawn(async move {
            for i in 0..replies {
                let (mut sock, _) = listener.accept().await.expect("accept");
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let head = loop {
                    let n = sock.read(&mut chunk).await.expect("read");
                    assert!(n > 0, "connection closed early");
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len = header(head, "content-length")
                            .map_or(0, |v| v.parse::<usize>().expect("len"));
                        if body.len() >= len {
                            break head.to_owned();
                        }
                    }
                };
                let raw = reply(i, &head);
                log.lock().expect("log").push(head);
                sock.write_all(raw.as_bytes()).await.expect("write");
                sock.shutdown().await.ok();
            }
        });
        (format!("http://127.0.0.1:{port}"), seen)
    }

    /// Value of header `name` (case-insensitive) in a raw request head.
    pub(crate) fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then_some(value.trim())
        })
    }

    /// A complete `connection: close` HTTP response.
    pub(crate) fn http_reply(status: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finish(asm: ToolCallAssembler) -> Vec<ToolCall> {
        asm.finish("test").expect("finish")
    }

    #[test]
    fn interleaved_indexed_deltas_assemble_into_separate_calls() {
        let mut asm = ToolCallAssembler::default();
        asm.by_index(0, Some("call_a")).name.push_str("read");
        asm.by_index(1, Some("call_b")).name.push_str("grep");
        asm.by_index(0, None).args.push_str("{\"path\":");
        asm.by_index(1, None).args.push_str("{\"pattern\":\"x\"}");
        asm.by_index(0, None).args.push_str("\"a.rs\"}");
        let calls = finish(asm);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(calls[1].name, "grep");
        assert_eq!(calls[1].arguments, r#"{"pattern":"x"}"#);
    }

    #[test]
    fn gateway_reusing_index_zero_with_new_ids_yields_distinct_calls() {
        let mut asm = ToolCallAssembler::default();
        for (id, name, args) in [("c1", "read", "{\"a\":1}"), ("c2", "write", "{\"b\":2}")] {
            let slot = asm.by_index(0, Some(id));
            slot.id = id.to_owned();
            slot.name = name.to_owned();
            slot.args.push_str(args);
            // A follow-up delta repeating the same id must stay on the same call.
            asm.by_index(0, Some(id));
        }
        let calls = finish(asm);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, "{\"a\":1}");
        assert_eq!(calls[1].name, "write");
    }

    #[test]
    fn empty_arguments_become_empty_object_and_missing_id_is_synthesized() {
        let mut asm = ToolCallAssembler::default();
        asm.by_key("item_1").name = "ls".to_owned();
        let calls = finish(asm);
        assert_eq!(calls[0].arguments, "{}");
        assert!(calls[0].id.starts_with("call_hrn_"));
    }

    #[test]
    fn invalid_json_arguments_are_preserved_for_the_tool_layer() {
        let mut asm = ToolCallAssembler::default();
        let slot = asm.by_key("k");
        slot.id = "x".to_owned();
        slot.name = "edit".to_owned();
        slot.args = "{\"path\": \"a".to_owned();
        assert_eq!(finish(asm)[0].arguments, "{\"path\": \"a");
    }

    #[test]
    fn nameless_call_is_a_protocol_error() {
        let mut asm = ToolCallAssembler::default();
        asm.by_key("k").args.push_str("{}");
        assert!(asm.finish("test").is_err());
    }

    #[test]
    fn malformed_event_is_an_error_not_a_skip() {
        assert!(parse_event("p", "{\"a\":").is_err());
        assert!(parse_event("p", "{\"a\":1}").is_ok());
    }
}
