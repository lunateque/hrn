//! Working-directory facts for the status band: the git branch, read from `.git/HEAD` without
//! spawning `git`; the working-tree counts, from `git status` on a background thread; and a
//! `~`-shortened path.

use crate::port::UiSender;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Working-tree counts from `git status --porcelain`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Status {
    /// Paths with changes in the index.
    pub(crate) staged: usize,
    /// Tracked paths with changes not yet staged.
    pub(crate) unstaged: usize,
    /// Paths git does not track.
    pub(crate) untracked: usize,
}

impl Status {
    pub(crate) fn is_clean(self) -> bool {
        self == Self::default()
    }
}

/// Run [`status`] for `cwd` on a thread of its own and deliver the result on `tx`.
pub(crate) fn probe(cwd: &Path, tx: UiSender) {
    let cwd = cwd.to_path_buf();
    std::thread::spawn(move || {
        let status = status(&cwd);
        tx.send_git(cwd, status);
    });
}

/// The working-tree counts of the repository around `cwd`; `None` outside a repository or
/// without a `git` binary.
fn status(cwd: &Path) -> Option<Status> {
    let out = Command::new("git")
        .args(["status", "--porcelain=v1", "-z"])
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_status(&String::from_utf8_lossy(&out.stdout)))
}

/// Counts `XY path` records: `??` is untracked, a non-blank `X` is staged, a non-blank `Y` is
/// unstaged. Renames and copies carry their source path as an extra record.
fn parse_status(porcelain: &str) -> Status {
    let mut status = Status::default();
    let mut records = porcelain.split('\0');
    while let Some(record) = records.next() {
        let mut flags = record.chars();
        let (Some(x), Some(y)) = (flags.next(), flags.next()) else {
            continue;
        };
        match (x, y) {
            ('?', '?') => status.untracked += 1,
            ('!', '!') => {}
            _ => {
                status.staged += usize::from(x != ' ');
                status.unstaged += usize::from(y != ' ');
                if matches!(x, 'R' | 'C') {
                    records.next();
                }
            }
        }
    }
    status
}

/// Branch name, or the short hash of a detached head.
pub(crate) fn branch(cwd: &Path) -> Option<String> {
    let head = fs::read_to_string(git_dir(cwd)?.join("HEAD")).ok()?;
    parse_head(&head)
}

fn parse_head(head: &str) -> Option<String> {
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(reference) => Some(
            reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference)
                .to_owned(),
        ),
        None => head.get(..7).map(str::to_owned),
    }
}

/// The repository's git directory; worktrees and submodules point to it from a `.git` file.
fn git_dir(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            let target = gitdir_target(&fs::read_to_string(&dot_git).ok()?)?;
            let path = Path::new(&target);
            return Some(if path.is_absolute() {
                path.to_path_buf()
            } else {
                dir.join(path)
            });
        }
    }
    None
}

fn gitdir_target(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))
        .map(|target| target.trim().to_owned())
}

/// `path` with the home directory shown as `~` and `/` separators.
pub(crate) fn display_path(path: &Path) -> String {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    let shown = match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };
    if cfg!(windows) {
        shown.replace('\\', "/")
    } else {
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heads_and_gitdir_files_parse() {
        assert_eq!(
            parse_head("ref: refs/heads/main\n").as_deref(),
            Some("main")
        );
        assert_eq!(parse_head("0123456789abcdef\n").as_deref(), Some("0123456"));
        assert_eq!(
            gitdir_target("gitdir: ../.git/worktrees/x\n").as_deref(),
            Some("../.git/worktrees/x")
        );
    }

    #[test]
    fn porcelain_records_are_counted_by_kind() {
        let porcelain = " M a.rs\0M  b.rs\0MM c.rs\0?? d.rs\0R  new.rs\0old.rs\0!! target\0";
        assert_eq!(
            parse_status(porcelain),
            Status {
                staged: 3,
                unstaged: 2,
                untracked: 1
            }
        );
        assert!(parse_status("").is_clean());
    }

    #[test]
    fn this_repository_has_a_branch() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(branch(here).is_some_and(|b| !b.is_empty()));
    }
}
