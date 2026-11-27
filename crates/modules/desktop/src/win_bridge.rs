//! Zero-`unsafe` persistent Windows PowerShell + inline C# bridge for DPI-aware capture,
//! UI Automation (`UIAutomationClient`), clipboard, displays, and background/takeover input.

use crate::error::DesktopError;
use hrn_edge_proc::{ChildWrapper, build_stdio_command};
use hrn_edge_rt::tokio;
use hrn_kernel::serde_json::{self, Value};
use hrn_kernel::{HrnError, Result};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Embedded PowerShell + C# host compiled once per bridge lifetime.
#[cfg(windows)]
const WINDOWS_BRIDGE_PS: &str = include_str!("win_bridge.ps1");

/// Persistent JSON-lines bridge process (killed automatically via Win32 Job Object on drop).
pub(crate) struct DesktopBridge {
    reader: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    _stderr_task: Option<tokio::task::JoinHandle<()>>,
    _child: Option<Box<dyn ChildWrapper>>,
}

impl DesktopBridge {
    /// Spawn the Windows PowerShell/C# bridge process inside a Win32 Job Object.
    #[cfg(windows)]
    pub(crate) fn spawn() -> Result<Self> {
        let script_path = materialize_bridge_script()?;
        let args = vec![
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-ExecutionPolicy".to_owned(),
            "Bypass".to_owned(),
            "-File".to_owned(),
            script_path.to_string_lossy().into_owned(),
        ];
        let mut cmd = build_stdio_command("powershell.exe", &args, None, &BTreeMap::new());
        let mut child = cmd
            .spawn()
            .map_err(|e| HrnError::Tool(format!("failed to start Windows desktop bridge: {e}")))?;
        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| HrnError::Tool("desktop bridge stdin unavailable".to_owned()))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| HrnError::Tool("desktop bridge stdout unavailable".to_owned()))?;
        let stderr_task = child.stderr().take().map(|mut err| {
            use tokio::io::AsyncReadExt as _;
            tokio::spawn(async move {
                let mut sink = [0u8; 4096];
                while let Ok(n) = err.read(&mut sink).await {
                    if n == 0 {
                        break;
                    }
                }
            })
        });
        Ok(Self {
            reader: BufReader::new(Box::new(stdout)),
            writer: Box::new(stdin),
            _stderr_task: stderr_task,
            _child: Some(child),
        })
    }

    /// On non-Windows hosts, report an explicit platform error rather than failing compilation.
    #[cfg(not(windows))]
    pub(crate) fn spawn() -> Result<Self> {
        let _ = (&build_stdio_command, &BTreeMap::<String, String>::new);
        Err(HrnError::Tool(
            "native desktop control is currently implemented for Windows hosts only".to_owned(),
        ))
    }

    /// Construct a bridge over arbitrary duplex streams for unit tests.
    #[cfg(test)]
    pub(crate) fn from_streams<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            reader: BufReader::new(Box::new(reader)),
            writer: Box::new(writer),
            _stderr_task: None,
            _child: None,
        }
    }

    /// Send one JSON command line and read one JSON response line, preserving typed bridge errors.
    pub(crate) async fn call(
        &mut self,
        request: &Value,
        timeout: Duration,
    ) -> std::result::Result<Value, DesktopError> {
        let mut line = serde_json::to_string(request)
            .map_err(|e| DesktopError::AxFailed(format!("desktop encode error: {e}")))?;
        line.push('\n');

        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| DesktopError::AxFailed(format!("desktop bridge write failed: {e}")))?;
        self.writer
            .flush()
            .await
            .map_err(|e| DesktopError::AxFailed(format!("desktop bridge flush failed: {e}")))?;

        let mut resp = String::new();
        let n = tokio::time::timeout(timeout, self.reader.read_line(&mut resp))
            .await
            .map_err(|_| {
                DesktopError::InputBusy(format!(
                    "desktop bridge timed out after {}s",
                    timeout.as_secs()
                ))
            })?
            .map_err(|e| DesktopError::AxFailed(format!("desktop bridge read failed: {e}")))?;
        if n == 0 {
            return Err(DesktopError::AxFailed(
                "desktop bridge exited unexpectedly".to_owned(),
            ));
        }
        let parsed: Value = serde_json::from_str(resp.trim())
            .map_err(|e| DesktopError::AxFailed(format!("desktop bridge invalid JSON: {e}")))?;
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(parse_bridge_error(&parsed));
        }
        Ok(parsed)
    }
}

fn parse_bridge_error(parsed: &Value) -> DesktopError {
    let message = parsed
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown desktop bridge error")
        .to_owned();
    if let Some(code) = parsed.get("code").and_then(Value::as_str)
        && let Some(typed) = DesktopError::from_code(code, message.clone())
    {
        return typed;
    }
    if message.contains("screen capture unavailable") {
        DesktopError::CaptureFailed(message)
    } else {
        DesktopError::AxFailed(message)
    }
}

#[cfg(windows)]
fn materialize_bridge_script() -> Result<std::path::PathBuf> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in WINDOWS_BRIDGE_PS.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let path = std::env::temp_dir().join(format!("hrn-desktop-bridge-{hash:016x}.ps1"));
    let needs_write = match std::fs::read_to_string(&path) {
        Ok(existing) => existing != WINDOWS_BRIDGE_PS,
        Err(_) => true,
    };
    if needs_write {
        std::fs::write(&path, WINDOWS_BRIDGE_PS).map_err(|e| {
            HrnError::Tool(format!(
                "failed to write Windows desktop bridge script `{}`: {e}",
                path.display()
            ))
        })?;
    }
    Ok(path)
}
