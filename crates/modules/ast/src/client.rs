//! The `hrn-ast` sidecar client: locate the binary, speak one JSON request per line, get one
//! JSON response back. A process per call keeps `hrn.exe` free of tree-sitter; startup cost of
//! the child is milliseconds, dwarfed by the parse it does.

use hrn_edge_ast::protocol::{Request, Response};
use hrn_edge_rt::tokio;
use hrn_edge_rt::tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use hrn_edge_rt::tokio::process::Command;
use hrn_edge_rt::tokio::time::timeout;
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long one sidecar round-trip may take before the call gives up.
const SIDECAR_TIMEOUT: Duration = Duration::from_mins(2);

/// Run `request` in a freshly spawned `hrn-ast` beside this executable.
///
/// # Errors
///
/// `HrnError::Tool` when the sidecar is missing or misbehaves, `HrnError::Cancelled` when
/// `cancel` fires while the child runs (the child dies with the future).
pub(crate) async fn run(request: &Request, cancel: &CancelFlag) -> Result<Response> {
    let exe = sidecar()?;
    let roundtrip = roundtrip(&exe, request);
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(HrnError::Cancelled),
        made = timeout(SIDECAR_TIMEOUT, roundtrip) => match made {
            Ok(done) => done,
            Err(_) => Err(HrnError::Tool(
                "ast: the hrn-ast sidecar timed out; narrow the scope".to_owned(),
            )),
        },
    }
}

async fn roundtrip(exe: &Path, request: &Request) -> Result<Response> {
    let mut child = Command::new(exe)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| HrnError::Tool(format!("ast: cannot start `{}`: {e}", exe.display())))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HrnError::Tool("ast: sidecar stdin is taken".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HrnError::Tool("ast: sidecar stdout is taken".to_owned()))?;
    let input = hrn_kernel::serde_json::to_string(request)
        .map_err(|e| HrnError::Internal(format!("ast request: {e}")))?;
    let writer = async move {
        let mut stdin = stdin;
        stdin
            .write_all(input.as_bytes())
            .await
            .map_err(|e| HrnError::Tool(format!("ast: sidecar stdin: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| HrnError::Tool(format!("ast: sidecar stdin: {e}")))?;
        Ok::<(), HrnError>(())
    };
    let reader = async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| HrnError::Tool(format!("ast: sidecar stdout: {e}")))?
        {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Response = hrn_kernel::serde_json::from_str(line.trim())
                .map_err(|e| HrnError::Tool(format!("ast: malformed sidecar response: {e}")))?;
            return Ok(parsed);
        }
        Err(HrnError::Tool(
            "ast: the hrn-ast sidecar answered nothing".to_owned(),
        ))
    };
    let (written, response) = tokio::join!(writer, reader);
    written?;
    let response = response?;
    let _status = child
        .wait()
        .await
        .map_err(|e| HrnError::Tool(format!("ast: sidecar wait: {e}")))?;
    Ok(response)
}

/// The sidecar next to the running `hrn` (`hrn-ast(.exe)` in the same directory).
/// `HRN_AST_SIDECAR` overrides the lookup (tests point it at a built sidecar).
///
/// # Errors
///
/// `HrnError::Tool` with a build hint when it is absent.
fn sidecar() -> Result<PathBuf> {
    if let Ok(overridden) = std::env::var("HRN_AST_SIDECAR") {
        let path = PathBuf::from(overridden);
        if path.is_file() {
            return Ok(path);
        }
    }
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let name = format!("hrn-ast{}", std::env::consts::EXE_SUFFIX);
    let path = dir.join(name);
    if path.is_file() {
        return Ok(path);
    }
    Err(HrnError::Tool(format!(
        "ast: the `{}` sidecar is missing next to hrn (looked in `{}`); build it with \
         `cargo build -p hrn-ast --release`",
        path.file_name().unwrap_or_default().to_string_lossy(),
        dir.display()
    )))
}
