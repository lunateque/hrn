//! Command handlers split out of `main.rs` to keep every file well under the line cap.

use crate::cli::{
    CommitArgs, ConfigCommand, DryBalanceArgs, GcArgs, GitArgs, McpCommand, ProvidersCommand,
    RemoteCommand, SessionsCommand, UpdateArgs, WorktreeCommand,
};
use crate::{
    active_agent_dir, app_data_dir, auth_location, commands, commit, config_cmd, database_location,
    dry_balance, export, gc, git_cmd, global_config_path, host_key_location, import_cmd,
    layer_input, load_config, mcp, models, omp_import, remote, remote_hosts_location,
    run_file_location, stats, stats_names, update, wiring, worktree,
};
use hrn_edge_tools::StdWorkspaceFs;
use hrn_kernel::{Result, WorkspaceFs as _};
use std::path::PathBuf;
use std::process::ExitCode;

pub(crate) fn sessions_command(command: Option<&SessionsCommand>) -> Result {
    let store = wiring::open_store(&database_location()?, false)?;
    match command {
        None => commands::sessions(store.as_ref()),
        Some(SessionsCommand::Export(args)) => export::run(store.as_ref(), args),
        Some(SessionsCommand::Delete { session, force }) => {
            commands::delete_session(store.as_ref(), session, *force, &commands::ask)
        }
        Some(SessionsCommand::Rename { session, title }) => {
            commands::rename_session(store.as_ref(), session, title)
        }
        Some(SessionsCommand::Pin { session } | SessionsCommand::Unpin { session }) => {
            let pinned = matches!(command, Some(SessionsCommand::Pin { .. }));
            commands::pin_session(store.as_ref(), session, pinned)
        }
        Some(SessionsCommand::Import(args)) => {
            import_cmd::run_sessions_import(store.as_ref(), args)
        }
    }
}

pub(crate) fn gc_command(overlays: &[PathBuf], args: &GcArgs) -> Result {
    let cwd = std::env::current_dir()?;
    let gc_cfg = gc::load_gc_config(
        Some(global_config_path()?),
        overlays,
        &cwd,
        StdWorkspaceFs.home_dir(),
        std::env::vars().collect(),
    )?;
    gc::run_cli(&gc_cfg, &app_data_dir()?, &database_location()?, args.apply)
}

pub(crate) fn stats_command(overlays: &[PathBuf]) -> Result {
    stats::run(
        wiring::open_store(&database_location()?, false)?.as_ref(),
        &stats_names(overlays),
    )
}

pub(crate) async fn models_command(overlays: &[PathBuf]) -> Result {
    let (cfg, ..) = wiring::load_config_with_discovery(overlays).await?;
    models::run(&cfg);
    Ok(())
}

pub(crate) fn providers_command(command: &ProvidersCommand) -> Result {
    match command {
        ProvidersCommand::Import(args) => {
            omp_import::run_cli(args, &active_agent_dir()?, &global_config_path()?)
        }
    }
}

pub(crate) async fn config_subcommand(
    overlays: &[PathBuf],
    command: Option<&ConfigCommand>,
) -> Result {
    if matches!(command, Some(ConfigCommand::Check)) {
        let (cfg, _, _, global, _) = wiring::load_config_with_discovery(overlays).await?;
        return commands::config_check(&global, &cfg);
    }
    let cwd = std::env::current_dir()?;
    let input = layer_input(overlays, &cwd)?;
    let global = global_config_path()?;
    let dir = active_agent_dir()?;
    config_cmd::run(command, &input, &global, &dir)
}

pub(crate) fn dry_balance_command(overlays: &[PathBuf], args: &DryBalanceArgs) -> Result {
    let cfg = load_config(overlays).ok().map(|(c, ..)| c);
    dry_balance::run(&auth_location()?, cfg.as_ref(), args)
}

pub(crate) async fn mcp_command(overlays: &[PathBuf], command: McpCommand) -> Result {
    let (cfg, ..) = load_config(overlays)?;
    let cwd = std::env::current_dir()?;
    match command {
        McpCommand::List => mcp::list(&cfg, &cwd),
        McpCommand::Ping { server } => mcp::ping(&cfg, &cwd, server.as_deref()).await?,
        McpCommand::Serve { profile } => {
            mcp::serve(&cfg, &database_location()?, &profile).await?;
        }
    }
    Ok(())
}

pub(crate) async fn worktree_command(command: WorktreeCommand) -> Result {
    let cwd = std::env::current_dir()?;
    match command {
        WorktreeCommand::Add { slug, base } => worktree::add(&cwd, &slug, base.as_deref()).await,
        WorktreeCommand::List => worktree::list(&cwd).await,
        WorktreeCommand::Rm { target, force } => worktree::remove(&cwd, &target, force).await,
    }
}

pub(crate) async fn dispatch_remote(overlays: &[PathBuf], command: RemoteCommand) -> Result {
    match command {
        RemoteCommand::Serve { listen } => {
            let (cfg, ..) = wiring::load_config_with_discovery(overlays).await?;
            remote::serve(
                &cfg,
                &database_location()?,
                &run_file_location()?,
                &host_key_location()?,
                &listen,
            )
            .await?;
        }
        RemoteCommand::HostKey => remote::host_key(&host_key_location()?)?,
        RemoteCommand::PrintRunFile => remote::print_run_file(&run_file_location()?)?,
        RemoteCommand::Trust { host, fingerprint } => {
            remote::trust(&remote_hosts_location()?, &host, &fingerprint)?;
        }
        RemoteCommand::Hosts => remote::hosts(&remote_hosts_location()?)?,
        RemoteCommand::Setup { host } => {
            remote::setup(&remote_hosts_location()?, &host).await?;
        }
        RemoteCommand::Doctor { host } => {
            remote::doctor(&remote_hosts_location()?, &host).await?;
        }
        RemoteCommand::Forget { host } => remote::forget(&remote_hosts_location()?, &host)?,
        RemoteCommand::Pair {
            control: _,
            view,
            bind,
            port,
            public_url,
            ttl_mins,
        } => {
            let (cfg, ..) = wiring::load_config_with_discovery(overlays).await?;
            let role = if view {
                hrn_edge_ipc::MobileRole::View
            } else {
                hrn_edge_ipc::MobileRole::Control
            };
            crate::remote_web::run_pair(
                &cfg,
                &database_location()?,
                &crate::remote_web::PairOptions {
                    role,
                    bind,
                    port,
                    public_url,
                    ttl_mins,
                },
            )
            .await?;
        }
    }
    Ok(())
}

pub(crate) async fn commit_command(overlays: &[PathBuf], args: &CommitArgs) -> Result<ExitCode> {
    let (cfg, ..) = wiring::load_config_with_discovery(overlays).await?;
    let store = wiring::open_store(&database_location()?, false)?;
    commit::run_cli(&cfg, store.as_ref(), args).await
}

pub(crate) fn git_command(overlays: &[PathBuf], args: &GitArgs) -> Result<ExitCode> {
    let cfg = load_config(overlays).ok().map(|(c, ..)| c);
    git_cmd::run(cfg.as_ref(), args)
}

pub(crate) async fn update_command(overlays: &[PathBuf], args: &UpdateArgs) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let update_cfg = update::load_update_config(
        Some(global_config_path()?),
        overlays,
        &cwd,
        StdWorkspaceFs.home_dir(),
        std::env::vars().collect(),
    )?;
    update::run_cli(&update_cfg, args).await
}
