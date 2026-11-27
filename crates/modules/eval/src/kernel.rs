//! One long-lived interpreter child speaking the JSON-lines protocol of `runner/kernel.*`,
//! paired with its per-kernel authenticated loopback HTTP bridge and pausable timeout watchdog.

use crate::bridge::{BridgeInit, KernelBridge};
use crate::lang::Lang;
use hrn_edge_proc::{ChildWrapper, build_stdio_command, resolve_shell};
use hrn_edge_rt::tokio;
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{AgentEvent, HrnError, ImageAttachment, Result, ToolContext};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Prefix of the reply line; everything before it on the same line is ordinary cell output.
const MARK: &str = "\x01HRN\x01";
/// Cap on the raw output captured per call (spill keeps inline <= 50 KiB and saves full to local://).
const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
/// How often a running call checks for cancellation.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// How a call ended.
pub(crate) enum CallEnd {
    /// The cell finished; `value` is the echoed trailing expression, `error` a traceback.
    Reply {
        value: Option<String>,
        error: Option<String>,
    },
    /// The interpreter exited (or its pipes closed) mid-call.
    Died,
    /// The per-call timeout elapsed.
    TimedOut,
    /// The session cancelled the call.
    Cancelled,
}

/// Everything a call produced: streamed output, attached images, and how it ended.
pub(crate) struct CallResult {
    pub(crate) output: String,
    pub(crate) images: Vec<ImageAttachment>,
    pub(crate) end: CallEnd,
}

/// Environment hints injected into a newly spawned interpreter.
pub(crate) struct SpawnOpts {
    pub(crate) computer_enabled: bool,
    pub(crate) max_concurrency: usize,
}

/// A running interpreter. Dropping it kills the whole process tree and stops its HTTP bridge.
pub(crate) struct Kernel {
    child: Box<dyn ChildWrapper>,
    stdin: ChildStdin,
    lines: UnboundedReceiver<String>,
    next_id: u64,
    bridge: KernelBridge,
}

impl Kernel {
    /// Start the per-kernel HTTP bridge and spawn the interpreter for `lang` running `script`.
    pub(crate) async fn spawn(
        lang: Lang,
        script: &Path,
        cwd: &Path,
        bridge_init: BridgeInit,
        opts: SpawnOpts,
    ) -> Result<Self> {
        let bridge = KernelBridge::start(bridge_init).await?;
        let (program, args) = lang.command(script)?;
        let mut env: BTreeMap<String, String> = [
            ("CHERE_INVOKING", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_PAGER", "cat"),
            ("PAGER", "cat"),
            ("PYTHONUTF8", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        env.insert("HRN_EVAL_SHELL".to_owned(), shell_descriptor().to_string());
        env.insert("HRN_EVAL_BRIDGE".to_owned(), bridge.url());
        env.insert("HRN_EVAL_TOKEN".to_owned(), bridge.token().to_owned());
        env.insert(
            "HRN_EVAL_COMPUTER".to_owned(),
            if opts.computer_enabled { "1" } else { "0" }.to_owned(),
        );
        env.insert(
            "HRN_EVAL_MAX_CONCURRENCY".to_owned(),
            opts.max_concurrency.to_string(),
        );
        let mut child = build_stdio_command(&program, &args, Some(cwd), &env)
            .spawn()
            .map_err(|e| HrnError::Tool(format!("eval: failed to start `{program}`: {e}")))?;
        let pipe_missing = |name: &str| HrnError::Tool(format!("eval: kernel {name} unavailable"));
        let stdin = child.stdin().take().ok_or_else(|| pipe_missing("stdin"))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| pipe_missing("stdout"))?;
        let stderr = child
            .stderr()
            .take()
            .ok_or_else(|| pipe_missing("stderr"))?;
        let (tx, lines) = unbounded_channel();
        tokio::spawn(pump(stdout, tx.clone()));
        tokio::spawn(pump(stderr, tx));
        Ok(Self {
            child,
            stdin,
            lines,
            next_id: 0,
            bridge,
        })
    }

    /// Whether both output pipes are closed, i.e. the interpreter is gone.
    pub(crate) fn is_dead(&self) -> bool {
        self.lines.is_closed()
    }

    /// Per-kernel bearer token (for redaction only; never exposed to models).
    pub(crate) fn bridge_token(&self) -> &str {
        self.bridge.token()
    }

    /// Loopback URL of this kernel's HTTP bridge (for security tests).
    #[cfg(test)]
    pub(crate) fn bridge_url(&self) -> String {
        self.bridge.url()
    }

    /// Kill the interpreter and its descendants.
    pub(crate) fn kill(mut self) -> Result<()> {
        match self.child.start_kill() {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(HrnError::Io(format!("eval: failed to kill kernel: {e}"))),
        }
    }

    /// Run one code cell with pausable timeout watchdog and live `ToolDelta` streaming.
    pub(crate) async fn call(
        &mut self,
        ctx: &ToolContext,
        code: &str,
        timeout: Option<Duration>,
    ) -> CallResult {
        self.next_id += 1;
        let request = json!({
            "id": self.next_id,
            "op": "eval",
            "code": code,
            "cwd": ctx.cwd,
        });
        self.send_and_wait(ctx, &request, timeout).await
    }

    /// Invoke a kernel-defined `@tool` function registered in this interpreter.
    pub(crate) async fn call_defined_tool(
        &mut self,
        ctx: &ToolContext,
        name: &str,
        args: &Value,
        timeout: Option<Duration>,
    ) -> CallResult {
        self.next_id += 1;
        let request = json!({
            "id": self.next_id,
            "op": "call_tool",
            "name": name,
            "args": args,
            "cwd": ctx.cwd,
        });
        self.send_and_wait(ctx, &request, timeout).await
    }

    async fn send_and_wait(
        &mut self,
        ctx: &ToolContext,
        request: &Value,
        timeout: Option<Duration>,
    ) -> CallResult {
        let mut out = OutputBuf::default();
        while let Ok(line) = self.lines.try_recv() {
            out.push(&line);
        }
        let call_guard = self.bridge.begin_call(ctx.clone());
        let sent = async {
            self.stdin
                .write_all(format!("{request}\n").as_bytes())
                .await?;
            self.stdin.flush().await
        }
        .await;
        if sent.is_err() {
            let images = call_guard.finish();
            return out.finish(CallEnd::Died, images);
        }

        let watchdog = Arc::clone(self.bridge.watchdog());
        let (end, mut reply_images) = self.wait_for_reply(ctx, &mut out, timeout, &watchdog).await;
        let mut images = call_guard.finish();
        images.append(&mut reply_images);
        out.finish(end, images)
    }

    async fn wait_for_reply(
        &mut self,
        ctx: &ToolContext,
        out: &mut OutputBuf,
        timeout: Option<Duration>,
        watchdog: &crate::bridge::WatchdogState,
    ) -> (CallEnd, Vec<ImageAttachment>) {
        enum Step {
            Line(Option<String>),
            Expired,
            WatchdogChanged,
            CancelTick,
        }
        let mut remaining = timeout;
        let mut ticker = tokio::time::interval(CANCEL_POLL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let paused = watchdog.is_paused();
            let started = (!paused).then(Instant::now);
            let budget = remaining;
            let timer = async move {
                match (paused, budget) {
                    (false, Some(dur)) => tokio::time::sleep(dur).await,
                    _ => std::future::pending().await,
                }
            };
            tokio::pin!(timer);
            let notified = watchdog.notified();
            tokio::pin!(notified);

            let step = tokio::select! {
                line = self.lines.recv() => Step::Line(line),
                () = &mut timer => Step::Expired,
                () = &mut notified => Step::WatchdogChanged,
                _ = ticker.tick() => Step::CancelTick,
            };
            if let (Some(t), Some(rem)) = (started, remaining.as_mut()) {
                *rem = rem.saturating_sub(t.elapsed());
            }
            match step {
                Step::Line(None) => return (CallEnd::Died, Vec::new()),
                Step::Line(Some(line)) => {
                    let tok = Some(self.bridge.token());
                    if let Some((before, reply)) = line.split_once(MARK) {
                        out.push(before);
                        if !before.is_empty() {
                            emit_delta(ctx, crate::spill::redact_token(before, tok)).await;
                        }
                        return parse_reply(reply);
                    }
                    out.push(&line);
                    emit_delta(ctx, crate::spill::redact_token(&line, tok)).await;
                }
                Step::Expired => {
                    if !watchdog.is_paused() {
                        return (CallEnd::TimedOut, Vec::new());
                    }
                }
                Step::WatchdogChanged => {}
                Step::CancelTick => {
                    if ctx.cancel.is_cancelled() {
                        return (CallEnd::Cancelled, Vec::new());
                    }
                }
            }
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn emit_delta(ctx: &ToolContext, delta: String) {
    let event = AgentEvent::ToolDelta {
        call_id: ctx.call_id.clone(),
        delta,
    };
    let _ = ctx.events.emit(event).await;
}

fn parse_reply(text: &str) -> (CallEnd, Vec<ImageAttachment>) {
    let Ok(reply) = serde_json::from_str::<Value>(text.trim()) else {
        return (
            CallEnd::Reply {
                value: None,
                error: Some(format!("eval: malformed kernel reply: {}", text.trim())),
            },
            Vec::new(),
        );
    };
    let field = |key: &str| reply.get(key).and_then(Value::as_str).map(str::to_owned);
    let images = parse_reply_images(&reply);
    (
        CallEnd::Reply {
            value: field("value"),
            error: field("error"),
        },
        images,
    )
}

fn parse_reply_images(reply: &Value) -> Vec<ImageAttachment> {
    let Some(arr) = reply.get("images").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let media_type = item.get("media_type").and_then(Value::as_str)?;
            let data_base64 = item.get("data_base64").and_then(Value::as_str)?;
            if media_type.is_empty() || data_base64.is_empty() {
                return None;
            }
            Some(ImageAttachment {
                media_type: media_type.to_owned(),
                data_base64: data_base64.to_owned(),
                label: item.get("label").and_then(Value::as_str).map(str::to_owned),
                width: item
                    .get("width")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok()),
                height: item
                    .get("height")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok()),
            })
        })
        .collect()
}

/// The POSIX shell the kernel's `sh()` helper uses, or why there is none.
fn shell_descriptor() -> Value {
    match resolve_shell() {
        Ok(shell) => {
            let mut argv = vec![shell.program];
            argv.extend(shell.args);
            argv.push("-c".to_owned());
            json!({ "argv": argv })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Forward lines of `reader` to `tx` until EOF.
async fn pump<R: AsyncRead + Unpin>(reader: R, tx: UnboundedSender<String>) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let line = match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => return,
            Ok(_) => String::from_utf8_lossy(&buf).into_owned(),
            Err(e) => {
                let _ = tx.send(format!("[kernel pipe read error: {e}]\n"));
                return;
            }
        };
        if tx.send(line).is_err() {
            return;
        }
    }
}

/// Output accumulated during one call, bounded by [`MAX_OUTPUT_BYTES`].
#[derive(Default)]
struct OutputBuf {
    text: String,
    dropped: usize,
}

impl OutputBuf {
    fn push(&mut self, piece: &str) {
        let room = MAX_OUTPUT_BYTES.saturating_sub(self.text.len());
        let keep = piece.floor_char_boundary(room.min(piece.len()));
        self.text.push_str(piece.get(..keep).unwrap_or_default());
        self.dropped += piece.len() - keep;
    }

    fn finish(mut self, end: CallEnd, images: Vec<ImageAttachment>) -> CallResult {
        if self.dropped > 0 {
            let note = format!("\n[{} more output bytes dropped]", self.dropped);
            self.text.push_str(&note);
        }
        CallResult {
            output: self.text,
            images,
            end,
        }
    }
}
