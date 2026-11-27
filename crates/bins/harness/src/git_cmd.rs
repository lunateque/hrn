//! `hrn git [revision]`: standalone fullscreen git workbench CLI command.

use crate::cli::GitArgs;
use crate::commit::generate_commit_message;
use crate::config::Config;
use hrn_edge_http::HttpClient;
use hrn_edge_llm::LlmRouter;
use hrn_edge_oauth::OAuthManager;
use hrn_kernel::{HrnError, Result};
use hrn_tui::{find_repo_root, run_git_workbench, validate_revision};
use std::process::ExitCode;

/// Launch the interactive git workbench for the current repository.
pub(crate) fn run(cfg: Option<&Config>, args: &GitArgs) -> Result<ExitCode> {
    if let Some(ref rev) = args.revision {
        validate_revision(rev).map_err(HrnError::Config)?;
    }
    let cwd = std::env::current_dir()?;
    let repo_root = find_repo_root(&cwd).map_err(HrnError::Tool)?;
    let msg_gen = build_msg_generator(cfg);
    run_git_workbench(repo_root, args.revision.clone(), msg_gen)
        .map_err(|e| HrnError::Tool(format!("git workbench failed: {e}")))?;
    Ok(ExitCode::SUCCESS)
}

type MsgGenFn = Box<dyn FnMut(&str) -> std::result::Result<String, String>>;

fn build_msg_generator(cfg: Option<&Config>) -> Option<MsgGenFn> {
    let llm_cfg = cfg?.llm.clone();
    let http = HttpClient::new().ok()?;
    let router = LlmRouter::try_new(llm_cfg.clone(), http, OAuthManager::default()).ok()?;
    Some(Box::new(move |staged_diff: &str| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("failed to start async runtime: {e}"))?;
        rt.block_on(generate_commit_message(&router, &llm_cfg, staged_diff))
            .map_err(|e| e.to_string())
    }))
}
