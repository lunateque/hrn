//! The welcome banner of a new session: the product name and version. The lockup is centered and
//! written into the scrollback above the first message; settings live in the status band.

use crate::port_theme::SymbolPreset;
use crate::symbols;
use crate::theme;
use crate::wrap;
use hrn_kernel::SessionMeta;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// Maximum number of recent cwd sessions shown under the welcome banner.
const RECENT_LIMIT: usize = 4;

/// The lockup rows: the name and the version.
fn lockup() -> Vec<Line<'static>> {
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    vec![
        Line::from(Span::styled(
            "hrn",
            theme::plain().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(version, theme::dim())),
    ]
}

fn recent_age(then_ms: i64, now_ms: i64) -> String {
    if now_ms <= 0 || then_ms <= 0 {
        return String::new();
    }
    let secs = (now_ms - then_ms).max(0) / 1000;
    match secs {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{}m ago", secs / 60),
        86_400..2_592_000 => format!("{}d ago", secs / 86_400),
        _ => format!("{}mo ago", secs / 2_592_000),
    }
}

/// The welcome banner for a session with no entries yet: the lockup centered in `width`
/// columns, written into the scrollback once.
#[cfg(test)]
pub(crate) fn lines(width: usize) -> Vec<Line<'static>> {
    lines_with_recent_preset(width, &[], 0, SymbolPreset::Unicode)
}

/// The welcome banner plus up to 4 most recent sessions for the current working directory.
#[cfg(test)]
pub(crate) fn lines_with_recent(
    width: usize,
    recent: &[SessionMeta],
    now_ms: i64,
) -> Vec<Line<'static>> {
    lines_with_recent_preset(width, recent, now_ms, SymbolPreset::Unicode)
}

/// The welcome banner plus up to 4 most recent sessions, honoring `preset`.
pub(crate) fn lines_with_recent_preset(
    width: usize,
    recent: &[SessionMeta],
    now_ms: i64,
    preset: SymbolPreset,
) -> Vec<Line<'static>> {
    let rows = lockup();
    // The name and the version sit together: center the lockup rows.
    let block = rows.iter().take(2).map(Line::width).max().unwrap_or(0);
    let mut out: Vec<Line<'static>> = rows
        .into_iter()
        .enumerate()
        .map(|(i, mut line)| {
            let used = if i < 2 { block } else { line.width() };
            line.spans
                .insert(0, Span::raw(" ".repeat(width.saturating_sub(used) / 2)));
            line
        })
        .collect();
    if !recent.is_empty() && width >= 28 {
        out.push(Line::default());
        let header = Line::from(vec![
            Span::styled(
                "Recent sessions",
                theme::muted().add_modifier(Modifier::BOLD),
            ),
            Span::styled("  (/sessions to resume)", theme::dim()),
        ]);
        let pad = width.saturating_sub(header.width()) / 2;
        let mut h = header;
        h.spans.insert(0, Span::raw(" ".repeat(pad)));
        out.push(h);
        let max_row = width.saturating_sub(6).min(68);
        for session in recent.iter().take(RECENT_LIMIT) {
            let title = if session.title.trim().is_empty() {
                "(untitled)"
            } else {
                session.title.as_str()
            };
            let age = recent_age(session.updated_at_ms, now_ms);
            let suffix = if age.is_empty() {
                String::new()
            } else {
                format!("  {age}")
            };
            let title_room = max_row.saturating_sub(wrap::str_width(&suffix) + 2);
            let bullet = format!("{} ", symbols::for_preset(preset).dot);
            let mut row = Line::from(vec![
                Span::styled(bullet, theme::accent()),
                Span::styled(
                    wrap::truncate(&wrap::sanitize(title), title_room).into_owned(),
                    theme::plain(),
                ),
                Span::styled(suffix, theme::dim()),
            ]);
            let rpad = width.saturating_sub(row.width()) / 2;
            row.spans.insert(0, Span::raw(" ".repeat(rpad)));
            out.push(row);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paint(width: usize) -> Vec<String> {
        lines(width)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn the_name_and_version_show() {
        let rows = paint(80);
        let all = rows.join("\n");
        assert!(all.contains("hrn"), "{all}");
        assert!(
            all.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            "{all}"
        );
    }

    #[test]
    fn tiny_widths_never_panic() {
        let _ = paint(3);
        let _ = paint(1);
    }

    #[test]
    fn landing_lists_up_to_four_recent_sessions() {
        let sessions: Vec<SessionMeta> = (1..=6)
            .map(|i| SessionMeta {
                id: format!("s{i}"),
                title: format!("session {i}"),
                updated_at_ms: 1000 * i,
                ..SessionMeta::default()
            })
            .collect();
        let rows = lines_with_recent(80, &sessions, 7_200_000);
        let all = rows
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(all.iter().any(|r| r.contains("Recent sessions")));
        assert!(all.iter().any(|r| r.contains("session 3")));
        assert!(all.iter().any(|r| r.contains("session 4")));
        assert!(!all.iter().any(|r| r.contains("session 5")));
    }
}
