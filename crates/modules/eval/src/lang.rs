//! The two supported kernel languages and how to launch their interpreters.

use hrn_edge_proc::resolve_on_path;
use hrn_kernel::{HrnError, Result};
use std::path::Path;

/// A kernel language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    /// Python running `runner/kernel.py`.
    Py,
    /// JavaScript (Bun) running `runner/kernel.js`.
    Js,
}

impl Lang {
    /// Parse a language token (`"py"` or `"js"`).
    pub(crate) fn parse(token: &str) -> Option<Self> {
        match token {
            "py" => Some(Self::Py),
            "js" => Some(Self::Js),
            _ => None,
        }
    }

    /// Canonical short token (`"py"` or `"js"`).
    pub(crate) const fn token(self) -> &'static str {
        match self {
            Self::Py => "py",
            Self::Js => "js",
        }
    }

    /// File name of the embedded runner inside the data dir.
    pub(crate) const fn script_name(self) -> &'static str {
        match self {
            Self::Py => "kernel.py",
            Self::Js => "kernel.js",
        }
    }

    /// The embedded runner source.
    pub(crate) const fn script_source(self) -> &'static str {
        match self {
            Self::Py => include_str!("../runner/kernel.py"),
            Self::Js => include_str!("../runner/kernel.js"),
        }
    }

    /// File name of the embedded prelude inside the data dir.
    pub(crate) const fn prelude_name(self) -> &'static str {
        match self {
            Self::Py => "prelude.py",
            Self::Js => "prelude.js",
        }
    }

    /// The embedded prelude source.
    pub(crate) const fn prelude_source(self) -> &'static str {
        match self {
            Self::Py => include_str!("../runner/prelude.py"),
            Self::Js => include_str!("../runner/prelude.js"),
        }
    }

    /// Resolve the Python executable path on `PATH`.
    pub(crate) fn python_executable() -> Result<String> {
        ["python", "python3"]
            .into_iter()
            .find_map(resolve_on_path)
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or_else(|| {
                HrnError::Tool("eval: `python` not found on PATH (need Python 3.9+)".into())
            })
    }

    /// Program and arguments that run `script` under this language's interpreter.
    pub(crate) fn command(self, script: &Path) -> Result<(String, Vec<String>)> {
        let script = script.to_string_lossy().into_owned();
        match self {
            Self::Py => {
                let python = Self::python_executable()?;
                Ok((python, vec!["-X".to_owned(), "utf8".to_owned(), script]))
            }
            Self::Js => {
                let bun = resolve_on_path("bun")
                    .ok_or_else(|| HrnError::Tool("eval: `bun` not found on PATH".into()))?;
                Ok((bare_bun_exe(&bun), vec![script]))
            }
        }
    }
}

/// npm installs `bun` as a `.cmd` shim that re-launches the real executable through `cmd.exe`;
/// run that executable directly when it sits next to the shim.
fn bare_bun_exe(found: &Path) -> String {
    let direct = found
        .parent()
        .map(|dir| {
            dir.join("node_modules")
                .join("bun")
                .join("bin")
                .join("bun.exe")
        })
        .filter(|exe| exe.is_file());
    direct
        .as_deref()
        .unwrap_or(found)
        .to_string_lossy()
        .into_owned()
}
