//! `hrn doctor`: validates configuration, provider credentials, and Antigravity CLI (`agy_acp`)
//! binary, version (`>= 1.5.0`), ACP handshake, and authentication status.

use crate::commands;
use crate::term::out;
use hrn_edge_llm::{ProviderKind, doctor_agy};
use hrn_kernel::{HrnError, Result};
use std::path::PathBuf;
use std::time::Duration;

const DOCTOR_TIMEOUT: Duration = Duration::from_secs(15);

/// Execute `hrn doctor`.
pub(crate) async fn run(overlays: &[PathBuf]) -> Result {
    let mut problems = Vec::new();
    let mut explicit_binary: Option<String> = None;
    let mut has_agy_provider = false;

    match crate::wiring::load_config_with_discovery(overlays).await {
        Ok((cfg, _, _, global, _)) => {
            let (report, cfg_problems) = commands::render_check(&global, &cfg);
            out(&report);
            problems.extend(cfg_problems);
            for prov in cfg.llm.providers.values() {
                if prov.kind == ProviderKind::AgyAcp {
                    has_agy_provider = true;
                    if explicit_binary.is_none() {
                        explicit_binary.clone_from(&prov.agy.binary);
                    }
                }
            }
        }
        Err(err) => {
            out(&format!("config: {err}\n"));
        }
    }

    let cwd = std::env::current_dir()?;
    let agy_report = doctor_agy(explicit_binary.as_deref(), &cwd, DOCTOR_TIMEOUT).await;
    out("\n");
    out(&agy_report.render());

    if has_agy_provider {
        if agy_report.binary.is_none() {
            problems.push("agy_acp configured but `agy` CLI was not found".to_owned());
        } else if !agy_report.version_supported {
            problems.push("`agy` CLI version is unsupported (requires >= 1.5.0)".to_owned());
        } else if !agy_report.acp_ready {
            problems.push("`agy acp --stdio` initialize handshake failed".to_owned());
        } else if agy_report.authenticated == Some(false) {
            problems.push("`agy` CLI is not authenticated (run `hrn login agy`)".to_owned());
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(HrnError::Config(problems.join("; ")))
    }
}
