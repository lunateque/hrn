//! The actions of the `debug` tool: argument checking and the DAP request each one stands for.

use crate::format;
use crate::manager::{AttachOptions, DebugManager, LaunchOptions, Owner};
use crate::tool::{absolute, bad, duration, number, with_warnings};
use hrn_edge_dap::session::{Resume, Session};
use hrn_edge_dap::{Breakpoint, Kind as BpKind};
use hrn_edge_tools::args::opt_str;
use hrn_kernel::serde_json::{Map, Value, json};
use hrn_kernel::{Result, ToolOutput};
use std::path::Path;
use std::sync::Arc;

pub(crate) async fn dispatch(
    manager: &DebugManager,
    cwd: &Path,
    session_id: &str,
    root: &str,
    args: &Value,
    action: &str,
    secs: u64,
) -> Result<ToolOutput> {
    if action == "launch" {
        return launch(manager, cwd, session_id, root, args, secs).await;
    }
    if action == "attach" {
        return attach(manager, cwd, session_id, root, args, secs).await;
    }
    let session = session_for(manager, args)?;
    match action {
        "set_breakpoint" => edit_breakpoint(&session, args, true, secs).await,
        "remove_breakpoint" => edit_breakpoint(&session, args, false, secs).await,
        "set_instruction_breakpoint" => {
            edit_instruction_breakpoint(&session, args, true, secs).await
        }
        "remove_instruction_breakpoint" => {
            edit_instruction_breakpoint(&session, args, false, secs).await
        }
        "data_breakpoint_info" => data_breakpoint_info(&session, args, secs).await,
        "set_data_breakpoint" => edit_data_breakpoint(&session, args, true, secs).await,
        "remove_data_breakpoint" => edit_data_breakpoint(&session, args, false, secs).await,
        "continue" => resume(&session, Resume::Continue, secs).await,
        "step_over" => resume(&session, Resume::Over, secs).await,
        "step_in" => resume(&session, Resume::In, secs).await,
        "step_out" => resume(&session, Resume::Out, secs).await,
        "pause" => pause(&session, secs).await,
        "evaluate" => evaluate(&session, args, secs).await,
        "stack_trace" => stack_trace(&session, args, secs).await,
        "threads" => threads(&session, secs).await,
        "scopes" => scopes(&session, args, secs).await,
        "variables" => variables(&session, args, secs).await,
        "disassemble" => disassemble(&session, args, secs).await,
        "read_memory" => read_memory(&session, args, secs).await,
        "write_memory" => write_memory(&session, args, secs).await,
        "modules" => modules(&session, args, secs).await,
        "loaded_sources" => loaded_sources(&session, secs).await,
        "custom_request" => custom_request(&session, args, secs).await,
        "output" => Ok(ToolOutput::ok(format::output(&session.output()))),
        "terminate" => terminate(manager, args).await,
        "sessions" => {
            let summaries: Vec<hrn_edge_dap::Summary> = manager
                .list()
                .into_iter()
                .map(|(_, summary)| summary)
                .collect();
            Ok(ToolOutput::ok(format::sessions(&summaries)))
        }
        other => Err(bad(format!("Unsupported debug action: {other}"))),
    }
}

fn session_for(manager: &DebugManager, args: &Value) -> Result<Arc<Session>> {
    manager.session_for(opt_str(args, "session"))
}

fn owner(session_id: &str, root: &str) -> Owner {
    Owner {
        root: root.to_owned(),
        owner: session_id.to_owned(),
    }
}

/// `launch`: start the program under the chosen adapter.
async fn launch(
    manager: &DebugManager,
    ctx_cwd: &Path,
    session_id: &str,
    root: &str,
    args: &Value,
    secs: u64,
) -> Result<ToolOutput> {
    let program_raw = opt_str(args, "program").ok_or_else(|| bad("`launch` requires `program`"))?;
    let cwd = absolute(ctx_cwd, opt_str(args, "cwd").unwrap_or("."));
    let program = absolute(&cwd, program_raw);
    let mut arguments = extra_arguments(args);
    if let Some(program_args) = args.get("args").and_then(Value::as_array) {
        arguments.insert(
            "args".to_owned(),
            Value::Array(
                program_args
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|a| Value::String(a.to_owned()))
                    .collect(),
            ),
        );
    }
    let (session, outcome) = manager
        .launch(LaunchOptions {
            owner: owner(session_id, root),
            program: &program,
            adapter: opt_str(args, "adapter"),
            cwd: &cwd,
            arguments,
            timeout_secs: secs,
        })
        .await?;
    Ok(ToolOutput::ok(format!(
        "{}\nAdapter: {}",
        format::snapshot(&outcome.summary),
        session.adapter_name()
    )))
}

/// `attach`: take over a running process or connect to a debug server.
async fn attach(
    manager: &DebugManager,
    ctx_cwd: &Path,
    session_id: &str,
    root: &str,
    args: &Value,
    secs: u64,
) -> Result<ToolOutput> {
    let cwd = absolute(ctx_cwd, opt_str(args, "cwd").unwrap_or("."));
    let pid = number(args, "pid").and_then(|p| u32::try_from(p).ok());
    let port = number(args, "port").and_then(|p| u16::try_from(p).ok());
    let adapter = opt_str(args, "adapter");
    if pid.is_none() && port.is_none() && adapter.is_none() {
        return Err(bad(
            "attach requires `pid` or `port` unless an explicit `adapter` is given",
        ));
    }
    let (_, outcome) = manager
        .attach(AttachOptions {
            owner: owner(session_id, root),
            pid,
            port,
            host: opt_str(args, "host"),
            adapter,
            cwd: &cwd,
            arguments: extra_arguments(args),
            timeout_secs: secs,
        })
        .await?;
    Ok(ToolOutput::ok(format::snapshot(&outcome.summary)))
}

/// DAP arguments the model added through `json` (merged over the adapter defaults).
fn extra_arguments(args: &Value) -> Map<String, Value> {
    args.get("json")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// The breakpoint kind and entry the arguments describe.
fn breakpoint_entry(session: &Session, args: &Value, action: &str) -> Result<(BpKind, Breakpoint)> {
    let condition = opt_str(args, "condition").map(str::to_owned);
    if let Some(name) = opt_str(args, "function") {
        return Ok((
            BpKind::Function,
            Breakpoint {
                name: Some(name.to_owned()),
                condition,
                ..Default::default()
            },
        ));
    }
    let file = opt_str(args, "file")
        .ok_or_else(|| bad(format!("`{action}` requires `file` or `function`")))?;
    let line = number(args, "line").ok_or_else(|| bad(format!("`{action}` requires `line`")))?;
    let path = absolute(session.cwd(), file);
    Ok((
        BpKind::Source(path.to_string_lossy().into_owned()),
        Breakpoint {
            line: u64::try_from(line.max(0)).ok(),
            condition,
            hit_condition: opt_str(args, "hit_condition").map(str::to_owned),
            ..Default::default()
        },
    ))
}

/// `set_breakpoint` / `remove_breakpoint`.
async fn edit_breakpoint(
    session: &Session,
    args: &Value,
    add: bool,
    secs: u64,
) -> Result<ToolOutput> {
    let (kind, breakpoint) = breakpoint_entry(
        session,
        args,
        if add {
            "set_breakpoint"
        } else {
            "remove_breakpoint"
        },
    )?;
    let edit = if add {
        hrn_edge_dap::Edit::Set(Box::new(breakpoint))
    } else {
        let line = breakpoint.line;
        hrn_edge_dap::Edit::Remove(Box::new(move |b| b.line == line))
    };
    let result = session
        .edit_breakpoints(kind.clone(), edit, duration(secs))
        .await?;
    let text = match &kind {
        BpKind::Source(path) => format::source_breakpoints(path, &result.breakpoints),
        BpKind::Function => format::function_breakpoints(&result.breakpoints),
        _ => String::new(),
    };
    Ok(ToolOutput::ok(with_warnings(&text, &result.warnings)))
}

/// `set_instruction_breakpoint` / `remove_instruction_breakpoint`.
async fn edit_instruction_breakpoint(
    session: &Session,
    args: &Value,
    add: bool,
    secs: u64,
) -> Result<ToolOutput> {
    need(
        session,
        "supportsInstructionBreakpoints",
        "instruction breakpoints",
    )?;
    let reference = opt_str(args, "instruction_reference")
        .ok_or_else(|| bad("instruction breakpoints require `instruction_reference`"))?
        .to_owned();
    let edit = if add {
        hrn_edge_dap::Edit::Set(Box::new(Breakpoint {
            reference: Some(reference),
            offset: number(args, "offset"),
            condition: opt_str(args, "condition").map(str::to_owned),
            hit_condition: opt_str(args, "hit_condition").map(str::to_owned),
            ..Default::default()
        }))
    } else {
        hrn_edge_dap::Edit::Remove(Box::new(move |b| {
            b.reference.as_deref() == Some(&reference)
        }))
    };
    let result = session
        .edit_breakpoints(BpKind::Instruction, edit, duration(secs))
        .await?;
    Ok(ToolOutput::ok(with_warnings(
        &format::instruction_breakpoints(&result.breakpoints),
        &result.warnings,
    )))
}

/// `data_breakpoint_info`: ask the adapter what can be watched under a name.
async fn data_breakpoint_info(session: &Session, args: &Value, secs: u64) -> Result<ToolOutput> {
    need(session, "supportsDataBreakpoints", "data breakpoints")?;
    let name =
        opt_str(args, "name").ok_or_else(|| bad("`data_breakpoint_info` requires `name`"))?;
    let frame = number(args, "frame_id")
        .or(session.summary().frame_id)
        .ok_or_else(|| {
            bad("data_breakpoint_info needs `frame_id` or a stopped target to ask in")
        })?;
    let response = session
        .request(
            "dataBreakpointInfo",
            json!({"name": name, "frameId": frame}),
            duration(secs),
        )
        .await?;
    Ok(ToolOutput::ok(format::data_breakpoint_info(&response)))
}

/// `set_data_breakpoint` / `remove_data_breakpoint`.
async fn edit_data_breakpoint(
    session: &Session,
    args: &Value,
    add: bool,
    secs: u64,
) -> Result<ToolOutput> {
    need(session, "supportsDataBreakpoints", "data breakpoints")?;
    let data_id = opt_str(args, "data_id")
        .ok_or_else(|| bad("data breakpoints require `data_id` (from `data_breakpoint_info`)"))?
        .to_owned();
    let edit = if add {
        hrn_edge_dap::Edit::Set(Box::new(Breakpoint {
            data_id: Some(data_id.clone()),
            access_type: opt_str(args, "access_type").map(str::to_owned),
            hit_condition: opt_str(args, "hit_condition").map(str::to_owned),
            ..Default::default()
        }))
    } else {
        hrn_edge_dap::Edit::Remove(Box::new(move |b| b.data_id == Some(data_id.clone())))
    };
    let result = session
        .edit_breakpoints(BpKind::Data, edit, duration(secs))
        .await?;
    Ok(ToolOutput::ok(with_warnings(
        &format::data_breakpoints(&result.breakpoints),
        &result.warnings,
    )))
}

/// `continue` or one of the steps.
async fn resume(session: &Session, how: Resume, secs: u64) -> Result<ToolOutput> {
    let outcome = session.resume(how, duration(secs)).await?;
    Ok(ToolOutput::ok(format::outcome(how, &outcome, secs)))
}

/// `pause`.
async fn pause(session: &Session, secs: u64) -> Result<ToolOutput> {
    let outcome = session.pause(duration(secs)).await?;
    Ok(ToolOutput::ok(format::snapshot(&outcome.summary)))
}

/// `evaluate`.
async fn evaluate(session: &Session, args: &Value, secs: u64) -> Result<ToolOutput> {
    let expression =
        opt_str(args, "expression").ok_or_else(|| bad("`evaluate` requires `expression`"))?;
    let mut body =
        json!({"expression": expression, "context": opt_str(args, "context").unwrap_or("repl")});
    if let Some(frame) = number(args, "frame_id").or(session.summary().frame_id) {
        put(&mut body, "frameId", Some(frame));
    }
    let response = session.request("evaluate", body, duration(secs)).await?;
    Ok(ToolOutput::ok(format::evaluation(&response)))
}

/// `stack_trace`.
async fn stack_trace(session: &Session, args: &Value, secs: u64) -> Result<ToolOutput> {
    let levels = number(args, "levels").and_then(|l| u64::try_from(l).ok());
    let frames = session.stack_trace(levels, duration(secs)).await?;
    Ok(ToolOutput::ok(format::stack(&frames)))
}

/// `threads`.
async fn threads(session: &Session, secs: u64) -> Result<ToolOutput> {
    let (list, warnings) = session.threads(duration(secs)).await?;
    Ok(ToolOutput::ok(with_warnings(
        &format::threads(&list, &[]),
        &warnings,
    )))
}

/// `scopes`.
async fn scopes(session: &Session, args: &Value, secs: u64) -> Result<ToolOutput> {
    let frame = number(args, "frame_id")
        .or(session.summary().frame_id)
        .ok_or_else(|| bad("No active stack frame. Run stack_trace first or supply frame_id."))?;
    let response = session
        .request("scopes", json!({"frameId": frame}), duration(secs))
        .await?;
    let list = array(&response, "scopes");
    Ok(ToolOutput::ok(format::scopes(&list)))
}

/// `variables`.
async fn variables(session: &Session, args: &Value, secs: u64) -> Result<ToolOutput> {
    let reference = number(args, "variable_ref")
        .or(number(args, "scope_id"))
        .ok_or_else(|| bad("variables requires `variable_ref` or `scope_id`"))?;
    let response = session
        .request(
            "variables",
            json!({"variablesReference": reference}),
            duration(secs),
        )
        .await?;
    let list = array(&response, "variables");
    Ok(ToolOutput::ok(format::variables(&list)))
}

/// `disassemble`.
async fn disassemble(session: &Session, args: &Value, secs: u64) -> Result<ToolOutput> {
    need(session, "supportsDisassembleRequest", "disassembly")?;
    let count = number(args, "instruction_count")
        .ok_or_else(|| bad("disassemble requires `instruction_count`"))?;
    let reference = opt_str(args, "memory_reference")
        .map(str::to_owned)
        .or_else(|| session.summary().instruction_pointer.clone())
        .ok_or_else(|| {
            bad("disassemble needs `memory_reference` or a stopped target with an instruction pointer")
        })?;
    let mut body = json!({"instructionReference": reference, "instructionCount": count});
    put(
        &mut body,
        "instructionOffset",
        number(args, "instruction_offset"),
    );
    put(&mut body, "offset", number(args, "offset"));
    if args.get("resolve_symbols").and_then(Value::as_bool) == Some(true) {
        put_bool(&mut body, "resolveSymbols");
    }
    let response = session.request("disassemble", body, duration(secs)).await?;
    Ok(ToolOutput::ok(format::disassembly(&array(
        &response,
        "instructions",
    ))))
}

/// `read_memory`.
async fn read_memory(session: &Session, args: &Value, secs: u64) -> Result<ToolOutput> {
    need(session, "supportsReadMemoryRequest", "memory reads")?;
    let reference = opt_str(args, "memory_reference")
        .ok_or_else(|| bad("read_memory requires `memory_reference`"))?;
    let count = number(args, "count").ok_or_else(|| bad("read_memory requires `count`"))?;
    let mut body = json!({"memoryReference": reference, "count": count});
    put(&mut body, "offset", number(args, "offset"));
    let response = session.request("readMemory", body, duration(secs)).await?;
    Ok(ToolOutput::ok(format::memory(
        response
            .get("address")
            .and_then(Value::as_str)
            .unwrap_or(reference),
        response.get("data").and_then(Value::as_str).unwrap_or(""),
        response.get("unreadableBytes").and_then(Value::as_i64),
    )))
}

/// `write_memory`.
async fn write_memory(session: &Session, args: &Value, secs: u64) -> Result<ToolOutput> {
    need(session, "supportsWriteMemoryRequest", "memory writes")?;
    let reference = opt_str(args, "memory_reference")
        .ok_or_else(|| bad("write_memory requires `memory_reference`"))?;
    let data = opt_str(args, "data").ok_or_else(|| bad("write_memory requires base64 `data`"))?;
    let mut body = json!({"memoryReference": reference, "data": data});
    put(&mut body, "offset", number(args, "offset"));
    if args.get("allow_partial").and_then(Value::as_bool) == Some(true) {
        put_bool(&mut body, "allowPartial");
    }
    let response = session.request("writeMemory", body, duration(secs)).await?;
    Ok(ToolOutput::ok(format!(
        "Bytes written: {}",
        response
            .get("bytesWritten")
            .and_then(Value::as_i64)
            .unwrap_or(0)
    )))
}

/// `modules`.
async fn modules(session: &Session, args: &Value, secs: u64) -> Result<ToolOutput> {
    need(session, "supportsModulesRequest", "modules")?;
    let mut body = Map::new();
    put_number(&mut body, "startModule", number(args, "start_module"));
    put_number(&mut body, "moduleCount", number(args, "module_count"));
    let response = session
        .request("modules", Value::Object(body), duration(secs))
        .await?;
    Ok(ToolOutput::ok(format::modules(&array(
        &response, "modules",
    ))))
}

/// `loaded_sources`.
async fn loaded_sources(session: &Session, secs: u64) -> Result<ToolOutput> {
    need(session, "supportsLoadedSourcesRequest", "loaded sources")?;
    let response = session
        .request("loadedSources", json!({}), duration(secs))
        .await?;
    Ok(ToolOutput::ok(format::sources(&array(
        &response, "sources",
    ))))
}

/// `custom_request`.
async fn custom_request(session: &Session, args: &Value, secs: u64) -> Result<ToolOutput> {
    let command =
        opt_str(args, "command").ok_or_else(|| bad("`custom_request` requires `command`"))?;
    let body = args.get("arguments").cloned().unwrap_or(Value::Null);
    let response = session.request(command, body, duration(secs)).await?;
    Ok(ToolOutput::ok(format::body(command, &response)))
}

/// `terminate`.
async fn terminate(manager: &DebugManager, args: &Value) -> Result<ToolOutput> {
    let Some((summary, notes, _)) = manager.terminate(opt_str(args, "session")).await else {
        return Ok(ToolOutput::ok("No debug session to terminate."));
    };
    Ok(ToolOutput::ok(with_warnings(
        &format::terminated(&summary),
        &notes,
    )))
}

fn array(body: &Value, key: &str) -> Vec<Value> {
    body.get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn put(body: &mut Value, key: &str, value: Option<i64>) {
    if let Some(value) = value
        && let Some(map) = body.as_object_mut()
    {
        map.insert(key.to_owned(), json!(value));
    }
}

fn put_bool(body: &mut Value, key: &str) {
    if let Some(map) = body.as_object_mut() {
        map.insert(key.to_owned(), json!(true));
    }
}

fn put_number(body: &mut Map<String, Value>, key: &str, value: Option<i64>) {
    if let Some(value) = value {
        body.insert(key.to_owned(), json!(value));
    }
}

fn need(session: &Session, capability: &str, what: &str) -> Result {
    if session.supports(capability) {
        Ok(())
    } else {
        Err(bad(format!(
            "Current adapter does not support {what} (adapter `{}`)",
            session.adapter_name()
        )))
    }
}
