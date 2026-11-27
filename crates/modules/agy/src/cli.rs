//! `hrn agy status | run`: the module's command line.

use crate::{AgyConfig, AgyRequest, run_agy, status_report};
use hrn_kernel::{CancelFlag, Command, CommandOutcome, HrnError, Result};
use std::path::PathBuf;
use std::sync::Arc;

const USAGE: &str = "usage: hrn agy status\n       hrn agy run [--model M] [--effort E] [--mode MODE] [--cwd DIR] [--resume ID] \
                     [--worktree] [--allow GLOB] [--deny GLOB] [--keep-worktree] [--skip-permissions] [--timeout-secs N] <prompt>\n";

/// The `agy` command descriptor.
pub(crate) fn command() -> Command {
    Command {
        name: "agy",
        about: "Official `agy` CLI subprocess bridge (`status`, `run`)",
        run: Arc::new(|args, cancel| Box::pin(async move { dispatch(args, cancel).await })),
    }
}

async fn dispatch(args: Vec<String>, cancel: CancelFlag) -> Result<CommandOutcome> {
    match args.first().map(String::as_str) {
        Some("status") if args.len() == 1 => Ok(status()),
        Some("run") => run(args.into_iter().skip(1).collect(), cancel).await,
        _ => Ok(usage("")),
    }
}

fn usage(prefix: &str) -> CommandOutcome {
    CommandOutcome {
        stdout: String::new(),
        stderr: format!("{prefix}{USAGE}"),
        code: 2,
    }
}

/// Whether the official `agy` CLI binary is discovered, and the active defaults.
fn status() -> CommandOutcome {
    CommandOutcome {
        stdout: format!("{}\n", status_report()),
        ..CommandOutcome::default()
    }
}

/// Parse the `run` flags and the prompt.
fn parse_run(args: Vec<String>) -> std::result::Result<AgyRequest, String> {
    let mut req = AgyRequest::default();
    let mut cwd = None;
    let mut prompt = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| it.next().ok_or_else(|| format!("{flag} needs a value"));
        match arg.as_str() {
            "--model" => req.model = Some(value("--model")?),
            "--effort" => req.effort = Some(value("--effort")?),
            "--mode" => req.mode = Some(value("--mode")?),
            "--resume" => req.resume = Some(value("--resume")?),
            "--cwd" => cwd = Some(PathBuf::from(value("--cwd")?)),
            "--allow" => req.allowed_paths.push(value("--allow")?),
            "--deny" => req.denied_paths.push(value("--deny")?),
            "--worktree" => req.worktree = true,
            "--keep-worktree" => {
                req.worktree = true;
                req.keep_worktree = true;
            }
            "--timeout-secs" => {
                let raw = value("--timeout-secs")?;
                req.timeout_secs = Some(
                    raw.parse()
                        .map_err(|e| format!("--timeout-secs `{raw}`: {e}"))?,
                );
            }
            "--skip-permissions" => req.skip_permissions = true,
            flag if flag.starts_with("--") => return Err(format!("unknown flag `{flag}`")),
            _ if prompt.is_none() => prompt = Some(arg),
            _ => return Err("only one prompt argument is accepted".to_owned()),
        }
    }
    req.prompt = prompt.ok_or_else(|| "run needs a prompt".to_owned())?;
    req.cwd = match cwd {
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|e| format!("cannot read the current dir: {e}"))?,
    };
    Ok(req)
}

/// Execute one non-interactive prompt via the official `agy` CLI subprocess.
async fn run(args: Vec<String>, cancel: CancelFlag) -> Result<CommandOutcome> {
    let req = match parse_run(args) {
        Ok(req) => req,
        Err(msg) => return Ok(usage(&format!("{msg}\n"))),
    };
    let cfg = AgyConfig::from_env();
    match run_agy(&cfg, &req, &cancel, |_| {}).await {
        Ok(resp) => {
            let mut done = CommandOutcome::default();
            if !resp.output.is_empty() {
                done.stdout.push_str(&resp.output);
                if !resp.output.ends_with('\n') {
                    done.stdout.push('\n');
                }
            }
            if let Some(id) = resp.conversation_id {
                done.stderr.push_str("[agy conversation: ");
                done.stderr.push_str(&id);
                done.stderr.push_str("]\n");
            }
            if !resp.stderr.is_empty() {
                done.stderr.push_str(&resp.stderr);
                done.stderr.push('\n');
            }
            done.code = if resp.timed_out {
                done.stderr.push_str("agy timed out\n");
                1
            } else {
                match resp.exit_code {
                    Some(0) => 0,
                    Some(code) => u8::try_from(code).unwrap_or(1),
                    None => 1,
                }
            };
            Ok(done)
        }
        Err(HrnError::Cancelled) => Ok(CommandOutcome {
            stderr: "interrupted\n".to_owned(),
            code: 130,
            ..CommandOutcome::default()
        }),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn run_flags_and_prompt_parse() {
        let req = parse_run(args(&[
            "--model",
            "m",
            "--skip-permissions",
            "--timeout-secs",
            "9",
            "hi",
        ]))
        .expect("parses");
        assert_eq!(req.model.as_deref(), Some("m"));
        assert!(req.skip_permissions);
        assert_eq!(req.timeout_secs, Some(9));
        assert_eq!(req.prompt, "hi");
    }

    #[test]
    fn bad_run_arguments_are_named() {
        assert!(parse_run(args(&["--nope", "x"])).is_err_and(|e| e.contains("--nope")));
        assert!(parse_run(args(&["--model"])).is_err_and(|e| e.contains("needs a value")));
        assert!(parse_run(args(&[])).is_err_and(|e| e.contains("prompt")));
        assert!(parse_run(args(&["a", "b"])).is_err());
    }
}
