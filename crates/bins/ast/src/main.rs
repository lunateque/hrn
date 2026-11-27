//! `hrn-ast` — the structural-search sidecar of the `ast` module.
//!
//! `hrn` spawns this binary per tool call and speaks one JSON request per line on stdin, one
//! JSON response per line on stdout (see `hrn_edge_ast::protocol`). Keeping every tree-sitter
//! grammar here instead of in `hrn.exe` is what holds the main binary's size (and its Defender
//! startup cost) down; a process start of a few milliseconds per call is noise next to a parse.

mod engine;
mod lang;

#[cfg(test)]
mod tests;

use hrn_edge_ast::protocol::{Data, Request, Response};
use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => handle(request),
            Err(e) => Response::err(format!("malformed request: {e}")),
        };
        if serde_json::to_writer(&mut out, &response).is_err()
            || out.flush().is_err()
            || writeln!(out).is_err()
        {
            break;
        }
    }
}

fn handle(request: Request) -> Response {
    match run(request) {
        Ok(data) => Response::ok(data),
        Err(error) => Response::err(error),
    }
}

fn run(request: Request) -> Result<Data, String> {
    match request {
        Request::Search {
            cwd,
            path,
            lang,
            pattern,
            skip,
            limit,
        } => {
            let cwd = checked_cwd(cwd)?;
            engine::search(&cwd, &path, lang.as_deref(), &pattern, skip, limit).map(Data::Search)
        }
        Request::Rewrite {
            cwd,
            path,
            lang,
            rules,
            max_files,
            write,
        } => {
            let cwd = checked_cwd(cwd)?;
            engine::rewrite(&cwd, &path, lang.as_deref(), &rules, max_files, write)
                .map(Data::Rewrite)
        }
        Request::CheckSyntax {
            path,
            lang,
            content,
        } => engine::check_syntax(&path, lang.as_deref(), &content).map(Data::Syntax),
    }
}

/// The working directory of a request, refusing a missing one with a clear message.
fn checked_cwd(cwd: String) -> Result<std::path::PathBuf, String> {
    let path = std::path::PathBuf::from(cwd);
    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!(
            "working directory `{}` does not exist",
            path.display()
        ))
    }
}
