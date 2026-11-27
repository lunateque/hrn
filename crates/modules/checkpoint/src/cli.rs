//! `hrn checkpoints list | restore`.

use crate::{Shared, tools::describe};
use hrn_edge_tools::args::blocking;
use hrn_kernel::{CancelFlag, Command, CommandOutcome, Result};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

const USAGE: &str = "usage: hrn checkpoints list [COUNT]\n       hrn checkpoints restore <ID>\n";
/// Snapshots `list` shows unless asked for another count.
const LISTED: usize = 20;
/// Session id characters shown in the listing.
const SESSION_WIDTH: usize = 22;

/// The `checkpoints` command descriptor; it works on `cwd`. Module commands are built with the
/// default settings (a command runs without reading the config file).
pub(crate) fn command(shared: Arc<Shared>, cwd: PathBuf) -> Command {
    Command {
        name: "checkpoints",
        about: "Working-tree snapshots of this directory (`list`, `restore`)",
        run: Arc::new(move |args, _cancel: CancelFlag| {
            let shared = Arc::clone(&shared);
            let cwd = cwd.clone();
            Box::pin(async move { dispatch(shared, cwd, args).await })
        }),
    }
}

fn usage(prefix: &str) -> CommandOutcome {
    CommandOutcome {
        stderr: format!("{prefix}{USAGE}"),
        code: 2,
        ..CommandOutcome::default()
    }
}

async fn dispatch(shared: Arc<Shared>, cwd: PathBuf, args: Vec<String>) -> Result<CommandOutcome> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["list"] => list(shared, cwd, LISTED).await,
        ["list", count] => match count.parse::<usize>() {
            Ok(count) if count > 0 => list(shared, cwd, count).await,
            _ => Ok(usage(&format!("`{count}` is not a count\n"))),
        },
        ["restore", id] => restore(shared, cwd, (*id).to_owned()).await,
        _ => Ok(usage("")),
    }
}

async fn list(shared: Arc<Shared>, cwd: PathBuf, count: usize) -> Result<CommandOutcome> {
    let snapshots = blocking("checkpoints", move || {
        shared.reader(&cwd)?.list(Some(count))
    })
    .await?;
    if snapshots.is_empty() {
        return Ok(CommandOutcome {
            stdout: "no checkpoints for this directory yet\n".to_owned(),
            ..CommandOutcome::default()
        });
    }
    let mut text = format!(
        "{:<12}  {:<19}  {:<8}  {:<SESSION_WIDTH$}  LABEL\n",
        "ID", "WHEN", "KIND", "SESSION"
    );
    for snap in &snapshots {
        let session: String = snap
            .session
            .as_deref()
            .unwrap_or("-")
            .chars()
            .take(SESSION_WIDTH)
            .collect();
        let _ = writeln!(
            text,
            "{:<12}  {:<19}  {:<8}  {session:<SESSION_WIDTH$}  {}",
            snap.id,
            snap.when,
            snap.kind.as_str(),
            snap.label
        );
    }
    Ok(CommandOutcome {
        stdout: text,
        ..CommandOutcome::default()
    })
}

async fn restore(shared: Arc<Shared>, cwd: PathBuf, id: String) -> Result<CommandOutcome> {
    let (target, restored) = blocking("checkpoints", move || {
        let _busy = shared.lock();
        let repo = shared.writer(&cwd)?;
        let target = repo.find(&id)?;
        let restored = repo.restore(&target.id, None, None)?;
        Ok((target, restored))
    })
    .await?;
    Ok(CommandOutcome {
        stdout: format!(
            "restored {} ({}): {}\nundo with: hrn checkpoints restore {}\n",
            target.id,
            target.label,
            describe(&restored.changes),
            restored.undo
        ),
        ..CommandOutcome::default()
    })
}
