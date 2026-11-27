//! Which project a working directory belongs to: the key under which project memories live.

use std::fs;
use std::path::{Path, PathBuf};

/// The scope name of memories every project recalls.
pub(crate) const GLOBAL: &str = "global";

/// The scope key of the project containing `cwd`: its primary checkout root, so every linked
/// worktree of one repository shares memories. Outside a repository the key is `cwd` itself.
pub(crate) fn project_key(cwd: &Path) -> String {
    let root = repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let text = root.to_string_lossy().replace('\\', "/");
    let text = text
        .strip_prefix("//?/")
        .unwrap_or(&text)
        .trim_end_matches('/');
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.to_owned()
    }
}

/// The primary checkout root of the repository containing `cwd`.
fn repo_root(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dir.to_path_buf());
        }
        if dot_git.is_file() {
            return Some(primary_of_worktree(dir, &dot_git).unwrap_or_else(|| dir.to_path_buf()));
        }
    }
    None
}

/// `dir/.git` is a pointer file in a linked worktree: `gitdir: <primary>/.git/worktrees/<name>`,
/// and that directory's `commondir` names the primary `.git` directory.
fn primary_of_worktree(dir: &Path, pointer: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(pointer).ok()?;
    let gitdir = Path::new(text.trim().strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir.to_path_buf()
    } else {
        dir.join(gitdir)
    };
    let common = fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = gitdir.join(common.trim());
    // `<primary>/.git/worktrees/<name>/../..` still names `.git`; take its parent.
    let common = common.canonicalize().ok()?;
    common.parent().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linked_worktree_shares_the_key_of_its_primary_checkout() {
        let tmp = std::env::temp_dir().join(format!("hrn-mem-proj-{}", std::process::id()));
        fs::create_dir_all(&tmp).expect("tmp");
        // Canonical, so 8.3 short names in the temp path cannot differ from the resolved root.
        let tmp = tmp.canonicalize().expect("canonical tmp");
        let primary = tmp.join("primary");
        let wt = tmp.join("wt");
        let gitdir = primary.join(".git").join("worktrees").join("wt");
        fs::create_dir_all(&gitdir).expect("gitdir");
        fs::create_dir_all(wt.join("src")).expect("wt");
        fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).expect("pointer");
        fs::write(gitdir.join("commondir"), "../..\n").expect("commondir");
        let key = project_key(&wt.join("src"));
        assert_eq!(key, project_key(&primary));
        assert_ne!(key, project_key(&tmp));
        fs::remove_dir_all(&tmp).expect("cleanup");
    }
}
