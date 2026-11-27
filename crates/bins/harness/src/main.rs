//! `hrn`: the single binary of the harness. Wires the edge crates into the core runtime and
//! exposes the interactive chat (the default), the headless `exec`, `sessions`, and `config`.

mod auth;
mod auth_agy;
mod auth_input;
mod chat;
mod chat_info;
mod cli;
mod collab;
mod commands;
mod commit;
mod completions;
mod config;
mod config_cmd;
mod config_layers;
mod config_watch;
mod daemon;
mod dispatch_sync;
mod doctor;
mod dry_balance;
mod eval_host;
mod exec;
mod export;
mod external;
mod gc;
mod git_cmd;
mod idle_compact;
mod import_cmd;
mod logout;
mod main_dispatch;
mod mcp;
mod mcp_urls;
mod models;
mod modules;
mod omp_config;
mod omp_import;
mod paths;
mod probe;
mod promptstats;
mod remote;
mod remote_web;
mod resume;
mod roles;
mod rpc;
mod session_lock;
mod settings_schema;
mod setup;
mod setup_wizard;
mod share;
mod stats;
mod task_isolation;
mod term;
mod toml_write;
mod update;
mod update_archive;
mod url_adapters;
mod usage;
mod wiring;
mod worktree;

#[cfg(test)]
mod compaction_native_tests;
#[cfg(test)]
mod share_collab_tests;

use clap::Parser;
use cli::{AuthCommand, ChatArgs, Cli, Command, ServeArgs, ServeMode, UsageCommand};
use config::Config;
use config_layers::{LayerInput, MergedConfig};
use hrn_edge_tools::StdWorkspaceFs;
use hrn_kernel::{Result, WorkspaceFs};
use main_dispatch::{
    commit_command, config_subcommand, dispatch_remote, dry_balance_command, gc_command,
    git_command, mcp_command, models_command, providers_command, sessions_command, stats_command,
    update_command, worktree_command,
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn global_config_path() -> Result<PathBuf> {
    let profile = paths::active_profile();
    paths::global_config_path(
        env_string("HRN_CONFIG"),
        StdWorkspaceFs.home_dir().as_deref(),
        profile.as_deref(),
    )
}

fn active_agent_dir() -> Result<PathBuf> {
    let profile = paths::active_profile();
    paths::agent_dir(StdWorkspaceFs.home_dir().as_deref(), profile.as_deref())
}

fn layer_input(overlays: &[PathBuf], cwd: &Path) -> Result<LayerInput> {
    let home = StdWorkspaceFs.home_dir();
    let global_path = global_config_path()?;
    let project_path = paths::project_config_path(cwd);
    Ok(LayerInput {
        global_path: Some(global_path),
        project_path: Some(project_path),
        overlays: overlays.to_vec(),
        cwd: cwd.to_path_buf(),
        home,
        env: std::env::vars().collect(),
        runtime: toml::Table::new(),
    })
}

fn load_config(overlays: &[PathBuf]) -> Result<(Config, LayerInput, MergedConfig, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let input = layer_input(overlays, &cwd)?;
    let global = global_config_path()?;
    let no_files =
        !global.is_file() && overlays.is_empty() && !paths::project_config_path(&cwd).is_file();
    match Config::load_layered(&input) {
        Ok((cfg, merged)) => Ok((cfg, input, merged, global)),
        Err(err) if no_files => Err(hrn_kernel::HrnError::Config(format!(
            "no config at `{}` · run `hrn login` to configure a provider ({err})",
            global.display()
        ))),
        Err(err) => Err(err),
    }
}

fn app_data_dir() -> Result<PathBuf> {
    let profile = paths::active_profile();
    paths::data_dir_for_profile(
        env_string("LOCALAPPDATA"),
        env_string("XDG_DATA_HOME"),
        StdWorkspaceFs.home_dir().as_deref(),
        profile.as_deref(),
    )
}

fn keybindings_paths() -> Vec<PathBuf> {
    let profile = paths::active_profile();
    paths::keybindings_paths(
        env_string("LOCALAPPDATA"),
        env_string("XDG_DATA_HOME"),
        StdWorkspaceFs.home_dir().as_deref(),
        profile.as_deref(),
    )
    .unwrap_or_default()
}

fn auth_location() -> Result<PathBuf> {
    paths::auth_dir(StdWorkspaceFs.home_dir().as_deref())
}

fn database_location() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("sessions.db"))
}

fn run_file_location() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("run.json"))
}

fn host_key_location() -> Result<PathBuf> {
    paths::host_key_path(StdWorkspaceFs.home_dir().as_deref())
}

fn remote_hosts_location() -> Result<PathBuf> {
    paths::remote_hosts_path(StdWorkspaceFs.home_dir().as_deref())
}

fn continue_misplaced() -> hrn_kernel::HrnError {
    hrn_kernel::HrnError::Config("--continue resumes the chat; it has no meaning here".to_owned())
}

/// The names `hrn stats` shows: the config's models when the config file can be read (a
/// broken or absent config must not hide the numbers that are stored).
fn stats_names(overlays: &[PathBuf]) -> stats::ModelNames {
    load_config(overlays)
        .ok()
        .map_or_else(stats::ModelNames::default, |(cfg, ..)| {
            stats::names(&cfg.llm)
        })
}

/// The complaint about `--no-session` where it cannot apply.
const NO_SESSION_MSG: &str =
    "--no-session is a chat and exec option; this command reads the store anyway";

/// `--no-session` is a chat and exec option; it has no meaning for a command that only reads
/// the store.
fn apply_no_session(command: Option<Command>, no_session: bool) -> Result<Command> {
    let mut command = command.unwrap_or_else(|| Command::Chat(ChatArgs::default()));
    if no_session {
        if !matches!(command, Command::Chat(_) | Command::Exec(_)) {
            return Err(hrn_kernel::HrnError::Config(NO_SESSION_MSG.to_owned()));
        }
        match &mut command {
            Command::Chat(args) => {
                if args.restore.is_some()
                    || args.from_claude.is_some()
                    || args.from_codex.is_some()
                    || args.from_omp.is_some()
                {
                    return Err(hrn_kernel::HrnError::Config(
                        "--restore, --from-claude, --from-codex, and --from-omp cannot be combined with --no-session".to_owned(),
                    ));
                }
                args.no_session = true;
            }
            Command::Exec(args) => {
                if args.resume.is_some() || args.db.is_some() {
                    return Err(hrn_kernel::HrnError::Config(
                        "--resume and --db cannot be combined with --no-session".to_owned(),
                    ));
                }
                args.no_session = true;
            }
            _ => {}
        }
    }
    Ok(command)
}

/// `--mode rpc|rpc-ui|acp`: the same servers the `rpc`/`rpc-ui`/`acp` subcommands run.
async fn serve_mode(
    mode: ServeMode,
    overlays: &[PathBuf],
    approval_override: Option<hrn_kernel::PermissionMode>,
    args: ServeArgs,
) -> Result<ExitCode> {
    let (mut cfg, mut input, merged, global, discovered) =
        wiring::load_config_with_discovery(overlays).await?;
    cfg.apply_output_default(&merged, false);
    if let Some(m) = approval_override {
        cfg.approval_mode = m;
    }
    crate::config::apply_prompt_flags(&mut cfg, &args);
    crate::config::apply_no_lsp(&mut cfg, Some(&mut input), args.no_lsp);
    match mode {
        ServeMode::Rpc => rpc::serve(&cfg, input, merged, global, args, false, discovered).await,
        ServeMode::RpcUi => rpc::serve(&cfg, input, merged, global, args, true, discovered).await,
        ServeMode::Acp => rpc::serve_acp(&cfg, input, merged, global, args, discovered).await,
    }
}

/// The interactive chat: load the layered config, apply launch source flags, start the TUI.
async fn chat_command(
    overlays: &[PathBuf],
    approval_override: Option<hrn_kernel::PermissionMode>,
    args: ChatArgs,
) -> Result<ExitCode> {
    let (mut cfg, mut input, merged, global, discovered) =
        wiring::load_config_with_discovery(overlays).await?;
    cfg.apply_output_default(&merged, true);
    if let Some(m) = approval_override {
        cfg.approval_mode = m;
    }
    config::apply_source_flags(&mut cfg, &args.sources);
    config::apply_no_lsp(&mut cfg, Some(&mut input), args.sources.no_lsp);
    chat::run(
        &cfg,
        input,
        merged,
        global,
        &database_location()?,
        args,
        discovered,
    )
    .await
}

/// Run the headless `exec` subcommand with frontend defaults (`out_enabled = false`) and launch flags.
async fn exec_command(
    overlays: &[PathBuf],
    approval_override: Option<hrn_kernel::PermissionMode>,
    args: cli::ExecArgs,
) -> Result<ExitCode> {
    let (mut cfg, _, merged, _, discovered) = wiring::load_config_with_discovery(overlays).await?;
    cfg.apply_output_default(&merged, false);
    if let Some(m) = approval_override {
        cfg.approval_mode = m;
    }
    config::apply_source_flags(&mut cfg, &args.sources);
    config::apply_no_lsp(&mut cfg, None, args.sources.no_lsp);
    exec::run(&cfg, &database_location()?, args, discovered).await
}

/// Start the local daemon with frontend defaults (`out_enabled = false`).
async fn daemon_command(overlays: &[PathBuf], listen: &str) -> Result {
    let (mut cfg, _, merged, _, _) = wiring::load_config_with_discovery(overlays).await?;
    cfg.apply_output_default(&merged, false);
    daemon::run_daemon(
        &cfg,
        &database_location()?,
        &run_file_location()?,
        &host_key_location()?,
        listen,
    )
    .await
}

fn apply_continue_flag(command: Command, continue_session: bool) -> Result<Command> {
    match command {
        Command::Chat(mut args) => {
            if continue_session
                && (args.resume.is_some()
                    || args.restore.is_some()
                    || args.from_claude.is_some()
                    || args.from_codex.is_some()
                    || args.from_omp.is_some())
            {
                return Err(hrn_kernel::HrnError::Config(
                    "--continue cannot be combined with --resume, --restore, --from-claude, --from-codex, or --from-omp"
                        .to_owned(),
                ));
            }
            args.continue_session |= continue_session;
            Ok(Command::Chat(args))
        }
        _ if continue_session => Err(continue_misplaced()),
        other => Ok(other),
    }
}

async fn dispatch(cli: Cli) -> Result<ExitCode> {
    let approval_override = cli.approval_mode_override();
    if let Some(mode) = cli.mode {
        return serve_mode(mode, &cli.config, approval_override, cli.serve).await;
    }
    let command = apply_no_session(cli.command, cli.no_session)?;
    let command = apply_continue_flag(command, cli.continue_session)?;
    match command {
        Command::Chat(args) => {
            return Box::pin(chat_command(&cli.config, approval_override, args)).await;
        }
        // dispatch_sync took it; reaching here means the CLI changed shape.
        Command::Tool { .. } => unreachable!("`hrn tool` is answered synchronously"),
        Command::Sessions { command } => sessions_command(command.as_ref())?,
        Command::Gc(args) => gc_command(&cli.config, &args)?,
        Command::Stats => stats_command(&cli.config)?,
        Command::Models => models_command(&cli.config).await?,
        Command::Providers { command } => providers_command(&command)?,
        Command::Usage { command } => match command {
            None => usage::run(&auth_location()?).await?,
            Some(UsageCommand::Accounts) => usage::accounts(&auth_location()?)?,
        },
        Command::DryBalance(args) => dry_balance_command(&cli.config, &args)?,
        Command::Agents => commands::agents(&StdWorkspaceFs, &std::env::current_dir()?)?,
        Command::Skills => commands::skills(&StdWorkspaceFs, &std::env::current_dir()?)?,
        Command::Commands => {
            commands::custom_commands(&StdWorkspaceFs, &std::env::current_dir()?)?;
        }
        Command::Login(args) => {
            let cfg_path = global_config_path()?;
            auth::login(&cfg_path, &auth_location()?, &args).await?;
        }
        Command::Logout { provider } => {
            auth::logout(
                &global_config_path()?,
                &auth_location()?,
                provider.as_deref(),
            )?;
        }
        Command::Setup => {
            setup_wizard::run_cli(&global_config_path()?, &auth_location()?).await?;
        }
        Command::Auth {
            command: AuthCommand::Status,
        } => auth::status(&global_config_path()?, &auth_location()?),
        Command::Doctor => doctor::run(&cli.config).await?,
        Command::Config { command } => config_subcommand(&cli.config, command.as_ref()).await?,
        Command::Mcp { command } => mcp_command(&cli.config, command).await?,
        Command::PromptStats { cwd, fixture } => {
            let dir = cwd
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let input = layer_input(&cli.config, &dir)?;
            promptstats::command(&input, cwd, fixture)?;
        }
        Command::Rpc(args) => {
            return serve_mode(ServeMode::Rpc, &cli.config, approval_override, args).await;
        }
        Command::RpcUi(args) => {
            return serve_mode(ServeMode::RpcUi, &cli.config, approval_override, args).await;
        }
        Command::Acp(args) => {
            return serve_mode(ServeMode::Acp, &cli.config, approval_override, args).await;
        }
        Command::External(args) => return external::run(args).await,
        Command::Daemon { listen } => daemon_command(&cli.config, &listen).await?,
        Command::Attach { target } => {
            daemon::attach(&target, &run_file_location()?, &remote_hosts_location()?).await?;
        }
        Command::Share(args) => dispatch_share(&cli.config, &args).await?,
        Command::Join(args) => dispatch_join(&cli.config, &args).await?,
        Command::Worktree { command } => worktree_command(command).await?,
        Command::Remote { command } => dispatch_remote(&cli.config, command).await?,
        Command::Commit(args) => return commit_command(&cli.config, &args).await,
        Command::Git(args) => return git_command(&cli.config, &args),
        Command::Update(args) => return update_command(&cli.config, &args).await,
        Command::Completions { shell } => return completions::run(shell),
        Command::Exec(args) => return exec_command(&cli.config, approval_override, args).await,
    }
    Ok(ExitCode::SUCCESS)
}

async fn dispatch_share(config_overlays: &[PathBuf], args: &cli::ShareArgs) -> Result<()> {
    let (cfg, ..) = load_config(config_overlays)?;
    let store = wiring::open_store(&database_location()?, false)?;
    share::run_cli(store.as_ref(), &cfg, &std::env::current_dir()?, args).await
}

async fn dispatch_join(config_overlays: &[PathBuf], args: &cli::JoinArgs) -> Result<()> {
    let (cfg, ..) = load_config(config_overlays)?;
    let store = wiring::open_store(&database_location()?, false)?;
    collab::run_join_cli(&cfg, store, &std::env::current_dir()?, args).await
}

fn run_async(cli: Cli) -> Result<ExitCode> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| hrn_kernel::HrnError::Internal(format!("failed to start runtime: {err}")))?;
    rt.block_on(dispatch(cli))
}

fn main() -> ExitCode {
    update::cleanup_stale_old_binaries();
    // Every descendant dies with hrn, however it ends (see `hrn_edge_proc::guard_process_tree`).
    #[cfg(windows)]
    if let Err(err) = hrn_edge_proc::guard_process_tree() {
        term::err_line(&format!("warning: descendants may outlive hrn: {err}"));
    }
    let cli = Cli::parse();
    if let Err(err) =
        paths::init_active_profile(cli.profile.as_deref(), env_string("HRN_PROFILE").as_deref())
    {
        term::err_line(&format!("error: {err}"));
        return ExitCode::FAILURE;
    }
    let result = dispatch_sync::run(&cli).unwrap_or_else(|| run_async(cli));
    match result {
        Ok(code) => code,
        Err(err) => {
            term::err_line(&format!("error: {err}"));
            ExitCode::FAILURE
        }
    }
}
