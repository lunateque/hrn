//! Wire protocol of a persistent shell: marker generation, the per-call script, and the pipe pump
//! that streams output up to the marker.

use crate::capture::{Capture, Utf8Chunker};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt};

/// One output pipe of the shell plus the bytes already read from it.
pub(crate) struct Pipe<R> {
    reader: R,
    /// Bytes read past the last marker (output of stray background processes).
    pending: Vec<u8>,
}

impl<R> Pipe<R> {
    pub(crate) const fn new(reader: R) -> Self {
        Self {
            reader,
            pending: Vec::new(),
        }
    }
}

/// Bounded capture plus live forwarding for one pipe of one call.
pub(crate) struct Sink<'a> {
    cap: Capture,
    chunker: Utf8Chunker,
    budget: &'a AtomicUsize,
    on_chunk: &'a (dyn Fn(String) + Sync),
}

impl<'a> Sink<'a> {
    pub(crate) fn new(
        limit: usize,
        budget: &'a AtomicUsize,
        on_chunk: &'a (dyn Fn(String) + Sync),
    ) -> Self {
        Self {
            cap: Capture::new(limit, limit),
            chunker: Utf8Chunker::default(),
            budget,
            on_chunk,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.cap.push(bytes);
        let granted = crate::take_budget(self.budget, bytes.len());
        let text = self.chunker.push(bytes);
        if granted > 0 && !text.is_empty() {
            (self.on_chunk)(text);
        }
    }

    pub(crate) fn finish(&mut self) {
        let tail = self.chunker.finish();
        if !tail.is_empty() && self.budget.load(Ordering::Relaxed) > 0 {
            (self.on_chunk)(tail);
        }
    }

    pub(crate) fn truncated(&self) -> bool {
        self.cap.truncated()
    }

    pub(crate) fn render(&self) -> String {
        self.cap.render()
    }
}

/// How the marker was found on a pipe.
pub(crate) enum Marked {
    /// The marker line arrived; carries the text between the marker and the end of its line.
    Line(String),
    /// The pipe closed first: the shell is gone.
    Eof,
}

/// Longest suffix of `pending` that is a proper prefix of `marker` (it may still complete).
fn partial_marker_len(pending: &[u8], marker: &[u8]) -> usize {
    (1..marker.len().min(pending.len() + 1))
        .rev()
        .find(|&k| marker.get(..k).is_some_and(|p| pending.ends_with(p)))
        .unwrap_or(0)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Spare capacity requested before each read.
const READ_CHUNK: usize = 8192;

/// Stream `pipe` into `sink` up to `marker`, then consume the rest of the marker line.
pub(crate) async fn pump_to_marker<R: AsyncRead + Unpin>(
    pipe: &mut Pipe<R>,
    marker: &[u8],
    sink: &mut Sink<'_>,
) -> io::Result<Marked> {
    loop {
        if let Some(at) = find_subslice(&pipe.pending, marker) {
            sink.push(pipe.pending.get(..at).unwrap_or_default());
            pipe.pending.drain(..at);
            let body = pipe.pending.get(marker.len()..).unwrap_or_default();
            if let Some(nl) = body.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(body.get(..nl).unwrap_or_default())
                    .trim_end_matches('\r')
                    .to_owned();
                pipe.pending.drain(..=marker.len() + nl);
                sink.finish();
                return Ok(Marked::Line(line));
            }
        } else {
            let safe = pipe.pending.len() - partial_marker_len(&pipe.pending, marker);
            sink.push(pipe.pending.get(..safe).unwrap_or_default());
            pipe.pending.drain(..safe);
        }
        pipe.pending.reserve(READ_CHUNK);
        let n = match pipe.reader.read_buf(&mut pipe.pending).await {
            Ok(n) => n,
            // A killed writer surfaces as a broken pipe on Windows.
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => 0,
            Err(err) => return Err(err),
        };
        if n == 0 {
            sink.push(&std::mem::take(&mut pipe.pending));
            sink.finish();
            return Ok(Marked::Eof);
        }
    }
}

pub(crate) fn single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

pub(crate) fn shell_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

/// Per-call marker: unguessable by the command it delimits (the nonce mixes the clock and pid).
pub(crate) fn make_marker(call: u64) -> String {
    static SALT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let salt = SALT.fetch_add(0x9E37_79B9, Ordering::Relaxed);
    format!("\u{1e}HRN{nanos:x}{:x}{salt:x}c{call}:", std::process::id())
}

pub(crate) const PWD: &str = if cfg!(windows) { "pwd -W" } else { "pwd" };

/// Sent once at startup, before the handshake reply. Commands run through `__hrn_run`, a function
/// whose `INT` trap makes it return: the host interrupts a timed-out or cancelled command with
/// `kill -INT` on the shell plus a kill of the running child, and the shell then abandons the rest
/// of the command instead of carrying on with the next statement. Outside a call `INT` is
/// ignored by a no-op trap so a late signal cannot take the shell down.
///
/// Limits of running in a function: `declare`/`typeset` without `-g` make variables local to the
/// call, and a function that the command itself defines and calls returns alone when interrupted,
/// so statements after that call still run.
pub(crate) const INIT: &str =
    "__hrn_run() { trap 'return 130' INT; builtin eval \"$__hrn_c\"; }; trap ':' INT";

/// The protocol script for one command. `tag` terminates the heredoc and must not occur in
/// `command`; both it and the marker are derived from the unguessable `marker`.
pub(crate) fn call_script(
    command: &str,
    cd: Option<&Path>,
    exports: &BTreeMap<String, String>,
    marker: &str,
) -> String {
    let tag = format!(
        "HRN_EOF_{}",
        marker.trim_matches(|c: char| !c.is_ascii_alphanumeric())
    );
    let mut script = String::with_capacity(command.len() + 384);
    // `read` is a builtin: no fork per call. stdin is cut off so a command that reads it sees
    // EOF instead of eating the protocol.
    let _ = write!(
        script,
        "IFS= builtin read -r -d '' __hrn_c <<'{tag}'\n{command}\n{tag}\n"
    );
    if let Some(dir) = cd {
        let _ = write!(script, "cd -- {} && ", single_quote(&shell_path(dir)));
    }
    let mut export = String::new();
    for (k, v) in exports {
        let _ = write!(export, " {k}={}", single_quote(v));
    }
    if !export.is_empty() {
        export = format!(
            "export{export}
"
        );
    }
    let q = single_quote(marker);
    let _ = writeln!(
        script,
        "{export}__hrn_run </dev/null; __hrn_rc=$?; trap ':' INT; printf '%s%d:' {q} \"$__hrn_rc\"; {PWD}; printf '%s\\n' {q} >&2"
    );
    script
}
