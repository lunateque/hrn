//! The `bash` card (the headerless shell renderer): the highlighted command, an `Output`
//! section tail-capped to the last rows, and a footer with the wall time against the timeout and
//! the exit code. The tool appends `[exit code N]`, `[stderr]`, `[warning] …` and
//! `[timed out after Ns; …]` lines to its output; they are read back here, not shown raw.

use super::{Cx, EXPANDED_CAP, expand_hint, short};
use crate::card::Section;
use crate::highlight::{self, Lang};
use crate::theme;
use crate::transcript::ToolState;
use crate::wrap;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::fmt::Write as _;
use std::time::Duration;

/// Command rows kept while collapsed.
const COMMAND_ROWS: usize = 12;
/// Output rows kept while collapsed.
const OUTPUT_ROWS: usize = 10;
/// The tool's default timeout in seconds.
const DEFAULT_TIMEOUT: u64 = 120;

/// The output of a finished call, with the tool's trailer lines taken off.
struct Parsed<'a> {
    lines: Vec<&'a str>,
    exit: Option<i64>,
    timed_out: bool,
}

fn parse(text: &str) -> Parsed<'_> {
    let mut lines: Vec<&str> = text.trim_end().lines().collect();
    let mut parsed = Parsed {
        lines: Vec::new(),
        exit: None,
        timed_out: false,
    };
    while let Some(last) = lines.last().copied() {
        if let Some(code) = last
            .strip_prefix("[exit code ")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            parsed.exit = code.parse().ok();
        } else if last == "[terminated by signal]" {
            parsed.exit = Some(-1);
        } else if last.starts_with("[timed out after") {
            parsed.timed_out = true;
        } else {
            break;
        }
        lines.pop();
    }
    parsed.lines = lines;
    parsed
}

/// The last `cap` rows of `rows`, led by a dim marker saying what was left out.
fn tail(rows: Vec<Line<'static>>, cap: usize, with_total: bool) -> Vec<Line<'static>> {
    let total = rows.len();
    if total <= cap {
        return rows;
    }
    let hidden = total - cap;
    let what = if with_total {
        format!("… ({hidden} earlier lines, showing {cap} of {total}) ")
    } else {
        format!("… {hidden} earlier lines ")
    };
    let mut out = vec![Line::from(vec![
        Span::styled(what, theme::dim()),
        expand_hint(),
    ])];
    out.extend(rows.into_iter().skip(hidden));
    out
}

fn command_rows(cx: &Cx<'_>, inner: usize) -> Vec<Line<'static>> {
    let cwd = cx.arg("cwd");
    let mut lead = String::from("$ ");
    if !cwd.is_empty() {
        let _ = write!(lead, "cd {cwd} && ");
    }
    let first = [Span::styled(lead, theme::dim())];
    let rest = [Span::raw("  ")];
    let command = wrap::sanitize(cx.arg("command"));
    let mut rows = Vec::new();
    for (i, segs) in highlight::lines(&command, Some(Lang::Shell), theme::plain())
        .into_iter()
        .enumerate()
    {
        let prefix = if i == 0 { &first } else { &rest };
        rows.extend(wrap::wrap(&segs, inner, prefix, &rest));
    }
    tail(rows, cx.cap(COMMAND_ROWS), false)
}

/// Output lines in their roles: stderr marker dim, warnings amber, the rest muted (or red when
/// the call died without an exit code).
fn output_rows(lines: &[&str], base: Style, inner: usize) -> Vec<Line<'static>> {
    lines
        .iter()
        .flat_map(|line| {
            let style = if *line == "[stderr]" {
                theme::dim()
            } else if line.starts_with("[warning]") {
                theme::fg(theme::WARNING)
            } else {
                base
            };
            wrap::wrap_text(line, style, inner, &[], &[])
        })
        .collect()
}

fn footer(cx: &Cx<'_>, parsed: &Parsed<'_>, timeout: u64) -> Vec<Span<'static>> {
    if !matches!(cx.state(), ToolState::Done | ToolState::Failed) {
        return cx.footer();
    }
    let mut parts = Vec::new();
    if let Some(wall) = cx.wall() {
        let limit = short(Duration::from_secs(timeout));
        parts.push(Span::styled(
            format!("{}/{limit}", short(wall)),
            theme::dim(),
        ));
    }
    if parsed.timed_out {
        parts.push(Span::styled("⚠ timed out", theme::fg(theme::WARNING)));
    }
    if let Some(code) = parsed.exit.filter(|c| *c != 0) {
        let style = theme::fg(theme::ERROR).add_modifier(Modifier::BOLD);
        parts.push(Span::styled(format!("✘ {code}"), style));
    }
    let mut out = Vec::new();
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            out.push(Span::styled(" · ", theme::dim()));
        }
        out.push(part);
    }
    out
}

pub(super) fn render(cx: &Cx<'_>) -> Vec<Line<'static>> {
    let card = cx.card;
    let mut frame = cx.frame(1);
    frame.bg = None;
    let inner = frame.inner();
    let live = matches!(cx.state(), ToolState::Running | ToolState::Awaiting);
    let shows_output = live || matches!(cx.state(), ToolState::Done | ToolState::Failed);
    let text = if live { &card.live } else { &card.output };
    let parsed = if shows_output {
        parse(text)
    } else {
        Parsed {
            lines: Vec::new(),
            exit: None,
            timed_out: false,
        }
    };
    if parsed.timed_out {
        frame.border = theme::WARNING;
    }
    let timeout = cx
        .args
        .get("timeout_secs")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT);
    let intent = ["i", "intent", "description"]
        .iter()
        .map(|key| cx.arg(key).trim())
        .find(|v| !v.is_empty());
    let header = intent.map_or_else(Vec::new, |text| {
        vec![Span::styled(
            wrap::sanitize(text).into_owned(),
            theme::muted(),
        )]
    });
    let mut sections = vec![Section::plain(command_rows(cx, inner))];
    if !parsed.lines.is_empty() {
        let died = cx.state() == ToolState::Failed && parsed.exit.is_none() && !parsed.timed_out;
        let base = if died {
            theme::fg(theme::ERROR)
        } else {
            theme::muted()
        };
        let rows = output_rows(&parsed.lines, base, inner);
        let rows = tail(rows, cx.cap(OUTPUT_ROWS).min(EXPANDED_CAP), true);
        sections.push(Section::labelled(
            vec![Span::styled("Output", theme::plain())],
            rows,
        ));
    }
    frame.render(header, sections, footer(cx, &parsed, timeout))
}
