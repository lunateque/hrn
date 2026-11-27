//! Fast synchronous CLI subcommand dispatch that avoids starting a Tokio runtime.

use crate::cli::{AuthCommand, Cli, Command, McpCommand, RemoteCommand, UsageCommand};
use crate::{
    NO_SESSION_MSG, auth, auth_location, commands, completions, continue_misplaced, dry_balance,
    gc_command, git_command, global_config_path, host_key_location, load_config, mcp, remote,
    remote_hosts_location, run_file_location, sessions_command, stats_command, usage,
};
use hrn_edge_tools::StdWorkspaceFs;
use hrn_kernel::Result;
use std::process::ExitCode;

fn ok_unit(res: Result) -> Result<ExitCode> {
    res.map(|()| ExitCode::SUCCESS)
}

fn dispatch_remote_sync(command: &RemoteCommand) -> Option<Result<ExitCode>> {
    match command {
        RemoteCommand::HostKey => Some(ok_unit(
            host_key_location().and_then(|p| remote::host_key(&p)),
        )),
        RemoteCommand::PrintRunFile => Some(ok_unit(
            run_file_location().and_then(|p| remote::print_run_file(&p)),
        )),
        RemoteCommand::Trust { host, fingerprint } => Some(ok_unit(
            remote_hosts_location().and_then(|p| remote::trust(&p, host, fingerprint)),
        )),
        RemoteCommand::Hosts => Some(ok_unit(
            remote_hosts_location().and_then(|p| remote::hosts(&p)),
        )),
        RemoteCommand::Forget { host } => Some(ok_unit(
            remote_hosts_location().and_then(|p| remote::forget(&p, host)),
        )),
        RemoteCommand::Serve { .. }
        | RemoteCommand::Setup { .. }
        | RemoteCommand::Doctor { .. }
        | RemoteCommand::Pair { .. } => None,
    }
}

/// Handle subcommands that do not need a Tokio runtime. Returns `None` for async commands.
pub(super) fn run(cli: &Cli) -> Option<Result<ExitCode>> {
    let command = cli.command.as_ref()?;
    if cli.continue_session && !matches!(command, Command::Chat(_)) {
        return Some(Err(continue_misplaced()));
    }
    if cli.no_session && !matches!(command, Command::Chat(_) | Command::Exec(_)) {
        return Some(Err(hrn_kernel::HrnError::Config(NO_SESSION_MSG.to_owned())));
    }
    match command {
        Command::Tool { argv } => Some(hrn_edge_ipc::tool_shim(argv)),
        Command::Sessions { command } => Some(ok_unit(sessions_command(command.as_ref()))),
        Command::Gc(args) => Some(ok_unit(gc_command(&cli.config, args))),
        Command::Stats => Some(ok_unit(stats_command(&cli.config))),
        Command::Usage {
            command: Some(UsageCommand::Accounts),
        } => Some(ok_unit(auth_location().and_then(|a| usage::accounts(&a)))),
        Command::DryBalance(args) => Some(ok_unit((|| {
            let cfg = load_config(&cli.config).ok().map(|(c, ..)| c);
            dry_balance::run(&auth_location()?, cfg.as_ref(), args)
        })())),
        Command::Agents => Some(ok_unit(
            std::env::current_dir()
                .map_err(Into::into)
                .and_then(|cwd| commands::agents(&StdWorkspaceFs, &cwd)),
        )),
        Command::Skills => Some(ok_unit(
            std::env::current_dir()
                .map_err(Into::into)
                .and_then(|cwd| commands::skills(&StdWorkspaceFs, &cwd)),
        )),
        Command::Commands => Some(ok_unit(
            std::env::current_dir()
                .map_err(Into::into)
                .and_then(|cwd| commands::custom_commands(&StdWorkspaceFs, &cwd)),
        )),
        Command::Auth {
            command: AuthCommand::Status,
        } => Some(ok_unit((|| {
            auth::status(&global_config_path()?, &auth_location()?);
            Ok(())
        })())),
        Command::Logout { provider } => Some(ok_unit((|| {
            auth::logout(
                &global_config_path()?,
                &auth_location()?,
                provider.as_deref(),
            )
        })())),
        Command::Mcp {
            command: McpCommand::List,
        } => Some(ok_unit((|| {
            let (cfg, ..) = load_config(&cli.config)?;
            let cwd = std::env::current_dir()?;
            mcp::list(&cfg, &cwd);
            Ok(())
        })())),
        Command::Remote { command } => dispatch_remote_sync(command),
        Command::Git(args) => Some(git_command(&cli.config, args)),
        Command::Completions { shell } => Some(completions::run(*shell)),
        _ => None,
    }
}
