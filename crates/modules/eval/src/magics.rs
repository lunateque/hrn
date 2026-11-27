//! Host-side standalone magics (`%pip <args>` and `%load <path-or-local://>`) for Python cells.
//!
//! `%pip` executes `python -m pip <args>` directly (no shell interpolation, no 30s cell watchdog)
//! while respecting `ctx.cancel`. `%load` reads either a workspace file or an internal URL
//! (`local://...`, `skill://...`, etc.) through `ctx.urls` and returns the source to execute inside
//! the persistent kernel.

use crate::lang::Lang;
use crate::spill::apply_spill;
use hrn_edge_proc::build_stdio_command;
use hrn_edge_rt::tokio;
use hrn_kernel::{AgentEvent, HrnError, Result, ToolContext, ToolOutput, UrlBody, UrlRouter};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// Maximum file size `%load` will read into a cell (4 MiB).
const MAX_LOAD_BYTES: usize = 4 * 1024 * 1024;
/// Maximum output bytes captured from `%pip` before capping/spilling.
const MAX_PIP_BYTES: usize = 2 * 1024 * 1024;
/// Cancellation poll cadence while `%pip` runs.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Standalone host-handled Python magic parsed from a cell.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HostMagic {
    /// `%pip <args...>`
    Pip { args: Vec<String> },
    /// `%load <path-or-url>`
    Load { target: String },
}

/// Detect standalone `%pip` or `%load` cells for Python.
pub(crate) fn parse_host_magic(lang: Lang, code: &str) -> Result<Option<HostMagic>> {
    if lang != Lang::Py {
        return Ok(None);
    }
    let lines: Vec<&str> = code
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let [only] = lines.as_slice() else {
        return Ok(None);
    };
    if let Some(rest) = strip_magic_head(only, "%pip") {
        let args = split_words(rest)?;
        if args.is_empty() {
            return Err(HrnError::Tool(
                "eval: `%pip` requires arguments (e.g. `%pip install <pkg>` or `%pip --version`)"
                    .to_owned(),
            ));
        }
        return Ok(Some(HostMagic::Pip { args }));
    }
    if let Some(rest) = strip_magic_head(only, "%load") {
        let target = unquote(rest.trim());
        if target.is_empty() {
            return Err(HrnError::Tool(
                "eval: `%load` requires a file path or `local://` URL".to_owned(),
            ));
        }
        return Ok(Some(HostMagic::Load {
            target: target.to_owned(),
        }));
    }
    Ok(None)
}

fn strip_magic_head<'a>(line: &'a str, magic: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(magic)?;
    if rest.is_empty() {
        Some("")
    } else if rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

fn unquote(raw: &str) -> &str {
    if raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')))
    {
        raw.get(1..raw.len() - 1).unwrap_or(raw)
    } else {
        raw
    }
}

/// POSIX-style quote-aware word splitter (no shell execution).
pub(crate) fn split_words(input: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') if ch == '\'' => quote = None,
            Some('"') if ch == '"' => quote = None,
            Some('"') if ch == '\\' => {
                if let Some(&next) = chars.peek()
                    && matches!(next, '"' | '\\')
                {
                    let _ = chars.next();
                    cur.push(next);
                } else {
                    cur.push('\\');
                }
            }
            Some(_) => cur.push(ch),
            None if ch.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                in_word = true;
            }
            None => {
                cur.push(ch);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err(HrnError::Tool(
            "eval: unterminated quote in `%pip` arguments".to_owned(),
        ));
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

/// Resolve and read a `%load` target (either an internal URL such as `local://...` or a workspace path).
pub(crate) async fn load_source(ctx: &ToolContext, target: &str) -> Result<String> {
    if let Some((scheme, rest)) = UrlRouter::split(target) {
        let body = ctx.urls.read(ctx, scheme, rest)?;
        return match body {
            UrlBody::Text(text) => {
                if text.len() > MAX_LOAD_BYTES {
                    return Err(HrnError::Tool(format!(
                        "eval: `%load {target}` exceeds {MAX_LOAD_BYTES} byte limit"
                    )));
                }
                Ok(text)
            }
            UrlBody::Path(path) => read_bounded_file(&path, target).await,
        };
    }
    let path = resolve_path(&ctx.cwd, target);
    read_bounded_file(&path, target).await
}

fn resolve_path(cwd: &Path, target: &str) -> PathBuf {
    let p = Path::new(target);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

pub(crate) async fn read_bounded_file(path: &Path, display_target: &str) -> Result<String> {
    let meta = tokio::fs::metadata(path).await.map_err(|e| {
        HrnError::Tool(format!(
            "eval: `%load {display_target}` cannot stat `{}`: {e}",
            path.display()
        ))
    })?;
    if !meta.is_file() {
        return Err(HrnError::Tool(format!(
            "eval: `%load {display_target}` target `{}` is not a regular file",
            path.display()
        )));
    }
    if meta.len() > MAX_LOAD_BYTES as u64 {
        return Err(HrnError::Tool(format!(
            "eval: `%load {display_target}` exceeds {MAX_LOAD_BYTES} byte limit"
        )));
    }
    let file = tokio::fs::File::open(path).await.map_err(|e| {
        HrnError::Tool(format!(
            "eval: `%load {display_target}` cannot open `{}`: {e}",
            path.display()
        ))
    })?;
    let opened_meta = file.metadata().await.map_err(|e| {
        HrnError::Tool(format!(
            "eval: `%load {display_target}` cannot inspect `{}`: {e}",
            path.display()
        ))
    })?;
    if !opened_meta.is_file() {
        return Err(HrnError::Tool(format!(
            "eval: `%load {display_target}` target `{}` is not a regular file",
            path.display()
        )));
    }
    let mut buf = Vec::new();
    file.take((MAX_LOAD_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| {
            HrnError::Tool(format!(
                "eval: `%load {display_target}` cannot read `{}`: {e}",
                path.display()
            ))
        })?;
    if buf.len() > MAX_LOAD_BYTES {
        return Err(HrnError::Tool(format!(
            "eval: `%load {display_target}` exceeds {MAX_LOAD_BYTES} byte limit"
        )));
    }
    String::from_utf8(buf).map_err(|e| {
        HrnError::Tool(format!(
            "eval: `%load {display_target}` is not valid UTF-8: {e}"
        ))
    })
}

/// Execute standalone `%pip <args>` directly via `<python> -m pip <args>` with no cell watchdog.
pub(crate) async fn run_pip(ctx: &ToolContext, args: &[String]) -> Result<ToolOutput> {
    let python = Lang::python_executable()?;
    let mut cmd_args = Vec::with_capacity(args.len() + 2);
    cmd_args.push("-m".to_owned());
    cmd_args.push("pip".to_owned());
    cmd_args.extend(args.iter().cloned());
    let env: BTreeMap<String, String> = [
        ("PYTHONUTF8".to_owned(), "1".to_owned()),
        ("PIP_DISABLE_PIP_VERSION_CHECK".to_owned(), "1".to_owned()),
    ]
    .into_iter()
    .collect();
    let mut child = build_stdio_command(&python, &cmd_args, Some(&ctx.cwd), &env)
        .spawn()
        .map_err(|e| HrnError::Tool(format!("eval: `%pip` failed to start `{python}`: {e}")))?;
    let stdout = child
        .stdout()
        .take()
        .ok_or_else(|| HrnError::Tool("eval: `%pip` stdout unavailable".to_owned()))?;
    let stderr = child
        .stderr()
        .take()
        .ok_or_else(|| HrnError::Tool("eval: `%pip` stderr unavailable".to_owned()))?;
    let (tx, mut rx) = unbounded_channel::<String>();
    tokio::spawn(pump_lines(stdout, tx.clone()));
    tokio::spawn(pump_lines(stderr, tx));

    let mut out = String::new();
    let mut dropped = 0usize;
    let mut ticker = tokio::time::interval(CANCEL_POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            line = rx.recv() => {
                let Some(line) = line else { break };
                push_capped(&mut out, &mut dropped, &line);
                let _ = ctx.events.emit(AgentEvent::ToolDelta {
                    call_id: ctx.call_id.clone(),
                    delta: line,
                }).await;
            }
            _ = ticker.tick() => {
                if ctx.cancel.is_cancelled() {
                    let _ = child.start_kill();
                    return Err(HrnError::Cancelled);
                }
            }
        }
    }
    let status = child
        .wait()
        .await
        .map_err(|e| HrnError::Io(format!("eval: `%pip` wait failed: {e}")))?;
    if dropped > 0 {
        let _ = write!(out, "\n[{dropped} more output bytes dropped]");
    }
    let failed = !status.success();
    let body = if out.trim().is_empty() {
        if failed {
            format!("pip exited with status {status}")
        } else {
            "(pip completed with no output)".to_owned()
        }
    } else {
        out.trim_end().to_owned()
    };
    Ok(apply_spill(ctx, &body, Vec::new(), None, failed))
}

fn push_capped(buf: &mut String, dropped: &mut usize, piece: &str) {
    let room = MAX_PIP_BYTES.saturating_sub(buf.len());
    let keep = piece.floor_char_boundary(room.min(piece.len()));
    buf.push_str(piece.get(..keep).unwrap_or_default());
    *dropped = dropped.saturating_add(piece.len() - keep);
}

async fn pump_lines<R: AsyncRead + Unpin>(reader: R, tx: UnboundedSender<String>) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let line = match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => String::from_utf8_lossy(&buf).into_owned(),
        };
        if tx.send(line).is_err() {
            return;
        }
    }
}
