//! Model-facing text for debug results, one shape per kind of thing reported.

use hrn_edge_dap::session::{Outcome, Resume, RunState, Summary};
use hrn_kernel::serde_json::Value;
use std::fmt::Write as _;

fn text(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn location(path: Option<&str>, line: Option<i64>, column: Option<i64>) -> Option<String> {
    let path = path?;
    match (line, column) {
        (Some(line), Some(column)) if column > 0 => Some(format!("{path}:{line}:{column}")),
        (Some(line), _) => Some(format!("{path}:{line}")),
        _ => Some(path.to_owned()),
    }
}

fn frame_of(source: &Value, line: i64, column: i64) -> String {
    let path = text(source.pointer("/path")).or_else(|| text(source.get("name")));
    location(path, Some(line), Some(column)).unwrap_or_else(|| "<unknown>".to_owned())
}

/// A target's state, as `launch`/`attach`/`pause`/`sessions` show it.
pub(crate) fn snapshot(summary: &Summary) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Session {}", summary.id);
    let _ = writeln!(out, "Adapter: {}", summary.adapter);
    let _ = writeln!(out, "Status: {}", summary.status);
    let _ = writeln!(out, "CWD: {}", summary.cwd);
    if let Some(program) = &summary.program {
        let _ = writeln!(out, "Program: {program}");
    }
    if let Some(reason) = &summary.stop_reason {
        let _ = writeln!(out, "Stop reason: {reason}");
    }
    if let Some(name) = &summary.frame_name {
        let _ = writeln!(out, "Frame: {name}");
    }
    if let Some(pointer) = &summary.instruction_pointer {
        let _ = writeln!(out, "Instruction pointer: {pointer}");
    }
    if let Some(at) = position(summary) {
        let _ = writeln!(out, "Location: {at}");
    }
    if summary.needs_configuration_done {
        out.push_str("Configuration: pending configurationDone; set breakpoints, then continue.\n");
    }
    if let Some(code) = summary.exit_code {
        let _ = writeln!(out, "Exit code: {code}");
    }
    out.trim_end().to_owned()
}

fn verb(how: Resume) -> &'static str {
    match how {
        Resume::Continue => "Continue",
        Resume::Over => "Step over",
        Resume::In => "Step in",
        Resume::Out => "Step out",
    }
}

/// Where a stopped target is, for the one-line reports.
fn position(summary: &Summary) -> Option<String> {
    location(summary.source_path.as_deref(), summary.line, summary.column)
}

/// The end of a resume/step: the state of the target, then the sentence saying what happened.
/// `secs` is the wait the tool allowed, named when it ran out.
pub(crate) fn outcome(how: Resume, outcome: &Outcome, secs: u64) -> String {
    let summary = &outcome.summary;
    let mut lines = vec![snapshot(summary)];
    lines.push(match outcome.state {
        RunState::Stopped => match position(summary) {
            Some(at) => format!("{} stopped at {at}.", verb(how)),
            None => format!("{} stopped at an unknown location.", verb(how)),
        },
        RunState::Terminated => match summary.exit_code {
            Some(code) => format!("Program terminated with exit code {code}."),
            None => "Program terminated.".to_owned(),
        },
        RunState::Running if outcome.timed_out => format!(
            "Program is still running after {secs}s. Use pause to interrupt and inspect state."
        ),
        RunState::Running => "Program is running.".to_owned(),
    });
    for note in &outcome.notes {
        lines.push(format!("Warning: {note}"));
    }
    lines.join("\n")
}

/// The stack of the stopped thread.
pub(crate) fn stack(frames: &[Value]) -> String {
    if frames.is_empty() {
        return "Stack trace:\n(empty)".to_owned();
    }
    let mut out = String::from("Stack trace:");
    for frame in frames {
        let id = frame.get("id").and_then(Value::as_i64).unwrap_or(0);
        let name = text(frame.get("name")).unwrap_or("<anonymous>");
        let line = frame.get("line").and_then(Value::as_i64).unwrap_or(0);
        let column = frame.get("column").and_then(Value::as_i64).unwrap_or(0);
        let at = frame.get("source").map_or_else(
            || format!("<unknown>:{line}:{column}"),
            |s| frame_of(s, line, column),
        );
        let _ = writeln!(out, "- #{id} {name} @ {at}");
    }
    out
}

/// The scopes of a frame.
pub(crate) fn scopes(list: &[Value]) -> String {
    if list.is_empty() {
        return "Scopes:\n(none)".to_owned();
    }
    let mut out = String::from("Scopes:");
    for scope in list {
        let name = text(scope.get("name")).unwrap_or("<unnamed>");
        let reference = scope
            .get("variablesReference")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let expensive = if scope.get("expensive").and_then(Value::as_bool) == Some(true) {
            "yes"
        } else {
            "no"
        };
        let hint = text(scope.pointer("/presentationHint"));
        let _ = match hint {
            Some(hint) => writeln!(
                out,
                "- {name}: ref={reference}, expensive={expensive}, hint={hint}"
            ),
            None => writeln!(out, "- {name}: ref={reference}, expensive={expensive}"),
        };
    }
    out
}

/// The members of a variables reference.
pub(crate) fn variables(list: &[Value]) -> String {
    if list.is_empty() {
        return "Variables:\n(none)".to_owned();
    }
    let mut out = String::from("Variables:");
    for variable in list {
        let name = text(variable.get("name")).unwrap_or("<unnamed>");
        let value = text(variable.get("value")).unwrap_or("");
        let kind = text(variable.get("type"));
        let reference = variable
            .get("variablesReference")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let _ = match (kind, reference > 0) {
            (Some(kind), true) => writeln!(out, "- {name} = {value} ({kind}) [ref={reference}]"),
            (Some(kind), false) => writeln!(out, "- {name} = {value} ({kind})"),
            (None, true) => writeln!(out, "- {name} = {value} [ref={reference}]"),
            (None, false) => writeln!(out, "- {name} = {value}"),
        };
    }
    out
}

/// Threads of every target of the session.
pub(crate) fn threads(list: &[Value], warnings: &[String]) -> String {
    let mut out = String::from("Threads:");
    if list.is_empty() {
        out.push_str("\n(none)");
    }
    for thread in list {
        let id = thread.get("id").and_then(Value::as_i64).unwrap_or(0);
        let name = text(thread.get("name")).unwrap_or("<unnamed>");
        match text(thread.get("target")) {
            Some(target) => {
                let _ = writeln!(out, "- {target} #{id}: {name}");
            }
            None => {
                let _ = writeln!(out, "- #{id}: {name}");
            }
        }
    }
    let _ = write!(out, "{}", notes(warnings));
    out
}

/// Source breakpoints of one file.
pub(crate) fn source_breakpoints(path: &str, list: &[hrn_edge_dap::Breakpoint]) -> String {
    let mut out = format!("Breakpoints for {path}:");
    if list.is_empty() {
        out.push_str("\n(none)");
    }
    for bp in list {
        let _ = writeln!(out, "- line {}: {}", bp.line.unwrap_or(0), describe(bp));
    }
    out
}

/// Function breakpoints.
pub(crate) fn function_breakpoints(list: &[hrn_edge_dap::Breakpoint]) -> String {
    let mut out = String::from("Function breakpoints:");
    if list.is_empty() {
        out.push_str("\n(none)");
    }
    for bp in list {
        let _ = writeln!(
            out,
            "- {}: {}",
            bp.name.as_deref().unwrap_or("?"),
            describe(bp)
        );
    }
    out
}

/// Instruction breakpoints.
pub(crate) fn instruction_breakpoints(list: &[hrn_edge_dap::Breakpoint]) -> String {
    let mut out = String::from("Instruction breakpoints:");
    if list.is_empty() {
        out.push_str("\n(none)");
    }
    for bp in list {
        let offset = bp.offset.unwrap_or(0);
        let _ = writeln!(
            out,
            "- {}{offset}: {}",
            bp.reference.as_deref().unwrap_or("?"),
            describe(bp)
        );
    }
    out
}

/// Data breakpoints.
pub(crate) fn data_breakpoints(list: &[hrn_edge_dap::Breakpoint]) -> String {
    let mut out = String::from("Data breakpoints:");
    if list.is_empty() {
        out.push_str("\n(none)");
    }
    for bp in list {
        let _ = writeln!(
            out,
            "- {} ({}): {}",
            bp.data_id.as_deref().unwrap_or("?"),
            bp.access_type.as_deref().unwrap_or("write"),
            describe(bp)
        );
    }
    out
}

fn describe(bp: &hrn_edge_dap::Breakpoint) -> String {
    let mut out = if bp.verified { "verified" } else { "pending" }.to_owned();
    if let Some(condition) = &bp.condition {
        let _ = write!(out, " if {condition}");
    }
    if let Some(hits) = &bp.hit_condition {
        let _ = write!(out, " after {hits} hits");
    }
    if let Some(message) = &bp.message {
        let _ = write!(out, " ({message})");
    }
    out
}

/// An `evaluate` answer.
pub(crate) fn evaluation(body: &Value) -> String {
    let mut out = format!("Result: {}", text(body.get("result")).unwrap_or(""));
    if let Some(kind) = text(body.get("type")) {
        let _ = writeln!(out, "\nType: {kind}");
    }
    if body
        .get("variablesReference")
        .and_then(Value::as_i64)
        .is_some_and(|r| r > 0)
    {
        let _ = writeln!(
            out,
            "Variables ref: {}",
            body["variablesReference"].as_i64().unwrap_or(0)
        );
    }
    out
}

/// What `data_breakpoint_info` came back with.
pub(crate) fn data_breakpoint_info(body: &Value) -> String {
    let mut out = format!(
        "Data breakpoint info for `{}`:",
        text(body.get("dataId")).unwrap_or("?")
    );
    if let Some(description) = text(body.get("description")) {
        let _ = writeln!(out, "\nDescription: {description}");
    }
    if let Some(access) = body.get("accessTypes").and_then(Value::as_array) {
        let names: Vec<&str> = access.iter().filter_map(Value::as_str).collect();
        let _ = writeln!(out, "Access types: {}", names.join(", "));
    }
    let _ = write!(
        out,
        "Can persist: {}",
        if body.get("canPersist").and_then(Value::as_bool) == Some(true) {
            "yes"
        } else {
            "no"
        }
    );
    out
}

/// An instruction list.
pub(crate) fn disassembly(instructions: &[Value]) -> String {
    let mut out = String::from("Disassembly:");
    if instructions.is_empty() {
        out.push_str("\n(empty)");
    }
    for instruction in instructions {
        let address = text(instruction.get("address")).unwrap_or("?");
        let mnemonic = text(instruction.get("mnemonic")).unwrap_or("");
        let comment = text(instruction.get("comment"));
        let _ = match comment {
            Some(comment) => writeln!(out, "{address}: {mnemonic}  ; {comment}"),
            None => writeln!(out, "{address}: {mnemonic}"),
        };
    }
    out
}

/// A memory dump.
pub(crate) fn memory(address: &str, data: &str, unreadable: Option<i64>) -> String {
    let mut out = format!("Memory at {address} (base64):\n{data}");
    if unreadable.is_some_and(|n| n > 0) {
        let _ = write!(out, "\nUnreadable bytes: {}", unreadable.unwrap_or(0));
    }
    out
}

/// Loaded modules.
pub(crate) fn modules(list: &[Value]) -> String {
    let mut out = String::from("Modules:");
    if list.is_empty() {
        out.push_str("\n(none)");
    }
    for module in list {
        let name = text(module.get("name")).unwrap_or("<unnamed>");
        let path = text(module.get("path"));
        let _ = match path {
            Some(path) => writeln!(out, "- {name} ({path})"),
            None => writeln!(out, "- {name}"),
        };
    }
    out
}

/// Sources the adapter has loaded.
pub(crate) fn sources(list: &[Value]) -> String {
    let mut out = String::from("Loaded sources:");
    if list.is_empty() {
        out.push_str("\n(none)");
    }
    for source in list {
        let name = text(source.get("name")).unwrap_or("?");
        let _ = writeln!(out, "- {name}");
    }
    out
}

/// The body of any other request.
pub(crate) fn body(command: &str, value: &Value) -> String {
    match hrn_kernel::serde_json::to_string_pretty(value) {
        Ok(text) if text != "null" && text != "{}" => format!("{command} result:\n{text}"),
        _ => format!("{command}: no result"),
    }
}

/// The last words of a terminated session.
pub(crate) fn terminated(summary: &Summary) -> String {
    format!("{}\nDebug session terminated.", snapshot(summary))
}

/// Captured program/adapter output.
pub(crate) fn output(text: &str) -> String {
    if text.is_empty() {
        "(no output captured)".to_owned()
    } else {
        text.to_owned()
    }
}

/// Every target, live first, as one block each.
pub(crate) fn sessions(list: &[Summary]) -> String {
    if list.is_empty() {
        return "No debug sessions.".to_owned();
    }
    list.iter()
        .map(|summary| {
            let mut lines = vec![
                format!("{}: {}", summary.id, summary.status),
                format!("  adapter={}", summary.adapter),
                format!("  cwd={}", summary.cwd),
            ];
            if let Some(program) = &summary.program {
                lines.push(format!("  program={program}"));
            }
            if let Some(at) = position(summary) {
                lines.push(format!("  location={at}"));
            }
            if let Some(reason) = &summary.stop_reason {
                lines.push(format!("  reason={reason}"));
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Warnings that came with a result.
pub(crate) fn notes(warnings: &[String]) -> String {
    let mut out = String::new();
    for warning in warnings {
        out.push_str("\nWarning: ");
        out.push_str(warning);
    }
    out
}
