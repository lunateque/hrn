//! `hrn <word> ...` for a word no built-in command owns: the commands installed modules register.

use crate::modules;
use crate::term::{err_line, out};
use hrn_kernel::{CancelFlag, Host, HrnError, Registry, Result, Shared};
use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::Arc;

/// Run the module command named by `args[0]` with the remaining arguments.
pub(crate) async fn run(args: Vec<String>) -> Result<ExitCode> {
    let host = Host {
        cwd: std::env::current_dir()?,
        data_dir: crate::app_data_dir()?,
    };
    // No config file is read: module commands run without one, so modules register with defaults.
    let registry = Registry::new(host, BTreeMap::new(), Arc::new(Shared::new()));
    let commands = modules::registry(registry)?.finish()?.commands;
    let mut words = args.into_iter();
    let word = words.next().unwrap_or_default();
    let Some(command) = commands.iter().find(|c| c.name == word) else {
        let known: Vec<String> = commands
            .iter()
            .map(|c| format!("{} ({})", c.name, c.about))
            .collect();
        return Err(HrnError::Config(format!(
            "unknown command `{word}`; module commands: {}",
            if known.is_empty() {
                "none installed".to_owned()
            } else {
                known.join(", ")
            }
        )));
    };
    let cancel = CancelFlag::new();
    let on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });
    let outcome = (command.run)(words.collect(), cancel).await?;
    out(&outcome.stdout);
    if !outcome.stderr.is_empty() {
        err_line(outcome.stderr.trim_end_matches('\n'));
    }
    Ok(ExitCode::from(outcome.code))
}
