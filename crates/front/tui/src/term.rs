//! The real terminal: raw mode on the normal screen, restored on exit and on panic, and the
//! input thread that turns blocking reads into channel messages.

use crate::port::UiSender;
use crate::vim::CursorShape;
use crossterm::event::{self, Event, KeyEventKind, MouseButton, MouseEventKind};
use crossterm::terminal;
use crossterm::{cursor, execute};
use std::io::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static MOUSE_ENABLED: AtomicBool = AtomicBool::new(false);
static IN_TERMINAL: AtomicBool = AtomicBool::new(false);

/// Switch to raw mode, staying on the normal screen: the transcript lives in the terminal's own
/// scrollback, so mouse reporting stays off by default (`[tui] mouse = false`) and the terminal's
/// selection and wheel keep working. When `mouse == true`, minimal mouse capture is enabled for
/// click-to-focus on the pinned Subagents jump block.
/// Colors are forced on: the TUI owns its rows, so `NO_COLOR` (meant for line-oriented output)
/// does not apply to it.
pub(crate) fn enter(mouse: bool) -> io::Result<()> {
    crossterm::style::force_color_output(true);
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    // The Windows console reports keys and pastes through its own input API instead.
    #[cfg(not(windows))]
    execute!(
        out,
        event::EnableBracketedPaste,
        event::EnableFocusChange,
        event::PushKeyboardEnhancementFlags(
            event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    )
    .ok();
    if mouse {
        execute!(out, event::EnableMouseCapture).ok();
        MOUSE_ENABLED.store(true, Ordering::SeqCst);
    }
    // Any crossterm command makes it switch the Windows console to escape-sequence output,
    // which the inline renderer's raw writes rely on.
    if let Err(err) = execute!(out, cursor::Hide) {
        leave().ok();
        return Err(err);
    }
    IN_TERMINAL.store(true, Ordering::SeqCst);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave().ok();
        previous(info);
    }));
    Ok(())
}

/// Undo [`enter`]; every step runs even when an earlier one fails. Always restores the default
/// terminal cursor shape via DECSCUSR (`\x1b[0 q`).
pub(crate) fn leave() -> io::Result<()> {
    IN_TERMINAL.store(false, Ordering::SeqCst);
    let mut out = io::stdout();
    let _ = out.write_all(CursorShape::Default.decscusr());
    let _ = out.flush();
    if MOUSE_ENABLED.swap(false, Ordering::SeqCst) {
        execute!(out, event::DisableMouseCapture).ok();
    }
    #[cfg(not(windows))]
    execute!(
        out,
        event::PopKeyboardEnhancementFlags,
        event::DisableFocusChange,
        event::DisableBracketedPaste
    )
    .ok();
    let shown = execute!(out, cursor::Show);
    let raw = terminal::disable_raw_mode();
    shown.and(raw)
}

struct RestoreGuard {
    mouse: bool,
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        let _ = terminal::enable_raw_mode();
        let mut out = io::stdout();
        #[cfg(not(windows))]
        execute!(
            out,
            event::EnableBracketedPaste,
            event::EnableFocusChange,
            event::PushKeyboardEnhancementFlags(
                event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )
        .ok();
        if self.mouse {
            execute!(out, event::EnableMouseCapture).ok();
            MOUSE_ENABLED.store(true, Ordering::SeqCst);
        }
        execute!(out, cursor::Hide).ok();
        IN_TERMINAL.store(true, Ordering::SeqCst);
    }
}

/// Temporarily suspend raw mode and terminal enhancements while an external editor runs,
/// restoring them automatically even if `f` panics. No-op in headless tests.
pub(crate) fn with_suspended<T>(f: impl FnOnce() -> T) -> T {
    if !IN_TERMINAL.load(Ordering::SeqCst) {
        return f();
    }
    let mouse = MOUSE_ENABLED.load(Ordering::SeqCst);
    let _ = leave();
    let _guard = RestoreGuard { mouse };
    f()
}

/// The terminal's size in columns and rows.
pub(crate) fn size() -> io::Result<(u16, u16)> {
    terminal::size()
}

/// Read terminal input on a thread of its own. Events that arrive together are sent as one
/// batch, which is how a paste without bracketed-paste support is recognised.
pub(crate) fn spawn_input(tx: UiSender, mouse: bool) -> io::Result<()> {
    thread::Builder::new()
        .name("hrn-tui-input".to_owned())
        .spawn(move || {
            while let Ok(first) = event::read() {
                let mut batch = Vec::new();
                keep(&mut batch, first, mouse);
                while matches!(event::poll(Duration::ZERO), Ok(true)) {
                    match event::read() {
                        Ok(event) => keep(&mut batch, event, mouse),
                        Err(_) => break,
                    }
                }
                if !batch.is_empty() && !tx.send_terminal(batch) {
                    break;
                }
            }
        })
        .map(drop)
}

fn keep(batch: &mut Vec<Event>, event: Event, mouse: bool) {
    let wanted = match &event {
        Event::Key(key) => key.kind != KeyEventKind::Release,
        Event::Mouse(ev) => mouse && matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)),
        Event::FocusGained | Event::FocusLost | Event::Paste(_) | Event::Resize(..) => true,
    };
    if wanted {
        batch.push(event);
    }
}
