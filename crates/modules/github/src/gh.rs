//! The `gh` and `git` process wrappers: non-interactive environment, one deadline, bounded
//! output, and model-facing messages for the common failures.

use hrn_edge_proc::{RunOutput, RunSpec, resolve_on_path, run_streaming};
use hrn_edge_rt::tokio;
use hrn_kernel::serde_json::{self, Value};
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hard wall-clock ceiling for one `gh` / `git` run.
const TIMEOUT: Duration = Duration::from_mins(5);
/// Output kept in memory per run.
const MAX_OUTPUT: usize = 8 << 20;

/// A resolved `gh` executable.
#[derive(Clone)]
pub(crate) struct Gh {
    binary: PathBuf,
}

impl Gh {
    /// `gh` from `PATH`, or `None` when it is not installed.
    pub(crate) fn discover() -> Option<Self> {
        resolve_on_path("gh").map(|binary| Self { binary })
    }

    /// Run `gh args…` and return its raw outcome.
    pub(crate) async fn exec<I>(&self, cwd: &Path, args: I, cancel: &CancelFlag) -> Result<Outcome>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let env = [
            ("GH_PROMPT_DISABLED", "1"),
            ("GH_PAGER", "cat"),
            ("GH_NO_UPDATE_NOTIFIER", "1"),
            ("GH_SPINNER_DISABLED", "1"),
            ("NO_COLOR", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let program = self.binary.to_string_lossy().into_owned();
        run(program, args, cwd, env, cancel).await
    }

    /// Run `gh args…` and return trimmed stdout; a failed run becomes a tool error.
    pub(crate) async fn text<I>(&self, cwd: &Path, args: I, cancel: &CancelFlag) -> Result<String>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let out = self.exec(cwd, &args, cancel).await?;
        out.success(&args, "gh")
    }

    /// Like [`Gh::text`], parsed as JSON.
    pub(crate) async fn json<I>(&self, cwd: &Path, args: I, cancel: &CancelFlag) -> Result<Value>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let text = self.text(cwd, args, cancel).await?;
        serde_json::from_str(&text)
            .map_err(|e| HrnError::Tool(format!("gh returned invalid JSON: {e}")))
    }
}

/// A finished `gh` / `git` run, exit status not yet judged.
pub(crate) struct Outcome {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Outcome {
    /// Trimmed stdout of a run that exited 0.
    pub(crate) fn success(self, args: &[String], tool: &str) -> Result<String> {
        if self.code == Some(0) {
            return Ok(self.stdout.trim().to_owned());
        }
        Err(self.failure(args, tool))
    }

    /// The model-facing error for a failed run.
    pub(crate) fn failure(&self, args: &[String], tool: &str) -> HrnError {
        let text = [self.stderr.trim(), self.stdout.trim()]
            .into_iter()
            .find(|s| !s.is_empty());
        let lower = text.unwrap_or_default().to_lowercase();
        let message = if tool == "gh" && lower.contains("gh auth login") {
            "GitHub CLI is not authenticated. Run `gh auth login`.".to_owned()
        } else if tool == "gh"
            && (lower.contains("not a git repository")
                || lower.contains("no git remotes")
                || lower.contains("none of the git remotes")
                || lower.contains("unable to determine base repo"))
        {
            "GitHub repository context is unavailable. Pass `repo` explicitly or run the tool \
             inside a GitHub checkout."
                .to_owned()
        } else if let Some(text) = text {
            text.to_owned()
        } else {
            let head: Vec<&str> = args.iter().take(3).map(String::as_str).collect();
            format!("{tool} command failed: {tool} {}", head.join(" "))
        };
        HrnError::Tool(message)
    }
}

/// The bare message of an error, without the `tool error:` prefix, for embedding in another one.
pub(crate) fn reason(err: &HrnError) -> String {
    match err {
        HrnError::Tool(message) => message.clone(),
        other => other.to_string(),
    }
}

/// Run `git args…` in `cwd` and return trimmed stdout.
pub(crate) async fn git<I>(cwd: &Path, args: I, cancel: &CancelFlag) -> Result<String>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let env = BTreeMap::from([("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned())]);
    let out = run("git".to_owned(), args.clone(), cwd, env, cancel).await?;
    out.success(&args, "git")
}

async fn run(
    program: String,
    args: Vec<String>,
    cwd: &Path,
    env: BTreeMap<String, String>,
    cancel: &CancelFlag,
) -> Result<Outcome> {
    let spec = RunSpec {
        program,
        args,
        cwd: cwd.to_path_buf(),
        env,
        timeout: TIMEOUT,
        max_output_bytes: MAX_OUTPUT,
    };
    let RunOutput {
        exit_code,
        stdout,
        stderr,
        timed_out,
        ..
    } = run_streaming(&spec, cancel, |_| {}).await?;
    if timed_out {
        return Err(HrnError::Tool(format!(
            "{} did not finish within {}s",
            spec.program,
            TIMEOUT.as_secs()
        )));
    }
    Ok(Outcome {
        code: exit_code,
        stdout,
        stderr,
    })
}

/// Drive `fut` to completion from a synchronous caller (`read` resolves URLs on a blocking
/// thread). A scratch thread runs it so the caller may sit on a runtime worker too.
pub(crate) fn block_on<F>(fut: F) -> Result<F::Output>
where
    F: Future + Send,
    F::Output: Send,
{
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|e| HrnError::Tool(format!("no async runtime for `gh`: {e}")))?;
    std::thread::scope(|scope| scope.spawn(|| handle.block_on(fut)).join())
        .map_err(|_| HrnError::Tool("`gh` worker thread panicked".to_owned()))
}
