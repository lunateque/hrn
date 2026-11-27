//! NDJSON plumbing shared by both protocols: one writer task owning the output stream, a map of
//! requests awaiting the peer's reply, and the id of a JSON request.

use serde_json::Value;
use std::collections::HashMap;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Frames queued for the writer before senders wait.
const QUEUE: usize = 1024;

/// A frame did not fit the transport's frame limit.
#[derive(Debug)]
pub(crate) struct Oversize(pub(crate) usize);

/// The sending half of the single output stream; frames leave in the order they were queued.
#[derive(Clone)]
pub(crate) struct Out {
    tx: mpsc::Sender<String>,
    max_frame: Option<usize>,
}

/// Set `frame[key] = value` without panicking on a non-object frame.
pub(crate) fn put(frame: &mut Value, key: &str, value: Value) {
    if let Some(object) = frame.as_object_mut() {
        object.insert(key.to_owned(), value);
    }
}

impl Out {
    /// Start the writer task over `writer`; it ends once every [`Out`] is dropped, or on a write
    /// error, which it returns.
    pub(crate) fn spawn<W>(
        writer: W,
        max_frame: Option<usize>,
    ) -> (Self, JoinHandle<io::Result<()>>)
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(QUEUE);
        (Self { tx, max_frame }, tokio::spawn(write_loop(writer, rx)))
    }

    /// Queue `frame`; `Ok(false)` when the writer is gone.
    ///
    /// # Errors
    /// The serialized frame exceeds the frame limit.
    pub(crate) async fn send(&self, frame: &Value) -> Result<bool, Oversize> {
        let line = frame.to_string();
        if let Some(max) = self.max_frame
            && line.len() + 1 > max
        {
            return Err(Oversize(line.len() + 1));
        }
        Ok(self.tx.send(line).await.is_ok())
    }
}

async fn write_loop<W>(mut writer: W, mut rx: mpsc::Receiver<String>) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(mut line) = rx.recv().await {
        line.push('\n');
        writer.write_all(line.as_bytes()).await?;
        if rx.is_empty() {
            writer.flush().await?;
        }
    }
    writer.flush().await
}

/// Requests sent to the peer that have not been answered yet.
#[derive(Default)]
pub(crate) struct Pending {
    map: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    next: AtomicU64,
}

impl Pending {
    /// A fresh id (`prefix` + counter) and the receiver its answer will arrive on.
    pub(crate) fn register(&self, prefix: &str) -> (String, oneshot::Receiver<Value>) {
        let id = format!("{prefix}{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let (tx, rx) = oneshot::channel();
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), tx);
        (id, rx)
    }

    /// Hand `answer` to whoever waits on `id`; `false` when nobody does.
    pub(crate) fn resolve(&self, id: &str, answer: Value) -> bool {
        let sender = self
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        sender.is_some_and(|tx| tx.send(answer).is_ok())
    }

    /// Forget `id` (its waiter gave up).
    pub(crate) fn forget(&self, id: &str) {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    /// Drop every waiter: each sees its receiver close.
    pub(crate) fn fail_all(&self) {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// The key a JSON-RPC style id is stored under.
pub(crate) fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
