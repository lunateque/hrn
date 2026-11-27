//! `Ctrl+G`: hand the draft to `$VISUAL` / `$EDITOR` (notepad on Windows when neither is set)
//! and read the result back.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The editor to launch: `$VISUAL`, then `$EDITOR`, then the platform default.
pub(crate) fn command() -> (String, Vec<String>) {
    command_from(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok())
}

/// [`command`] over an explicit environment, so tests need not mutate the real one.
fn command_from(visual: Option<String>, editor: Option<String>) -> (String, Vec<String>) {
    let program = visual
        .or(editor)
        .map(|v| v.trim().to_owned())
        .unwrap_or_default();
    if program.is_empty() {
        return if cfg!(windows) {
            ("notepad".to_owned(), Vec::new())
        } else {
            ("vi".to_owned(), Vec::new())
        };
    }
    // Respect `EDITOR="code -w"`-style arguments.
    let mut parts = program.split_whitespace().map(str::to_owned);
    let exe = parts.next().unwrap_or_default();
    (exe, parts.collect())
}

/// Edit `draft` and return the saved text, or the reason nothing came back.
///
/// # Errors
/// The file could not be written, the editor could not start, or its buffer is unreadable.
pub(crate) fn edit(draft: &str) -> Result<String, String> {
    let path = temp_path();
    std::fs::write(&path, draft).map_err(|e| format!("cannot write `{}`: {e}", path.display()))?;
    open_file(&path)?;
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("cannot read back the draft: {e}"))?;
    let _ = std::fs::remove_file(&path);
    Ok(text)
}

/// Open an existing file at `path` in `$VISUAL` / `$EDITOR`, safely suspending and restoring
/// terminal raw mode around the child process.
///
/// # Errors
/// The editor could not start or exited with a non-zero status.
pub(crate) fn open_file(path: &Path) -> Result<(), String> {
    let (exe, mut args) = command();
    args.push(path.display().to_string());
    let status = crate::term::with_suspended(|| Command::new(&exe).args(&args).status())
        .map_err(|e| format!("cannot start `{exe}`: {e}"))?;
    if !status.success() {
        return Err(format!("`{exe}` exited with {status}"));
    }
    Ok(())
}

fn temp_path() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!("hrn-draft-{stamp}.md"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_wins_and_arguments_are_kept() {
        assert_eq!(
            command_from(Some("myed -w".into()), Some("other".into())),
            ("myed".to_owned(), vec!["-w".to_owned()])
        );
        assert_eq!(
            command_from(None, Some("nano".into())),
            ("nano".to_owned(), Vec::new())
        );
        let (exe, _) = command_from(None, None);
        assert!(!exe.is_empty());
    }
}
