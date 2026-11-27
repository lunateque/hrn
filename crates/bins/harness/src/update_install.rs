//! Atomic binary installation, Windows `.old` rename dance, and stale backup cleanup.

use crate::update_archive::hrn_ast_exe_name;
use hrn_kernel::{HrnError, Result};
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static STAGE_SEQ: AtomicU64 = AtomicU64::new(1);

/// Atomically replace `target` with `new_bytes` via same-directory temp staging and a
/// rollback-safe `.old` rename dance (handles locked running executables on Windows).
pub(crate) fn replace_binary_atomic(target: &Path, new_bytes: &[u8]) -> Result {
    if new_bytes.is_empty() {
        return Err(HrnError::Tool(
            "refusing to install empty binary".to_owned(),
        ));
    }
    if let Ok(meta) = fs::symlink_metadata(target)
        && meta.file_type().is_symlink()
    {
        return Err(HrnError::Tool(format!(
            "refusing to replace symlink target `{}`",
            target.display()
        )));
    }
    let parent = target.parent().ok_or_else(|| {
        HrnError::Tool(format!(
            "target binary `{}` has no parent directory",
            target.display()
        ))
    })?;
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| HrnError::Tool("invalid target executable filename".to_owned()))?;

    let seq = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let staged = parent.join(format!("{file_name}.tmp.{}.{seq}", std::process::id()));
    let write_res = (|| -> io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        f.write_all(new_bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&staged);
        return Err(HrnError::Io(err.to_string()));
    }
    #[cfg(unix)]
    set_executable_permissions(&staged)?;

    let old_path = old_backup_path(target);
    if let Ok(meta) = fs::symlink_metadata(&old_path) {
        if meta.file_type().is_file() {
            let _ = fs::remove_file(&old_path);
        } else {
            let _ = fs::remove_file(&staged);
            return Err(HrnError::Tool(format!(
                "refusing to overwrite non-regular backup path `{}`",
                old_path.display()
            )));
        }
    }

    let moved_existing = target.exists();
    if moved_existing && let Err(err) = fs::rename(target, &old_path) {
        let _ = fs::remove_file(&staged);
        return Err(HrnError::Tool(format!(
            "failed to move current binary `{}` to `{}`: {err}",
            target.display(),
            old_path.display()
        )));
    }

    if let Err(err) = fs::rename(&staged, target) {
        if moved_existing {
            let _ = fs::rename(&old_path, target);
        }
        let _ = fs::remove_file(&staged);
        return Err(HrnError::Tool(format!(
            "failed to install new binary `{}` (rolled back): {err}",
            target.display()
        )));
    }
    Ok(())
}

/// Remove stale `<exe>.old` and sibling `hrn-ast(.exe).old` left behind by a prior Windows
/// self-update. Never follows symlinks and ignores errors (e.g. if a file is still locked).
pub(crate) fn cleanup_stale_old_binaries() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    cleanup_stale_old_binaries_for(&exe);
}

pub(crate) fn cleanup_stale_old_binaries_for(exe: &Path) {
    let Some(parent) = exe.parent() else {
        return;
    };
    remove_if_regular_file(&old_backup_path(exe));
    let sibling_old = old_backup_path(&parent.join(hrn_ast_exe_name()));
    remove_if_regular_file(&sibling_old);
}

fn remove_if_regular_file(path: &Path) {
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_file()
    {
        let _ = fs::remove_file(path);
    }
}

pub(crate) fn old_backup_path(target: &Path) -> PathBuf {
    let mut raw = target.as_os_str().to_os_string();
    raw.push(".old");
    PathBuf::from(raw)
}

#[cfg(unix)]
fn set_executable_permissions(path: &Path) -> Result {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|e| HrnError::Io(e.to_string()))?;
    Ok(())
}
