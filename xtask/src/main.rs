//! The repository gate and module manager: `cargo xtask <command>`.
//!
//! `check` reports violations without fixing them; `module new|remove` is the only way a
//! module enters or leaves the tree; `removability` proves every module can leave.

mod check;
mod graph;
mod module;
mod process;
mod promptstats;
mod removability;
mod report;
mod workspace;

use std::process::ExitCode;

/// The error of a whole command, rendered as `error: <message>` on stderr.
pub(crate) type Outcome<T = ()> = Result<T, String>;

const USAGE: &str = "\
cargo xtask check
cargo xtask module new <id>
cargo xtask module remove <id>
cargo xtask removability
cargo xtask promptstats";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome: Outcome<Vec<String>> = match args.as_slice() {
        [one] if one == "check" => check::run(),
        [one] if one == "promptstats" => promptstats::run(),
        [one] if one == "removability" => removability::run().map(|()| Vec::new()),
        [one, verb, id] if one == "module" && verb == "new" => module::new(id).map(|()| Vec::new()),
        [one, verb, id] if one == "module" && verb == "remove" => {
            module::remove(id).map(|()| Vec::new())
        }
        _ => {
            for line in USAGE.lines() {
                report::line(line);
            }
            return ExitCode::from(2);
        }
    };
    match outcome {
        Err(message) => {
            report::line(&format!("error: {message}"));
            ExitCode::FAILURE
        }
        Ok(problems) => {
            for problem in &problems {
                report::line(&format!("xtask check: {problem}"));
            }
            if problems.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}
