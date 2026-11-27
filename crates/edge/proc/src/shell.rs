//! Program/shell resolution: `PATHEXT`-aware lookup and Git Bash discovery on Windows.

use hrn_kernel::Result;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Process-wide shell executable override set from `[shell] shell_path` (below `HRN_SHELL`).
static CONFIGURED_SHELL_PATH: Mutex<Option<String>> = Mutex::new(None);

/// Configure an explicit POSIX shell executable path override (from `[shell] shell_path` /
/// omp `shellPath`).
///
/// Checked after `HRN_SHELL` and before Git Bash / `bash` auto-discovery. Passing `None` or a
/// whitespace-only string clears the override.
pub fn set_shell_path(path: Option<String>) {
    let cleaned = path.and_then(|raw| {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    });
    *CONFIGURED_SHELL_PATH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = cleaned;
}

fn configured_shell_path() -> Option<String> {
    CONFIGURED_SHELL_PATH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The POSIX shell used by the `bash` tool.
#[derive(Clone, Debug)]
pub struct Shell {
    /// Executable path or name.
    pub program: String,
    /// Flags that go before `-c <command>` for a one-shot run, and alone for a persistent shell
    /// reading commands from stdin.
    pub args: Vec<String>,
}

impl Shell {
    /// Arguments of a one-shot run of `command`.
    pub fn command_args(&self, command: &str) -> Vec<String> {
        let mut args = self.args.clone();
        args.extend(["-c".to_owned(), command.to_owned()]);
        args
    }
}

/// Resolve the POSIX shell used by the `bash` tool.
///
/// Precedence:
/// 1. `HRN_SHELL` environment variable (if set and non-empty).
/// 2. Explicit configured path from [`set_shell_path`] (`[shell] shell_path` / omp `shellPath`).
/// 3. Auto-discovered Git Bash on Windows (never the WSL launcher in `System32`); on Unix `bash`
///    (`/bin/bash`, `/usr/bin/bash`, then `PATH`), falling back to `/bin/sh` only when there is
///    none. The session protocol and the tool-command rc file use bash builtins (`builtin`,
///    `read -d`, `type -P`), so `dash` as `/bin/sh` (Debian, Ubuntu) cannot run them.
///
/// There is deliberately no PowerShell fallback: the tool contract is POSIX shell syntax.
///
/// Git Bash runs with `--noprofile --norc`, not as a login shell: its login profile costs ~300 ms
/// of subprocesses per start, while the `bin\bash.exe` launcher already sets `MSYSTEM` and a
/// `PATH` with the MSYS and MinGW tools ahead of the inherited one.
pub fn resolve_shell() -> Result<Shell> {
    resolve_shell_with(std::env::var("HRN_SHELL").ok(), configured_shell_path())
}

#[cfg_attr(
    not(windows),
    expect(
        clippy::unnecessary_wraps,
        reason = "only the Windows Git Bash lookup can fail"
    )
)]
fn resolve_shell_with(env_shell: Option<String>, configured: Option<String>) -> Result<Shell> {
    if let Some(custom) = env_shell.filter(|s| !s.trim().is_empty()) {
        return Ok(Shell {
            program: custom,
            args: Vec::new(),
        });
    }
    if let Some(custom) = configured.filter(|s| !s.trim().is_empty()) {
        return Ok(Shell {
            program: custom,
            args: Vec::new(),
        });
    }
    #[cfg(windows)]
    {
        find_git_bash()
            .map(|p| Shell {
                program: p.to_string_lossy().into_owned(),
                args: vec!["--noprofile".to_owned(), "--norc".to_owned()],
            })
            .ok_or_else(|| {
                hrn_kernel::HrnError::Config(
                    "Git Bash not found: install Git for Windows or set HRN_SHELL / GIT_BASH to bash.exe"
                        .to_owned(),
                )
            })
    }
    #[cfg(not(windows))]
    {
        Ok(Shell {
            program: find_unix_bash().unwrap_or_else(|| "/bin/sh".to_owned()),
            args: Vec::new(),
        })
    }
}

#[cfg(not(windows))]
fn find_unix_bash() -> Option<String> {
    ["/bin/bash", "/usr/bin/bash"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .or_else(|| resolve_on_path("bash"))
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn find_git_bash() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(custom) = std::env::var_os("GIT_BASH") {
        candidates.push(PathBuf::from(custom));
    }
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(var) {
            let git = Path::new(&root).join("Git");
            candidates.push(git.join("bin").join("bash.exe"));
            candidates.push(git.join("usr").join("bin").join("bash.exe"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let git = Path::new(&local).join("Programs").join("Git");
        candidates.push(git.join("bin").join("bash.exe"));
    }
    // `<git root>\cmd\git.exe` -> `<git root>\bin\bash.exe` covers custom install locations.
    if let Some(git_exe) = resolve_on_path("git")
        && let Some(root) = git_exe.parent().and_then(Path::parent)
    {
        candidates.push(root.join("bin").join("bash.exe"));
    }
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }
    resolve_on_path("bash").filter(|p| {
        let lower = p.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
        // `System32\bash.exe` is the WSL launcher and `WindowsApps` holds app-execution aliases:
        // neither runs the MSYS userland the tool expects.
        !(lower.contains("\\windows\\system32\\") || lower.contains("\\windowsapps\\"))
    })
}

/// Resolve `program` to the concrete file to execute.
///
/// Windows: relative names containing a separator are joined to `cwd`; bare names are searched on
/// `PATH`, trying every `PATHEXT` extension *before* the bare name (the extension-less `npx` next
/// to `npx.cmd` is a shell script `CreateProcess` cannot run). `.cmd`/`.bat` targets are returned
/// as-is: Rust's `Command` applies the cmd.exe-safe argument escaping for them itself
/// (`BatBadBut`, CVE-2024-24576), which a hand-built `cmd /c` wrapper would bypass.
/// Other platforms: unchanged.
pub fn resolve_program(program: &str, cwd: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let p = Path::new(program);
        if p.is_absolute() || program.contains(['/', '\\']) {
            let full = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            };
            return with_known_extension(&full).unwrap_or(full);
        }
        resolve_on_path(program).unwrap_or_else(|| PathBuf::from(program))
    }
    #[cfg(not(windows))]
    {
        let _ = cwd;
        PathBuf::from(program)
    }
}

#[cfg(windows)]
fn pathext() -> Vec<String> {
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
        .split(';')
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// `base` itself when it already names a file with a `PATHEXT` extension, otherwise `base` with
/// the first extension that exists.
#[cfg(windows)]
fn with_known_extension(base: &Path) -> Option<PathBuf> {
    let exts = pathext();
    let has_ext = base
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.contains(&format!(".{}", e.to_ascii_lowercase())));
    if has_ext && base.is_file() {
        return Some(base.to_path_buf());
    }
    let name = base.file_name()?.to_string_lossy().into_owned();
    exts.iter()
        .map(|ext| base.with_file_name(format!("{name}{ext}")))
        .find(|candidate| candidate.is_file())
}

/// Resolve an executable name along `PATH` (honouring `PATHEXT` on Windows).
pub fn resolve_on_path(program: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        #[cfg(windows)]
        {
            if let Some(found) = with_known_extension(&dir.join(program)) {
                return Some(found);
            }
        }
        #[cfg(not(windows))]
        {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_shell_precedence_env_over_configured_over_default() {
        let env_wins =
            resolve_shell_with(Some("/env/bash".to_owned()), Some("/cfg/bash".to_owned()))
                .expect("env wins");
        assert_eq!(env_wins.program, "/env/bash");
        assert!(env_wins.args.is_empty());

        let cfg_wins = resolve_shell_with(Some("   ".to_owned()), Some("/cfg/bash".to_owned()))
            .expect("configured wins when env blank");
        assert_eq!(cfg_wins.program, "/cfg/bash");
        assert!(cfg_wins.args.is_empty());

        let cfg_only =
            resolve_shell_with(None, Some("/opt/bin/bash".to_owned())).expect("configured");
        assert_eq!(cfg_only.program, "/opt/bin/bash");
    }

    #[test]
    fn set_shell_path_trims_and_clears_blank() {
        let real = resolve_shell_with(None, None)
            .expect("default shell")
            .program;
        set_shell_path(Some(format!("  {real}  ")));
        assert_eq!(configured_shell_path().as_deref(), Some(real.as_str()));
        set_shell_path(Some("   ".to_owned()));
        assert_eq!(configured_shell_path(), None);
        set_shell_path(None);
        assert_eq!(configured_shell_path(), None);
    }
}
