//! Unbounded TTY line/paste input for `hrn login`.
//!
//! Cooked-mode console `read_line` on Windows enforces a small OS line buffer (~254–1024 chars),
//! which truncates long `OAuth2` callback URLs and API keys when pasted. When stdin is a TTY,
//! this module temporarily switches to raw mode + bracketed paste (`crossterm`) so pastes of any
//! length work, and also supports direct OS clipboard paste via `Ctrl+V`, `Alt+V`, or typing
//! `paste` + Enter.

use crate::term::out;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::sync::mpsc;

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Best-effort OS clipboard text read (returns `None` on headless systems or empty clipboards).
pub(crate) fn read_clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// Sanitize a pasted string: when `strip_ws` is `true` (`OAuth` redirect URLs / `code#state`),
/// remove all whitespace introduced by terminal wrapping; otherwise strip CR/LF only.
pub(crate) fn sanitize_pasted(raw: &str, strip_ws: bool) -> String {
    if strip_ws {
        raw.chars().filter(|c| !c.is_whitespace()).collect()
    } else {
        raw.replace(['\r', '\n'], "")
    }
}

fn expand_submission(raw: &str, strip_ws: bool) -> Result<String> {
    if strip_ws && raw.trim().eq_ignore_ascii_case("paste") {
        let clip = read_clipboard_text()
            .map(|t| sanitize_pasted(&t, true))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| HrnError::Io("clipboard is empty or unavailable".into()))?;
        return Ok(clip);
    }
    Ok(if strip_ws {
        sanitize_pasted(raw, true)
    } else {
        raw.trim().to_owned()
    })
}

/// RAII guard restoring bracketed paste and raw mode on every exit path.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let _ = execute!(io::stdout(), EnableBracketedPaste);
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
}

fn write_raw(s: &str) {
    let mut stdout = io::stdout();
    let _ = stdout.write_all(s.as_bytes());
    let _ = stdout.flush();
}

enum KeyStep {
    Continue,
    Submit,
    Eof,
}

fn handle_raw_key(
    key: KeyEvent,
    buf: &mut String,
    strip_ws: bool,
    cancel: Option<&CancelFlag>,
) -> Result<KeyStep> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('c' | 'C') if ctrl => {
            if let Some(flag) = cancel {
                flag.cancel();
            }
            write_raw("\r\n");
            return Err(HrnError::Cancelled);
        }
        KeyCode::Char('d' | 'D') if ctrl && buf.is_empty() => {
            write_raw("\r\n");
            return Ok(KeyStep::Eof);
        }
        KeyCode::Char('v' | 'V') if (ctrl && !alt) || (alt && !ctrl) => {
            if let Some(text) = read_clipboard_text() {
                let added = sanitize_pasted(&text, strip_ws);
                if !added.is_empty() {
                    buf.push_str(&added);
                    write_raw(&format!("[pasted {} chars]", added.chars().count()));
                }
            }
        }
        KeyCode::Char('u' | 'U') if ctrl => {
            buf.clear();
            write_raw("\r\x1b[2K> ");
        }
        KeyCode::Backspace => {
            if buf.pop().is_some() {
                write_raw("\x08 \x08");
            }
        }
        KeyCode::Enter => {
            // Without bracketed paste on Windows console, wrapped lines arrive as a burst with
            // an Enter immediately followed by more characters; ignore mid-paste line breaks.
            if strip_ws && event::poll(Duration::ZERO).unwrap_or(false) {
                return Ok(KeyStep::Continue);
            }
            write_raw("\r\n");
            return Ok(KeyStep::Submit);
        }
        KeyCode::Char(c) if (!ctrl && !alt) || (ctrl && alt) => {
            if strip_ws && c.is_whitespace() {
                return Ok(KeyStep::Continue);
            }
            buf.push(c);
            let mut enc = [0u8; 4];
            write_raw(c.encode_utf8(&mut enc));
        }
        _ => {}
    }
    Ok(KeyStep::Continue)
}

fn read_tty_line_raw(
    strip_ws: bool,
    stop: Option<&AtomicBool>,
    cancel: Option<&CancelFlag>,
) -> Result<Option<String>> {
    let mut buf = String::new();
    loop {
        if stop.is_some_and(|f| f.load(Ordering::Acquire)) {
            return Ok(None);
        }
        if cancel.is_some_and(CancelFlag::is_cancelled) {
            return Err(HrnError::Cancelled);
        }
        if !event::poll(POLL_INTERVAL).unwrap_or(false) {
            continue;
        }
        let Ok(ev) = event::read() else {
            return Ok(None);
        };
        match ev {
            Event::Paste(text) => {
                let added = sanitize_pasted(&text, strip_ws);
                if !added.is_empty() {
                    buf.push_str(&added);
                    write_raw(&format!("[pasted {} chars]", added.chars().count()));
                }
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                match handle_raw_key(key, &mut buf, strip_ws, cancel)? {
                    KeyStep::Continue => {}
                    KeyStep::Submit => return Ok(Some(buf)),
                    KeyStep::Eof => return Ok(None),
                }
            }
            _ => {}
        }
    }
}

/// Prompt the user for one line on stdin without OS cooked-mode line length truncation.
pub(crate) fn prompt_line(label: &str, strip_ws: bool) -> Result<String> {
    out(label);
    let _ = io::stdout().flush();
    if io::stdin().is_terminal()
        && let Ok(_guard) = RawModeGuard::enter()
    {
        let raw = read_tty_line_raw(strip_ws, None, None)?.unwrap_or_default();
        return expand_submission(&raw, strip_ws);
    }
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| HrnError::Io(format!("read stdin: {e}")))?;
    expand_submission(&line, strip_ws)
}

/// Print a notice cleanly whether raw mode is currently active or not.
pub(crate) fn write_notice(msg: &str, raw_armed: &AtomicBool) {
    if raw_armed.load(Ordering::Acquire) && io::stdin().is_terminal() {
        write_raw(&format!("\r{msg}\r\n> "));
    } else {
        crate::term::err_line(msg);
    }
}

/// Background stdin reader for `OAuth` login flows racing a localhost callback with pasted input.
///
/// Restores terminal raw mode synchronously on [`Drop`] so post-login output is never corrupted.
pub(crate) struct StdinLineReader {
    rx: mpsc::UnboundedReceiver<String>,
    stop: Arc<AtomicBool>,
    is_tty: bool,
    handle: Option<JoinHandle<()>>,
}

impl StdinLineReader {
    /// Spawn a background reader that activates raw mode only after `armed` is flipped to `true`
    /// (right after the authorize URL has been printed).
    pub(crate) fn spawn(cancel: CancelFlag) -> (Self, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let armed = Arc::new(AtomicBool::new(false));
        let is_tty = io::stdin().is_terminal();

        let stop_bg = Arc::clone(&stop);
        let armed_bg = Arc::clone(&armed);
        let handle = thread::spawn(move || {
            if is_tty {
                run_tty_reader(&tx, &stop_bg, &armed_bg, &cancel);
            } else {
                for line in io::stdin().lock().lines() {
                    let Ok(line) = line else { break };
                    let cleaned = expand_submission(&line, true).unwrap_or(line);
                    if tx.send(cleaned).is_err() {
                        break;
                    }
                }
            }
        });
        (
            Self {
                rx,
                stop,
                is_tty,
                handle: Some(handle),
            },
            armed,
        )
    }

    pub(crate) fn rx_mut(&mut self) -> &mut mpsc::UnboundedReceiver<String> {
        &mut self.rx
    }
}

impl Drop for StdinLineReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if self.is_tty
            && let Some(handle) = self.handle.take()
        {
            let _ = handle.join();
        }
    }
}

fn run_tty_reader(
    tx: &mpsc::UnboundedSender<String>,
    stop: &AtomicBool,
    armed: &AtomicBool,
    cancel: &CancelFlag,
) {
    while !armed.load(Ordering::Acquire) {
        if stop.load(Ordering::Acquire) || cancel.is_cancelled() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let Ok(_guard) = RawModeGuard::enter() else {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(sanitize_pasted(&line, true)).is_err() {
                break;
            }
        }
        return;
    };
    while !stop.load(Ordering::Acquire) && !cancel.is_cancelled() {
        match read_tty_line_raw(true, Some(stop), Some(cancel)) {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match expand_submission(&line, true) {
                    Ok(val) => {
                        if tx.send(val).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        write_raw(&format!("\r{err}\r\n> "));
                    }
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_pasted_handles_long_wrapped_urls_and_multiline_keys() {
        let long_code = "x".repeat(600);
        let wrapped =
            format!("http://localhost:1455/callback?code={long_code}\r\n  &state=abc123\n");
        let clean = sanitize_pasted(&wrapped, true);
        assert_eq!(
            clean.len(),
            "http://localhost:1455/callback?code=&state=abc123".len() + 600
        );
        assert!(!clean.chars().any(char::is_whitespace));

        assert_eq!(sanitize_pasted("sk-ant-\r\n1234", false), "sk-ant-1234");
    }

    #[test]
    fn expand_submission_trims_or_strips_without_truncation() {
        let long = format!("http://127.0.0.1:1455/cb?code={}&state=s1", "a".repeat(700));
        let Ok(out) = expand_submission(&format!("  {long}\n"), true) else {
            return;
        };
        assert_eq!(out, long);

        let Ok(plain) = expand_submission("  my-provider  \n", false) else {
            return;
        };
        assert_eq!(plain, "my-provider");
    }
}
