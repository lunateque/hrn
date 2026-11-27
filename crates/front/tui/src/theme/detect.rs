//! Terminal dark/light tone detection for `[tui.theme] current = "auto"`:
//! pure OSC-11 background luminance and `COLORFGBG` resolvers, plus a non-hanging interactive TTY
//! probe that immediately short-circuits in non-TTY / test environments.

use crate::port_theme::TerminalTone;

/// Rec. 709 relative luminance in `0.0..=1.0`.
#[cfg(not(test))]
pub(crate) fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    (0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b)) / 255.0
}

/// Classify an RGB background colour into dark vs light (`>= 0.5` is light).
#[cfg(not(test))]
pub(crate) fn tone_from_rgb(r: u8, g: u8, b: u8) -> TerminalTone {
    if relative_luminance(r, g, b) >= 0.5 {
        TerminalTone::Light
    } else {
        TerminalTone::Dark
    }
}

/// Parse an `XParseColor` hex component (1, 2, 3, or 4 hex digits) into an 8-bit channel.
#[cfg(not(test))]
fn parse_channel(component: &str) -> Option<u8> {
    let val = u16::from_str_radix(component, 16).ok()?;
    let scaled = match component.len() {
        1 => val * 17,
        2 => val,
        3 => val >> 4,
        4 => val >> 8,
        _ => return None,
    };
    u8::try_from(scaled).ok()
}

/// Parse an xterm OSC-11 reply (`\x1b]11;rgb:RRRR/GGGG/BBBB\x07` or ST-terminated, or `#rrggbb`).
#[cfg(not(test))]
pub(crate) fn parse_osc11_response(raw: &str) -> Option<(u8, u8, u8)> {
    let body = raw
        .split_once("11;")
        .map_or(raw, |(_, after)| after)
        .trim_end_matches(['\x07', '\\', '\x1b', '\r', '\n']);
    let body = body.strip_suffix('\x1b').unwrap_or(body);
    if let Some(rgb) = body
        .strip_prefix("rgb:")
        .or_else(|| body.strip_prefix("RGB:"))
    {
        let mut parts = rgb.split('/');
        let r = parse_channel(parts.next()?)?;
        let g = parse_channel(parts.next()?)?;
        let b = parse_channel(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        return Some((r, g, b));
    }
    let hex = body.strip_prefix('#').filter(|d| d.len() == 6)?;
    let r = u8::from_str_radix(hex.get(0..2)?, 16).ok()?;
    let g = u8::from_str_radix(hex.get(2..4)?, 16).ok()?;
    let b = u8::from_str_radix(hex.get(4..6)?, 16).ok()?;
    Some((r, g, b))
}

/// Parse `COLORFGBG` (`fg;bg` or `fg;extra;bg`): background index `>= 7` indicates a light theme.
#[cfg(not(test))]
pub(crate) fn parse_colorfgbg(raw: &str) -> Option<TerminalTone> {
    let bg_str = raw.split(';').rev().find(|part| !part.trim().is_empty())?;
    let bg: u16 = bg_str.trim().parse().ok()?;
    if bg >= 7 {
        Some(TerminalTone::Light)
    } else {
        Some(TerminalTone::Dark)
    }
}

/// Pure resolver: OSC-11 background luminance beats `COLORFGBG`, defaulting to dark.
#[cfg(not(test))]
pub(crate) fn resolve_tone(osc11_reply: Option<&str>, colorfgbg: Option<&str>) -> TerminalTone {
    if let Some((r, g, b)) = osc11_reply.and_then(parse_osc11_response) {
        return tone_from_rgb(r, g, b);
    }
    if let Some(tone) = colorfgbg.and_then(parse_colorfgbg) {
        return tone;
    }
    TerminalTone::Dark
}

/// Query OSC-11 background colour on an interactive TTY with a bounded 50 ms wait.
/// Never runs during unit tests or when stdio is not a TTY.
#[cfg(not(test))]
fn query_osc11_tty() -> Option<String> {
    use crossterm::event::{self, Event, KeyCode};
    use crossterm::terminal;
    use std::io::{self, IsTerminal, Write};
    use std::time::{Duration, Instant};

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return None;
    }
    let was_raw = terminal::is_raw_mode_enabled().unwrap_or(false);
    if !was_raw && terminal::enable_raw_mode().is_err() {
        return None;
    }
    let mut out = io::stdout();
    let wrote = out.write_all(b"\x1b]11;?\x1b\\").and_then(|()| out.flush());
    let mut reply = String::new();
    if wrote.is_ok() {
        let deadline = Instant::now() + Duration::from_millis(50);
        while reply.len() < 64 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || !matches!(event::poll(remaining), Ok(true)) {
                break;
            }
            let Ok(ev) = event::read() else { break };
            if let Event::Key(key) = ev {
                match key.code {
                    KeyCode::Esc => reply.push('\x1b'),
                    KeyCode::Char(c) => reply.push(c),
                    _ => {}
                }
                if (reply.ends_with('\x07') || reply.ends_with("\x1b\\"))
                    && parse_osc11_response(&reply).is_some()
                {
                    break;
                }
            }
        }
    }
    if !was_raw {
        let _ = terminal::disable_raw_mode();
    }
    (!reply.is_empty()).then_some(reply)
}

/// Cached terminal tone. Probed at most once per process: a mid-session OSC-11 query would
/// race the TUI input thread for the reply bytes, leaking the terminal's answer into the
/// prompt as typed characters.
#[cfg(not(test))]
static TONE: std::sync::OnceLock<TerminalTone> = std::sync::OnceLock::new();

/// Determine the current terminal background tone (`auto` theme selection). The OSC-11 query
/// runs only on the first call (startup, before the input thread exists); every later call
/// reuses the cached tone.
pub(crate) fn probe_terminal_tone() -> TerminalTone {
    #[cfg(test)]
    {
        TerminalTone::Dark
    }
    #[cfg(not(test))]
    {
        *TONE.get_or_init(|| {
            let osc = query_osc11_tty();
            let colorfgbg = std::env::var("COLORFGBG").ok();
            resolve_tone(osc.as_deref(), colorfgbg.as_deref())
        })
    }
}
