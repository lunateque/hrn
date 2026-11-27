//! `hrn remote pair` (alias `web`): opt-in multi-session mobile PWA server + QR pairing CLI,
//! plus shared mobile approval/ask/URL helpers reused by `/remote` in the live TUI.

#[path = "remote_web_backend.rs"]
mod backend;

pub(crate) use backend::RuntimeMobileBackend;

use crate::config::Config;
use crate::term::{err_line, out};
use crate::wiring::{Background, build_runtime_with_mcp, open_store};
use hrn_edge_ipc::{MobileRole, MobileWebHandle, MobileWebServer, QrMatrix, discover_lan_addrs};
use hrn_kernel::{
    Answer, ApprovalDecision, ApprovalGrantScope, ApprovalRequest, Field, Form, HrnError, Kind,
    Result, Submission,
};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Options parsed from `hrn remote pair` / `hrn remote web`.
#[derive(Clone, Debug)]
pub(crate) struct PairOptions {
    pub(crate) role: MobileRole,
    pub(crate) bind: String,
    pub(crate) port: Option<u16>,
    pub(crate) public_url: Option<String>,
    pub(crate) ttl_mins: u64,
}

/// Parse `--bind` + optional `--port` override into a [`SocketAddr`].
pub(crate) fn resolve_bind_addr(bind: &str, port: Option<u16>) -> Result<SocketAddr> {
    let mut addr: SocketAddr = bind
        .trim()
        .parse()
        .map_err(|e| HrnError::Config(format!("invalid --bind address `{bind}`: {e}")))?;
    if let Some(p) = port {
        addr.set_port(p);
    }
    Ok(addr)
}

/// Host machine display name shown in the mobile PWA header.
pub(crate) fn host_display_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "hrn".to_owned())
}

/// Build the primary QR pairing URL plus up to 3 fallback LAN/Tailscale URLs.
pub(crate) fn build_display_urls(
    bound: SocketAddr,
    public_url: Option<&str>,
    fragment: &str,
) -> (String, Vec<String>) {
    let lan_urls: Vec<String> = discover_lan_addrs()
        .into_iter()
        .map(|ip| format!("http://{}/#{fragment}", SocketAddr::new(ip, bound.port())))
        .collect();

    if let Some(base) = public_url
        .map(str::trim)
        .map(|s| s.trim_end_matches('/'))
        .filter(|s| !s.is_empty())
    {
        let primary = format!("{base}/#{fragment}");
        let alts = lan_urls
            .into_iter()
            .filter(|u| u != &primary)
            .take(3)
            .collect();
        return (primary, alts);
    }

    if bound.ip().is_unspecified() {
        let mut iter = lan_urls.into_iter();
        if let Some(primary) = iter.next() {
            return (primary, iter.take(3).collect());
        }
        return (
            format!("http://127.0.0.1:{}/#{fragment}", bound.port()),
            Vec::new(),
        );
    }

    (format!("http://{bound}/#{fragment}"), Vec::new())
}

/// Map a mobile approval tap (`approved`, `always`) to a kernel [`ApprovalDecision`],
/// respecting plain-only (`[ApprovalGrantScope::Once]`) security restrictions.
pub(crate) fn approval_decision_from_mobile(
    request: &ApprovalRequest,
    approved: bool,
    always: bool,
) -> ApprovalDecision {
    if !approved {
        return ApprovalDecision::Deny;
    }
    let only_once = request.choices.as_slice() == [ApprovalGrantScope::Once];
    if always && !only_once {
        ApprovalDecision::tool_session()
    } else {
        ApprovalDecision::once()
    }
}

/// Convert ordered string answers from the mobile PWA into a kernel [`Submission`].
pub(crate) fn answers_to_submission(form: &Form, raw_answers: &[String]) -> Submission {
    let answers: Vec<Answer> = form
        .fields()
        .enumerate()
        .map(|(idx, field)| {
            let raw = raw_answers.get(idx).map_or("", String::as_str).trim();
            answer_for_field(field, raw)
        })
        .collect();
    Submission {
        values: form.values(&answers),
        timed_out: false,
    }
}

fn find_option_index(field: &Field, token: &str) -> Option<usize> {
    field
        .options
        .iter()
        .position(|opt| opt.value == token || opt.label == token)
        .or_else(|| {
            field.options.iter().position(|opt| {
                opt.value.eq_ignore_ascii_case(token) || opt.label.eq_ignore_ascii_case(token)
            })
        })
}

fn answer_for_field(field: &Field, raw: &str) -> Answer {
    let mut ans = Answer::default();
    match &field.kind {
        Kind::One => {
            if let Some(idx) = find_option_index(field, raw) {
                ans.picked.push(idx);
            } else if !raw.is_empty() {
                raw.clone_into(&mut ans.other);
            }
        }
        Kind::Many => {
            let mut extras = Vec::new();
            for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if let Some(idx) = find_option_index(field, part) {
                    if !ans.picked.contains(&idx) {
                        ans.picked.push(idx);
                    }
                } else {
                    extras.push(part);
                }
            }
            if !extras.is_empty() {
                ans.other = extras.join(", ");
            }
        }
        Kind::Flag => {
            if let Some(idx) = find_option_index(field, raw) {
                ans.picked.push(idx);
            } else if matches!(
                raw.to_ascii_lowercase().as_str(),
                "true" | "yes" | "1" | "on"
            ) && !field.options.is_empty()
            {
                ans.picked.push(0);
            }
        }
        Kind::Text(_) => {
            raw.clone_into(&mut ans.text);
        }
        Kind::Hidden(default_val) => {
            ans.text = if raw.is_empty() {
                default_val.clone()
            } else {
                raw.to_owned()
            };
        }
    }
    ans
}

fn print_pairing_qr(
    handle: &MobileWebHandle,
    role: MobileRole,
    public_url: Option<&str>,
    ttl: Duration,
) -> Result<()> {
    let code = handle.create_pairing(role, ttl)?;
    let (primary, alts) = build_display_urls(handle.addr(), public_url, &format!("pair={code}"));
    let qr = QrMatrix::encode(&primary)?;
    let role_label = match role {
        MobileRole::Control => "control (interactive)",
        MobileRole::View => "view-only (read-only)",
    };
    out(&format!(
        "\nhrn mobile remote · {role_label} · listening on {}\n{}\n\nURL:  {primary}\n",
        handle.addr(),
        qr.render_ansi_dark_on_light(),
    ));
    for alt in &alts {
        out(&format!("Alt:  {alt}\n"));
    }
    out(&format!(
        "Code expires in {} min (single-use) · {} paired device(s)\n\
         Keys: [r] rotate QR · [v] view-only QR · [c] control QR · [q / Ctrl+C] stop\n",
        (ttl.as_secs() / 60).max(1),
        handle.connected_count(),
    ));
    Ok(())
}

/// Run `hrn remote pair` (alias `hrn remote web`) until `q` or `Ctrl+C`.
pub(crate) async fn run_pair(cfg: &Config, db_path: &Path, opts: &PairOptions) -> Result<()> {
    let listen_addr = resolve_bind_addr(&opts.bind, opts.port)?;
    let store = open_store(db_path, false)?;
    let cwd = std::env::current_dir()?;
    let mut cfg = cfg.clone();
    let background = Background::new();
    let (runtime, mcp_manager, warnings) =
        build_runtime_with_mcp(&mut cfg, Arc::clone(&store), &cwd, &background).await?;
    for warn in warnings {
        err_line(&format!("warning: {warn}"));
    }

    let models: Vec<String> = hrn_edge_llm::enabled_presets(&cfg.llm)
        .into_iter()
        .map(|(alias, _)| alias.to_owned())
        .collect();
    let backend = Arc::new(RuntimeMobileBackend::new(runtime, store, cwd, models));
    let handle = MobileWebServer::bind(listen_addr, backend).await?;
    let ttl = Duration::from_mins(opts.ttl_mins.max(1));
    let mut current_role = opts.role;
    print_pairing_qr(&handle, current_role, opts.public_url.as_deref(), ttl)?;

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            line = lines.next_line() => {
                match line {
                    Ok(Some(raw)) => match raw.trim().to_ascii_lowercase().as_str() {
                        "q" | "quit" | "exit" => break,
                        "r" | "rotate" => {
                            if let Err(err) = print_pairing_qr(
                                &handle,
                                current_role,
                                opts.public_url.as_deref(),
                                ttl,
                            ) {
                                err_line(&format!("error: {err}"));
                            }
                        }
                        "v" | "view" => {
                            current_role = MobileRole::View;
                            if let Err(err) = print_pairing_qr(
                                &handle,
                                current_role,
                                opts.public_url.as_deref(),
                                ttl,
                            ) {
                                err_line(&format!("error: {err}"));
                            }
                        }
                        "c" | "control" => {
                            current_role = MobileRole::Control;
                            if let Err(err) = print_pairing_qr(
                                &handle,
                                current_role,
                                opts.public_url.as_deref(),
                                ttl,
                            ) {
                                err_line(&format!("error: {err}"));
                            }
                        }
                        "" => {}
                        _ => out("Keys: [r] rotate QR · [v] view-only QR · [c] control QR · [q] stop\n"),
                    },
                    Ok(None) => {
                        let _ = tokio::signal::ctrl_c().await;
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
    }

    out("\nrevoking mobile pairings and stopping server...\n");
    handle.revoke_all();
    handle.shutdown().await;
    mcp_manager.shutdown().await;
    Ok(())
}

#[cfg(test)]
#[path = "remote_web_tests.rs"]
mod tests;
