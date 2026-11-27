//! [`StdWorkspaceFs`]: the real-filesystem implementation of the kernel's `WorkspaceFs` port.

use hrn_kernel::{HrnError, PathKind, Result, WorkspaceFs};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// `WorkspaceFs` over `std::fs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdWorkspaceFs;

/// "Nothing there": the entry is missing, or an ancestor is a file.
fn is_absent(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

impl WorkspaceFs for StdWorkspaceFs {
    fn read_to_string(&self, path: &Path) -> Result<Option<String>> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if is_absent(&e) => Ok(None),
            Err(e) => Err(HrnError::Io(format!("read `{}`: {e}", path.display()))),
        }
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(e) if is_absent(&e) => return Ok(Vec::new()),
            Err(e) => return Err(HrnError::Io(format!("list `{}`: {e}", path.display()))),
        };
        let mut paths = entries
            .map(|entry| entry.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()
            .map_err(|e| HrnError::Io(format!("list `{}`: {e}", path.display())))?;
        paths.sort();
        Ok(paths)
    }

    fn kind(&self, path: &Path) -> PathKind {
        match fs::metadata(path) {
            Ok(meta) if meta.is_dir() => PathKind::Dir,
            Ok(meta) if meta.is_file() => PathKind::File,
            _ => PathKind::Missing,
        }
    }

    fn home_dir(&self) -> Option<PathBuf> {
        std::env::home_dir().filter(|p| !p.as_os_str().is_empty())
    }

    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        let canon = fs::canonicalize(path).ok()?;
        let s = canon.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return Some(PathBuf::from(format!(r"\\{rest}")));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return Some(PathBuf::from(rest));
        }
        Some(canon)
    }
}
