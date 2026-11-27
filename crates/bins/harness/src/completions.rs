//! `hrn completions <bash|zsh|fish|powershell>`: emit shell completion scripts via `clap_complete`.

use crate::cli::{Cli, CompletionShell};
use clap::CommandFactory;
use hrn_kernel::Result;
use std::io::Write;
use std::process::ExitCode;

/// Generate completion script for `shell` into `writer`.
pub(crate) fn write_completions(shell: CompletionShell, writer: &mut dyn Write) {
    let mut cmd = Cli::command();
    let generator: clap_complete::Shell = match shell {
        CompletionShell::Bash => clap_complete::Shell::Bash,
        CompletionShell::Zsh => clap_complete::Shell::Zsh,
        CompletionShell::Fish => clap_complete::Shell::Fish,
        CompletionShell::Powershell => clap_complete::Shell::PowerShell,
    };
    clap_complete::generate(generator, &mut cmd, "hrn", writer);
}

/// CLI entrypoint writing the completion script to stdout.
pub(crate) fn run(shell: CompletionShell) -> Result<ExitCode> {
    let mut stdout = std::io::stdout().lock();
    write_completions(shell, &mut stdout);
    stdout.flush()?;
    Ok(ExitCode::SUCCESS)
}
