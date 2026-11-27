//! Deterministic mock ACP v1 stdio server executed via self-re-exec in E2E integration tests.

use bytes::Bytes;
use hrn_edge_llm::agy_acp::bridge::BRIDGE_TOKEN_HEADER;
use http::{Method, Request, header};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};

fn emit(val: &Value) {
    if let Ok(mut bytes) = serde_json::to_vec(val) {
        bytes.push(b'\n');
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        let _ = lock.write_all(&bytes);
        let _ = lock.flush();
    }
}

fn log_event(log_path: Option<&str>, line: &str) {
    if let Some(path) = log_path
        && let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

async fn post_bridge(url: &str, token: &str, payload: &Value) -> Result<Value, String> {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let bytes = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let req = Request::builder()
        .method(Method::POST)
        .uri(url)
        .header(BRIDGE_TOKEN_HEADER, token)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(bytes)))
        .map_err(|e| e.to_string())?;
    let resp = client.request(req).await.map_err(|e| e.to_string())?;
    let raw = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    serde_json::from_slice(&raw).map_err(|e| e.to_string())
}

struct ServerEnv {
    mode: String,
    log_path: Option<String>,
    bridge_url: Option<String>,
    bridge_token: Option<String>,
}

pub(crate) fn run_mock_acp_server(mode: &str) {
    let env = ServerEnv {
        mode: mode.to_owned(),
        log_path: std::env::var("HRN_MOCK_LOG").ok(),
        bridge_url: std::env::var("HRN_BRIDGE_URL").ok(),
        bridge_token: std::env::var("HRN_BRIDGE_TOKEN").ok(),
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    let mut session_seq: u64 = 0;

    loop {
        line.clear();
        let Ok(n) = reader.read_line(&mut line) else {
            break;
        };
        if n == 0 {
            break;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            continue;
        };
        let id = msg.get("id");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        log_event(env.log_path.as_deref(), &format!("METHOD:{method}"));
        dispatch_rpc(
            &rt,
            &mut reader,
            &env,
            method,
            id,
            &params,
            &mut session_seq,
        );
    }
}

fn dispatch_rpc<R: BufRead>(
    rt: &tokio::runtime::Runtime,
    reader: &mut R,
    env: &ServerEnv,
    method: &str,
    id: Option<&Value>,
    params: &Value,
    session_seq: &mut u64,
) {
    match method {
        "initialize" => emit_initialize(id, &env.mode),
        "authenticate" => emit(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "authenticated": true }
        })),
        "session/new" => handle_session_new(id, env, session_seq),
        "session/resume" => {
            let sid = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            log_event(env.log_path.as_deref(), &format!("RESUME:{sid}"));
            emit(&json!({ "jsonrpc": "2.0", "id": id, "result": {} }));
        }
        "session/fork" => {
            *session_seq += 1;
            let sid = format!("mock-fork-{session_seq}");
            log_event(env.log_path.as_deref(), &format!("FORK:{sid}"));
            emit(&json!({ "jsonrpc": "2.0", "id": id, "result": { "sessionId": sid } }));
        }
        "session/set_config_option" => {
            let cfg_id = params.get("configId").and_then(Value::as_str).unwrap_or("");
            let val = params.get("value").and_then(Value::as_str).unwrap_or("");
            log_event(env.log_path.as_deref(), &format!("CONFIG:{cfg_id}={val}"));
            emit(&json!({ "jsonrpc": "2.0", "id": id, "result": { "configOptions": [] } }));
        }
        "session/close" => emit(&json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
        "session/cancel" => log_event(env.log_path.as_deref(), "CANCEL_RECEIVED"),
        "session/prompt" => handle_prompt(rt, reader, env, id, params),
        _ => {
            if id.is_some() {
                emit(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "unknown method" }
                }));
            }
        }
    }
}

fn emit_initialize(id: Option<&Value>, mode: &str) {
    let sess_caps = if mode == "fork_capable" {
        json!({ "resume": {}, "close": {}, "fork": {} })
    } else {
        json!({ "resume": {}, "close": {} })
    };
    emit(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": 1,
            "agentInfo": { "name": "Mock ACP", "version": "1.5.18" },
            "agentCapabilities": {
                "loadSession": true,
                "sessionCapabilities": sess_caps,
                "promptCapabilities": { "image": true }
            },
            "authMethods": [
                { "id": "oauth-personal", "name": "Google Personal" },
                { "id": "oauth-business", "name": "Google Business" }
            ]
        }
    }));
}

fn handle_session_new(id: Option<&Value>, env: &ServerEnv, session_seq: &mut u64) {
    if env.mode == "auth_error" {
        emit(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32000, "message": "Authentication required" }
        }));
        return;
    }
    *session_seq += 1;
    let sid = format!("mock-sid-{session_seq}");
    log_event(env.log_path.as_deref(), &format!("NEW:{sid}"));
    emit(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "sessionId": sid,
            "configOptions": [
                {
                    "id": "model",
                    "category": "model",
                    "currentValue": "gemini-3.8-flash",
                    "options": [
                        { "value": "gemini-3.8-flash", "name": "Gemini 3.8 Flash" },
                        { "value": "claude-opus-4-6-thinking", "name": "Claude Opus 4.6" }
                    ]
                },
                {
                    "id": "thought_level",
                    "category": "thought_level",
                    "currentValue": "medium",
                    "options": [
                        { "value": "low", "name": "Low" },
                        { "value": "medium", "name": "Medium" },
                        { "value": "high", "name": "High" },
                        { "value": "xhigh", "name": "XHigh" }
                    ]
                }
            ],
            "_meta": { "antigravity": { "tier": { "name": "Google AI Ultra" } } }
        }
    }));
}

fn handle_prompt<R: BufRead>(
    rt: &tokio::runtime::Runtime,
    reader: &mut R,
    env: &ServerEnv,
    id: Option<&Value>,
    params: &Value,
) {
    let sid = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let text = params
        .pointer("/prompt/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    log_event(env.log_path.as_deref(), &format!("PROMPT:{sid}:{text}"));

    match env.mode.as_str() {
        "quota_plaintext" => {
            emit(&json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": sid,
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "Usage Limit Reached. You have reached your current quota." }
                    }
                }
            }));
            emit(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "stopReason": "end_turn",
                    "usage": { "inputTokens": 0, "outputTokens": 0, "totalTokens": 0 },
                    "_meta": { "quota": { "retry_after_secs": 120, "remaining_fraction": 0.0 } }
                }
            }));
        }
        "tool_roundtrip" => handle_tool_roundtrip(rt, reader, id, sid, env),
        "cancel_mid_tool" => handle_cancel_mid_tool(rt, reader, id, env),
        _ => {
            emit(&json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": sid,
                    "update": {
                        "sessionUpdate": "agent_thought_chunk",
                        "content": { "type": "text", "text": "thinking..." }
                    }
                }
            }));
            emit(&json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": sid,
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": format!("echo[{sid}]: {text}") }
                    }
                }
            }));
            emit(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "stopReason": "end_turn",
                    "usage": { "inputTokens": 10, "outputTokens": 5, "totalTokens": 15 }
                }
            }));
        }
    }
}

fn read_next_json<R: BufRead>(reader: &mut R) -> Option<Value> {
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        if let Ok(v) = serde_json::from_str::<Value>(buf.trim()) {
            return Some(v);
        }
    }
}

fn handle_tool_roundtrip<R: BufRead>(
    rt: &tokio::runtime::Runtime,
    reader: &mut R,
    prompt_id: Option<&Value>,
    sid: &str,
    env: &ServerEnv,
) {
    emit(&json!({
        "jsonrpc": "2.0",
        "id": 9001,
        "method": "session/request_permission",
        "params": {
            "sessionId": sid,
            "toolCall": { "title": "run_command", "kind": "execute" },
            "options": [
                { "optionId": "allow_once", "kind": "allow_once" },
                { "optionId": "reject_once", "kind": "reject_once" }
            ]
        }
    }));
    if let Some(reply) = read_next_json(reader) {
        let opt = reply
            .pointer("/result/outcome/optionId")
            .and_then(Value::as_str)
            .unwrap_or("none");
        log_event(env.log_path.as_deref(), &format!("PERM_NATIVE:{opt}"));
    }

    emit(&json!({
        "jsonrpc": "2.0",
        "id": 9002,
        "method": "session/request_permission",
        "params": {
            "sessionId": sid,
            "toolCall": { "title": "hrn__read", "serverName": "hrn-bridge" },
            "options": [
                { "optionId": "allow_once", "kind": "allow_once" },
                { "optionId": "reject_once", "kind": "reject_once" }
            ]
        }
    }));
    if let Some(reply) = read_next_json(reader) {
        let opt = reply
            .pointer("/result/outcome/optionId")
            .and_then(Value::as_str)
            .unwrap_or("none");
        log_event(env.log_path.as_deref(), &format!("PERM_BRIDGE:{opt}"));
    }

    let (Some(url), Some(tok)) = (env.bridge_url.as_deref(), env.bridge_token.as_deref()) else {
        return;
    };
    let p1 = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "hrn__read", "arguments": { "path": "a.txt" } }
    });
    let p2 = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "hrn__read", "arguments": { "path": "b.txt" } }
    });
    let (r1, r2) =
        rt.block_on(async { tokio::join!(post_bridge(url, tok, &p1), post_bridge(url, tok, &p2)) });

    let t1 = r1
        .ok()
        .and_then(|v| {
            v.pointer("/result/content/0/text")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let t2 = r2
        .ok()
        .and_then(|v| {
            v.pointer("/result/content/0/text")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();

    emit(&json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": sid,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": format!("combined={t1}+{t2}") }
            }
        }
    }));
    emit(&json!({
        "jsonrpc": "2.0",
        "id": prompt_id,
        "result": {
            "stopReason": "end_turn",
            "usage": { "inputTokens": 20, "outputTokens": 10, "totalTokens": 30 }
        }
    }));
}

fn handle_cancel_mid_tool<R: BufRead>(
    rt: &tokio::runtime::Runtime,
    reader: &mut R,
    prompt_id: Option<&Value>,
    env: &ServerEnv,
) {
    let (Some(url), Some(tok)) = (env.bridge_url.as_deref(), env.bridge_token.as_deref()) else {
        return;
    };
    let req = json!({
        "jsonrpc": "2.0",
        "id": 77,
        "method": "tools/call",
        "params": { "name": "hrn__slow", "arguments": {} }
    });
    if let Ok(v) = rt.block_on(post_bridge(url, tok, &req)) {
        let is_err = v
            .pointer("/result/isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = v
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        log_event(
            env.log_path.as_deref(),
            &format!("BRIDGE_CANCEL:{is_err}:{text}"),
        );
    }
    if let Some(msg) = read_next_json(reader)
        && msg.get("method").and_then(Value::as_str) == Some("session/cancel")
    {
        log_event(env.log_path.as_deref(), "CANCEL_RECEIVED");
    }
    emit(&json!({
        "jsonrpc": "2.0",
        "id": prompt_id,
        "result": { "stopReason": "cancelled" }
    }));
}
