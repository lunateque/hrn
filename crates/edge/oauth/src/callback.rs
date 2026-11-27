//! Loopback `OAuth` callback listener raced with pasted redirect input (the browser may run on
//! another machine, or the redirect may not reach this one).

use crate::pkce::parse_manual_callback;
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;

/// How long one local connection may take to deliver its request head.
const CONN_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest callback request head accepted.
const MAX_REQUEST_HEAD: usize = 16 * 1024;
/// Polling interval when checking [`CancelFlag`] during callback wait.
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// Where an authorization code may come from while a login waits.
pub(crate) struct CodeSources<'a> {
    /// Loopback listener receiving the browser redirect (`None` for `--manual`).
    pub(crate) listener: Option<&'a TcpListener>,
    /// Callback path the redirect targets (`/callback`).
    pub(crate) path: &'a str,
    /// CSRF state the code must carry.
    pub(crate) state: &'a str,
    /// Lines the user pastes: a full redirect URL, `code#state`, or `?code=…&state=…`.
    pub(crate) pasted: Option<&'a mut UnboundedReceiver<String>>,
    /// Receives non-fatal notices (an ignored paste).
    pub(crate) notice: &'a (dyn Fn(&str) + Sync),
    /// Optional cancellation handle (e.g. `Esc` in the TUI).
    pub(crate) cancel: Option<&'a CancelFlag>,
}

/// Wait until the browser redirect or a pasted line yields an authorization code.
///
/// A paste that fails to parse or carries another login's state is reported and ignored, so a
/// typo cannot abort the login; stray loopback connections are ignored the same way. Without a
/// listener the paste is the only source, and closed input ends the wait.
pub(crate) async fn await_code(src: CodeSources<'_>) -> Result<String> {
    let CodeSources {
        listener,
        path,
        state,
        mut pasted,
        notice,
        cancel,
    } = src;
    let mut paste_open = pasted.is_some();
    loop {
        if cancel.is_some_and(CancelFlag::is_cancelled) {
            return Err(HrnError::Cancelled);
        }
        if listener.is_none() && !paste_open {
            return Err(HrnError::Auth(
                "input closed before an authorization code was pasted".into(),
            ));
        }
        tokio::select! {
            () = await_cancel(cancel) => return Err(HrnError::Cancelled),
            accepted = accept_next(listener) => {
                let mut socket = accepted?;
                let outcome =
                    tokio::time::timeout(CONN_TIMEOUT, handle_connection(&mut socket, path, state))
                        .await;
                match outcome {
                    Ok(Ok(CallbackOutcome::Code(code))) => return Ok(code),
                    Ok(Ok(CallbackOutcome::ProviderError(err))) => {
                        return Err(HrnError::Auth(format!(
                            "OAuth provider returned error `{err}`"
                        )));
                    }
                    // Stray, slow or broken connection (browser prefetch, port scanner): ignore it
                    // so it cannot abort the login the user is completing in another tab.
                    Ok(Ok(CallbackOutcome::Ignore) | Err(_)) | Err(_) => {}
                }
            }
            line = next_paste(&mut pasted), if paste_open => match line {
                Some(text) if text.trim().is_empty() => {}
                Some(text) => match parse_manual_callback(&text, state) {
                    Ok(code) => return Ok(code),
                    Err(err) => notice(&format!("ignored pasted input: {err}")),
                },
                None => paste_open = false,
            }
        }
    }
}

async fn await_cancel(cancel: Option<&CancelFlag>) {
    let Some(flag) = cancel else {
        return std::future::pending().await;
    };
    while !flag.is_cancelled() {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

async fn accept_next(listener: Option<&TcpListener>) -> Result<TcpStream> {
    match listener {
        Some(listener) => listener
            .accept()
            .await
            .map(|(socket, _)| socket)
            .map_err(|e| HrnError::Io(format!("accept OAuth callback: {e}"))),
        None => std::future::pending().await,
    }
}

async fn next_paste(pasted: &mut Option<&mut UnboundedReceiver<String>>) -> Option<String> {
    match pasted.as_deref_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// What one local connection asked for.
enum CallbackOutcome {
    /// Not our callback (favicon probe, wrong state, malformed): keep listening.
    Ignore,
    /// A callback carrying the expected state and an authorization code.
    Code(String),
    /// A callback carrying the expected state and a provider error.
    ProviderError(String),
}

async fn handle_connection(
    socket: &mut TcpStream,
    expected_path: &str,
    expected_state: &str,
) -> Result<CallbackOutcome> {
    let head = read_request_head(socket).await?;
    let target = head
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_default();
    let (req_path, query) = target.split_once('?').unwrap_or((target, ""));
    if req_path != expected_path {
        reply(socket, "404 Not Found", "Not found.").await;
        return Ok(CallbackOutcome::Ignore);
    }
    let params: HashMap<String, String> = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    if params.get("state").map(String::as_str) != Some(expected_state) {
        reply(socket, "400 Bad Request", "State mismatch. Please retry.").await;
        return Ok(CallbackOutcome::Ignore);
    }
    if let Some(err) = params.get("error") {
        reply(
            socket,
            "400 Bad Request",
            "Login failed. You may close this tab.",
        )
        .await;
        return Ok(CallbackOutcome::ProviderError(err.clone()));
    }
    if let Some(code) = params.get("code").filter(|c| !c.is_empty()) {
        reply(
            socket,
            "200 OK",
            "Authentication complete! You can return to the terminal.",
        )
        .await;
        return Ok(CallbackOutcome::Code(code.clone()));
    }
    reply(
        socket,
        "400 Bad Request",
        "Missing authorization code. Please retry.",
    )
    .await;
    Ok(CallbackOutcome::Ignore)
}

/// Read until the blank line that ends the HTTP request head (a single `read` may return a
/// partial head).
async fn read_request_head(socket: &mut TcpStream) -> Result<String> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() > MAX_REQUEST_HEAD {
            return Err(HrnError::Auth(
                "OAuth callback request head too large".into(),
            ));
        }
        let n = socket
            .read(&mut chunk)
            .await
            .map_err(|e| HrnError::Io(format!("read OAuth callback: {e}")))?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(chunk.get(..n).unwrap_or_default());
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// Best-effort browser reply. A failure here only affects the tab the user is about to close and
/// never the already-captured authorization result, so it is deliberately not propagated.
async fn reply(socket: &mut TcpStream, status: &str, msg: &str) {
    let body = format!("<!doctype html><html><body><h3>{msg}</h3></body></html>");
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if socket.write_all(resp.as_bytes()).await.is_ok() {
        socket.shutdown().await.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::mpsc::unbounded_channel;

    async fn get(port: u16, target: &str) {
        let mut s = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut sink = Vec::new();
        s.read_to_end(&mut sink).await.expect("read");
    }

    fn quiet(_: &str) {}

    #[tokio::test]
    async fn stray_connections_do_not_abort_the_callback_wait() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let waiter = tokio::spawn(async move {
            await_code(CodeSources {
                listener: Some(&listener),
                path: "/callback",
                state: "good-state",
                pasted: None,
                notice: &quiet,
                cancel: None,
            })
            .await
        });
        get(port, "/favicon.ico").await;
        get(port, "/callback?state=evil&code=stolen").await;
        get(port, "/callback?state=good-state&code=real-code").await;
        let code = waiter.await.expect("join").expect("code");
        assert_eq!(code, "real-code");
    }

    #[tokio::test]
    async fn provider_error_with_matching_state_fails_the_login() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let waiter = tokio::spawn(async move {
            await_code(CodeSources {
                listener: Some(&listener),
                path: "/callback",
                state: "s",
                pasted: None,
                notice: &quiet,
                cancel: None,
            })
            .await
        });
        get(port, "/callback?state=s&error=access_denied").await;
        let err = waiter.await.expect("join").expect_err("must fail");
        assert!(err.to_string().contains("access_denied"), "{err}");
    }

    #[tokio::test]
    async fn paste_races_the_listener_and_bad_pastes_are_ignored() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let (tx, mut rx) = unbounded_channel();
        tx.send("garbage?state=other&code=x".to_owned())
            .expect("send");
        tx.send("http://localhost:54545/callback?code=pasted&state=s1".to_owned())
            .expect("send");
        let notices = Mutex::new(Vec::new());
        let note = |msg: &str| notices.lock().expect("lock").push(msg.to_owned());
        let code = await_code(CodeSources {
            listener: Some(&listener),
            path: "/callback",
            state: "s1",
            pasted: Some(&mut rx),
            notice: &note,
            cancel: None,
        })
        .await
        .expect("pasted code");
        assert_eq!(code, "pasted");
        let notices = notices.into_inner().expect("lock");
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("state mismatch"), "{notices:?}");
    }

    #[tokio::test]
    async fn manual_wait_ends_when_input_closes() {
        let (tx, mut rx) = unbounded_channel::<String>();
        drop(tx);
        let err = await_code(CodeSources {
            listener: None,
            path: "/callback",
            state: "s",
            pasted: Some(&mut rx),
            notice: &quiet,
            cancel: None,
        })
        .await
        .expect_err("closed input");
        assert!(err.to_string().contains("input closed"), "{err}");
    }
}
