//! The one request/response exchange behind `hrn tool`: a command of the embedded shell answers a
//! tool call of the running session. One JSON line in, one JSON header line plus a raw body out,
//! on a loopback socket guarded by a bearer token — a lighter contract than [`crate::server`]'s
//! session API, because the caller is a short-lived child process of the agent's own shell.

use crate::auth::ct_eq;
use hrn_kernel::{BoxFuture, HrnError, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// The longest accepted request line.
const MAX_REQUEST: usize = 8 * 1024 * 1024;
/// The longest accepted answer header.
const MAX_HEADER: usize = 64 * 1024;
/// How long a client may take to send its request line.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the shim waits for the tool call to be answered (a build can run long).
const ANSWER_TIMEOUT: Duration = Duration::from_mins(30);

/// What the bridge answers.
#[derive(Debug)]
pub struct Reply {
    /// The exit code the `hrn tool` process should have.
    pub code: u8,
    /// Bytes for the command's stdout.
    pub stdout: Vec<u8>,
    /// Bytes for the command's stderr (usage and tool errors land here).
    pub stderr: Vec<u8>,
}

impl Reply {
    /// A failed call: `message` on stderr, exit `code`.
    #[must_use]
    pub fn err(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            stdout: Vec::new(),
            stderr: message.into().into_bytes(),
        }
    }
}

/// Answers one tool-call request.
pub trait ToolBridge: Send + Sync + 'static {
    /// `request` is the JSON line the shim sent; `cancel` is raised when the shim goes away.
    fn handle(&self, request: String, cancel: hrn_kernel::CancelFlag) -> BoxFuture<Result<Reply>>;
}

/// What the accept thread hands to each connection.
struct Served {
    token: String,
    bridge: Arc<dyn ToolBridge>,
    rt: tokio::runtime::Handle,
}

/// Serve [`ToolBridge`] requests for one process. The listener lives until the process ends.
pub struct BridgeServer {
    addr: SocketAddr,
}

impl BridgeServer {
    /// Bind a loopback listener and answer requests on it, running every handler on `rt` (the
    /// session's runtime).
    ///
    /// # Errors
    ///
    /// When the listener cannot be bound, `addr` is not loopback, or the thread cannot start.
    pub fn start(addr: SocketAddr, token: String, bridge: Arc<dyn ToolBridge>) -> Result<Self> {
        let rt = tokio::runtime::Handle::current();
        if !addr.ip().is_loopback() {
            return Err(HrnError::Auth(format!(
                "tool bridge refuses to bind non-loopback `{addr}`"
            )));
        }
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?;
        std::thread::Builder::new()
            .name("hrn-tool-bridge".to_owned())
            .spawn(move || {
                let served = Arc::new(Served { token, bridge, rt });
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            let served = Arc::clone(&served);
                            let rt = served.rt.clone();
                            rt.spawn_blocking(move || exchange(&stream, &served));
                        }
                        // A listener that stopped accepting has nothing left to serve.
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self { addr: local })
    }

    /// The address to dial.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

fn bad_request(what: &str) -> Reply {
    Reply::err(2, format!("hrn tool: {what}"))
}

/// One request line, one answer. A client that goes away cancels the call.
fn exchange(stream: &TcpStream, served: &Served) {
    let mut reader = match stream.try_clone() {
        Ok(clone) => BufReader::new(clone),
        Err(_) => return,
    };
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut line = String::new();
    match reader.read_line(&mut line) {
        // Closed without a request, or a socket error on a client that hung up: nothing to answer.
        Ok(0) | Err(_) => return,
        Ok(n) if n > MAX_REQUEST => {
            return write_reply(stream, &bad_request("request too large for a tool command"));
        }
        Ok(_) => {}
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line.trim_end()) else {
        return write_reply(stream, &bad_request("expected one JSON object"));
    };
    let given = parsed.get("token").and_then(serde_json::Value::as_str);
    if !given.is_some_and(|given| ct_eq(given.as_bytes(), served.token.as_bytes())) {
        return write_reply(stream, &bad_request("missing or invalid token"));
    }
    // The shim going away (Ctrl-C in the shell, a killed pipeline) cancels the tool call.
    let cancel = hrn_kernel::CancelFlag::new();
    let watching = cancel.clone();
    match stream.try_clone() {
        Ok(mut waiter) => {
            std::thread::spawn(move || {
                let mut sink = Vec::new();
                // EOF or a dead socket: nobody is waiting for the answer any more.
                let _ = BufReader::new(&mut waiter).read_to_end(&mut sink);
                watching.cancel();
            });
        }
        Err(_) => return write_reply(stream, &bad_request("connection clone failed")),
    }
    let reply = match served
        .rt
        .block_on(served.bridge.handle(parsed.to_string(), cancel))
    {
        Ok(reply) => reply,
        Err(err) => Reply::err(1, format!("hrn tool: {err}")),
    };
    write_reply(stream, &reply);
}

fn write_reply(mut stream: &TcpStream, reply: &Reply) {
    let header = serde_json::to_vec(&serde_json::json!({
        "code": reply.code, "out": reply.stdout.len(), "err": reply.stderr.len(),
    }))
    .unwrap_or_else(|_| br#"{"code":1,"out":0,"err":13}"#.to_vec());
    let _ = stream
        .write_all(&header)
        .and_then(|()| stream.write_all(b"\n"))
        .and_then(|()| stream.write_all(&reply.stdout))
        .and_then(|()| stream.write_all(&reply.stderr))
        .and_then(|()| stream.flush());
}

/// The shim's side: one request line, then the answer header and its two bodies. Blocking std IO,
/// so `hrn tool` needs no runtime of its own.
///
/// # Errors
///
/// When the bridge cannot be reached or answers something that is not the promised shape.
pub fn call_bridge(addr: SocketAddr, request: &str) -> Result<Reply> {
    let mut stream = TcpStream::connect(addr).map_err(|e| {
        HrnError::Io(format!(
            "connect tool bridge at `{addr}` (the session ended?): {e}"
        ))
    })?;
    stream.set_read_timeout(Some(ANSWER_TIMEOUT))?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut buffered = BufReader::new(&stream);
    let mut header = String::new();
    let io_err = |e: std::io::Error| HrnError::Io(e.to_string());
    if buffered.read_line(&mut header).map_err(io_err)? == 0 {
        return Err(HrnError::Io(
            "the session closed the tool bridge connection".to_owned(),
        ));
    }
    if header.len() > MAX_HEADER {
        return Err(HrnError::Io(
            "tool bridge answer header is too large".to_owned(),
        ));
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(header.trim_end()) else {
        return Err(HrnError::Io(format!(
            "tool bridge answered nonsense: {}",
            header.trim_end()
        )));
    };
    let number = |key: &str| -> usize {
        parsed
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0)
    };
    let take = |buffered: &mut BufReader<&TcpStream>, n: usize| -> Result<Vec<u8>> {
        let mut bytes = vec![0u8; n];
        buffered
            .read_exact(&mut bytes)
            .map(|()| bytes)
            .map_err(io_err)
    };
    let code = u8::try_from(number("code")).unwrap_or(1);
    let (stdout, stderr) = (
        take(&mut buffered, number("out"))?,
        take(&mut buffered, number("err"))?,
    );
    Ok(Reply {
        code,
        stdout,
        stderr,
    })
}

/// The `hrn tool <name> [args]` command line: turn the shell's arguments into one bridge request
/// and mirror the answer onto this process's stdout/stderr/exit code. `@FILE` and `-` operands are
/// read here, where the process's own directory is the shell's.
///
/// # Errors
///
/// When `HRN_TOOL_SOCK` is not a usable endpoint or the bridge cannot be reached.
pub fn tool_shim(args: &[String]) -> Result<std::process::ExitCode> {
    let spec = std::env::var(SOCK_VAR).unwrap_or_default();
    let parts: Vec<&str> = spec.split(':').collect();
    let [port, token, cid] = parts.as_slice() else {
        return Ok(usage_exit(&format!(
            "hrn tool: `{SOCK_VAR}` is `{spec}`: expected `<port>:<token>:<call id>` (set by the \
             `bash` tool)\n"
        )));
    };
    let Some(name) = args.first() else {
        return Ok(usage_exit("usage: hrn tool <tool> [arguments] | --help\n"));
    };
    let words = args.get(1..).unwrap_or_default();
    let files = read_operands(words)?;
    let request = serde_json::json!({
        "token": token,
        "cid": cid.parse::<u64>().unwrap_or_default(),
        "tool": name,
        "argv": words,
        "files": files,
    });
    let address = std::net::SocketAddr::new(
        std::net::Ipv4Addr::LOCALHOST.into(),
        port.parse()
            .map_err(|_| HrnError::Config(format!("`{SOCK_VAR}` has no usable port: {spec}")))?,
    );
    let reply = call_bridge(address, &request.to_string())?;
    let mut out = std::io::stdout();
    let io_err = |e: std::io::Error| HrnError::Io(e.to_string());
    std::io::Write::write_all(&mut out, &reply.stdout).map_err(io_err)?;
    std::io::Write::flush(&mut out).map_err(io_err)?;
    std::io::Write::write_all(&mut std::io::stderr(), &reply.stderr).map_err(io_err)?;
    Ok(std::process::ExitCode::from(reply.code))
}

/// Environment variable naming the bridge endpoint (`<port>:<token>:<call id>`).
pub const SOCK_VAR: &str = "HRN_TOOL_SOCK";

/// Tell the shell's user why the command cannot run, and exit 2 (a bad command line).
fn usage_exit(message: &str) -> std::process::ExitCode {
    let _ = std::io::Write::write_all(&mut std::io::stderr(), message.as_bytes());
    std::process::ExitCode::from(2)
}

/// Read `-` (stdin) and `@FILE` operands, keyed by their position in `argv`.
fn read_operands(argv: &[String]) -> Result<std::collections::BTreeMap<usize, String>> {
    use std::collections::BTreeMap;
    let mut files = BTreeMap::new();
    let mut stdin: Option<String> = None;
    for (position, token) in argv.iter().enumerate() {
        let text = match token.strip_prefix('@') {
            Some(path) if !path.is_empty() => {
                let full = std::path::Path::new(path);
                let full = if full.is_absolute() {
                    full.to_path_buf()
                } else {
                    std::env::current_dir()
                        .map_err(|e| HrnError::Io(format!("cwd: {e}")))?
                        .join(path)
                };
                match std::fs::read_to_string(&full) {
                    Ok(text) => text,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(err) => return Err(HrnError::Io(format!("read `{path}`: {err}"))),
                }
            }
            _ if token == "-" => {
                // One read per command line, shared by every `-`.
                if stdin.is_none() {
                    let mut text = String::new();
                    std::io::stdin()
                        .read_to_string(&mut text)
                        .map_err(|e| HrnError::Io(format!("read stdin: {e}")))?;
                    stdin = Some(text);
                }
                stdin.clone().unwrap_or_default()
            }
            _ => continue,
        };
        files.insert(position, text);
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo;

    impl ToolBridge for Echo {
        fn handle(&self, request: String, _: hrn_kernel::CancelFlag) -> BoxFuture<Result<Reply>> {
            Box::pin(async move {
                let v: serde_json::Value = serde_json::from_str(&request).expect("json");
                let text = v["text"].as_str().unwrap_or_default();
                Ok(Reply {
                    code: 0,
                    stdout: format!("echo {text}").into_bytes(),
                    stderr: Vec::new(),
                })
            })
        }
    }

    fn ask(addr: SocketAddr, token: &str) -> Reply {
        call_bridge(addr, &json!({"token": token, "text": "hi"}).to_string()).expect("call")
    }

    #[test]
    fn a_request_round_trips_and_a_bad_token_is_refused() {
        // A worker thread is needed: the answer runs on this runtime while the test blocks.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(async {
            BridgeServer::start(
                "127.0.0.1:0".parse().expect("addr"),
                "sekrit".to_owned(),
                Arc::new(Echo),
            )
            .expect("bind")
        });
        let ok = ask(server.addr(), "sekrit");
        assert_eq!((ok.code, ok.stdout.as_slice()), (0, b"echo hi".as_slice()));
        let bad = ask(server.addr(), "wrong");
        assert_eq!(bad.code, 2);
        assert!(String::from_utf8_lossy(&bad.stderr).contains("token"));
    }
}
