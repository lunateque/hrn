//! Adapters bridging pure `hrn-harness` URL scheme ports (`out://`, `ssh://`, `attachment://`)
//! to real edge crates (`hrn-edge-ipc`, `hrn-edge-tools`, tokio runtime).

use hrn_edge_ipc::remote::{self, HostTrustStore, MAX_SSH_READ_BYTES, parse_host_spec};
use hrn_edge_tools::DiskAttachmentStore;
use hrn_harness::{AttachmentEntry, AttachmentStore, EventEmitter, SshFileContent, SshTransport};
use hrn_kernel::{AgentEvent, EventSink, HrnError, ImageAttachment, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use tokio::runtime::{Handle, RuntimeFlavor};

/// Timeout for one `ssh://` URL read or directory listing.
const SSH_URL_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum number of attached images materialized in a single turn.
const MAX_ATTACHMENTS_PER_TURN: usize = 16;

/// Run an async operation from a synchronous [`hrn_kernel::UrlScheme`] callback without panicking
/// inside a tokio worker or deadlocking a single-threaded test runtime.
pub(crate) fn block_on_safe<F, T>(handle: Option<&Handle>, timeout: Duration, fut: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send,
    T: Send,
{
    let timed = async move {
        tokio::time::timeout(timeout, fut).await.map_err(|_| {
            HrnError::Tool(format!("request timed out after {}s", timeout.as_secs()))
        })?
    };
    let active = handle.cloned().or_else(|| Handle::try_current().ok());
    if let Some(h) = active {
        return std::thread::scope(|scope| {
            scope.spawn(|| h.block_on(timed)).join().map_err(|_| {
                HrnError::Internal("async URL bridge worker thread panicked".to_owned())
            })?
        });
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| HrnError::Internal(format!("cannot create helper runtime: {e}")))?;
    rt.block_on(timed)
}

/// Event emitter for [`hrn_harness::OutScheme`] that delivers `AgentEvent::AssistantNote` cleanly
/// for both synchronous (`TuiSink`, `ConsoleSink`) and async (`rpc`, `acp`) event sinks.
pub(crate) fn out_event_emitter() -> EventEmitter {
    Arc::new(|sink: &Arc<dyn EventSink>, event: AgentEvent| {
        let mut fut = sink.emit(event);
        let mut cx = Context::from_waker(Waker::noop());
        if matches!(fut.as_mut().poll(&mut cx), Poll::Ready(_)) {
            return Ok(());
        }
        match Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
                std::thread::scope(|scope| {
                    scope.spawn(|| handle.block_on(fut)).join().map_err(|_| {
                        HrnError::Internal("out:// event delivery thread panicked".to_owned())
                    })
                })?;
                Ok(())
            }
            Ok(handle) => {
                drop(handle.spawn(fut));
                Ok(())
            }
            Err(_) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| {
                        HrnError::Internal(format!("cannot create helper runtime: {e}"))
                    })?;
                let _ = rt.block_on(fut);
                Ok(())
            }
        }
    })
}

/// Concrete [`SshTransport`] backed by `hrn_edge_ipc::remote` and pinned `remote-hosts.json`.
pub(crate) struct RemoteSshTransport {
    hosts_path: PathBuf,
    handle: Option<Handle>,
}

impl RemoteSshTransport {
    pub(crate) fn new(hosts_path: PathBuf, handle: Option<Handle>) -> Self {
        Self { hosts_path, handle }
    }
}

impl SshTransport for RemoteSshTransport {
    fn list_hosts(&self) -> Result<Vec<String>> {
        let store = HostTrustStore::load(&self.hosts_path)?;
        Ok(remote::list_hosts(&store))
    }

    fn read_file(&self, host: &str, path: &str, max_bytes: usize) -> Result<SshFileContent> {
        let store = HostTrustStore::load(&self.hosts_path)?;
        let target = parse_host_spec(host)?;
        let cap = max_bytes.min(MAX_SSH_READ_BYTES);
        let path = path.to_owned();
        let read = block_on_safe(self.handle.as_ref(), SSH_URL_TIMEOUT, async move {
            remote::read_file(&target, &store, &path, cap).await
        })?;
        Ok(SshFileContent {
            content: read.content,
            truncated: read.truncated,
            total_bytes: None,
        })
    }

    fn list_dir(&self, host: &str, path: &str) -> Result<Vec<String>> {
        let store = HostTrustStore::load(&self.hosts_path)?;
        let target = parse_host_spec(host)?;
        let path = path.to_owned();
        block_on_safe(self.handle.as_ref(), SSH_URL_TIMEOUT, async move {
            remote::list_dir(&target, &store, &path).await
        })
    }
}

/// Adapter exposing [`DiskAttachmentStore`] through `hrn-harness`'s [`AttachmentStore`] port.
pub(crate) struct DiskAttachmentAdapter {
    store: Arc<DiskAttachmentStore>,
}

impl DiskAttachmentAdapter {
    pub(crate) fn new(store: Arc<DiskAttachmentStore>) -> Self {
        Self { store }
    }
}

impl AttachmentStore for DiskAttachmentAdapter {
    fn list(&self, root_session_id: &str) -> Result<Vec<AttachmentEntry>> {
        let rows = self.store.list(root_session_id)?;
        Ok(rows
            .into_iter()
            .map(|(name, size_bytes)| AttachmentEntry { name, size_bytes })
            .collect())
    }

    fn resolve(&self, root_session_id: &str, name: &str) -> Result<PathBuf> {
        self.store.resolve(root_session_id, name)
    }
}

/// Materialize user-submitted `images` into the session's attachment directory and append
/// `attachment://<n>-<name>` references to `prompt` so the model can re-read them via `read`.
pub(crate) fn materialize_prompt_images(
    store: &DiskAttachmentStore,
    root_session_id: &str,
    prompt: &str,
    images: &[ImageAttachment],
) -> String {
    if images.is_empty() {
        return prompt.to_owned();
    }
    let mut refs = Vec::new();
    for image in images.iter().take(MAX_ATTACHMENTS_PER_TURN) {
        if let Ok(key) = store.store_image(root_session_id, image) {
            refs.push(format!("attachment://{key}"));
        }
    }
    if refs.is_empty() {
        return prompt.to_owned();
    }
    if prompt.trim().is_empty() {
        format!("[Attachments: {}]", refs.join(", "))
    } else {
        format!("{prompt}\n\n[Attachments: {}]", refs.join(", "))
    }
}

/// Convenience helper for RPC/TUI turn entrypoints: materialize images into the session's root
/// attachment folder if `images` is non-empty.
pub(crate) fn materialize_turn_images(
    background: &crate::wiring::Background,
    runtime: &hrn_harness::AgentRuntime,
    session_id: &str,
    prompt: &str,
    images: &[ImageAttachment],
) -> String {
    if images.is_empty() {
        return prompt.to_owned();
    }
    let Ok(data_dir) = crate::app_data_dir() else {
        return prompt.to_owned();
    };
    let Ok(store) = background.attachments(&data_dir) else {
        return prompt.to_owned();
    };
    let root = runtime.agents().root_of(session_id);
    materialize_prompt_images(&store, &root, prompt, images)
}
