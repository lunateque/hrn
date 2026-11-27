//! `run_watch`: follow GitHub Actions runs of one run id or one commit until they settle, then
//! report the failed jobs with their log tails.

use crate::args;
use crate::gh::{self, Gh};
use crate::render::field;
use crate::urls::api_args;
use hrn_edge_rt::tokio;
use hrn_kernel::serde_json::{self, Value};
use hrn_kernel::{AgentEvent, HrnError, Result, ToolContext};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

const TAIL_DEFAULT: usize = 15;
const TAIL_MAX: usize = 200;
/// Poll every 3 s for the first minute, then every 15 s.
const FAST_WINDOW: Duration = Duration::from_mins(1);
const FAST: Duration = Duration::from_secs(3);
const SLOW: Duration = Duration::from_secs(15);
/// Wait this long after the first failure so concurrent failures are included.
const GRACE: Duration = Duration::from_secs(5);
/// A commit with no workflow runs after this long is reported as having none.
const NO_RUNS_GIVE_UP: Duration = Duration::from_secs(90);
const MAX_POLL_FAILURES: u32 = 5;

/// `(repo with optional host, run id)` from a numeric id or an Actions run URL.
fn parse_run(raw: &str) -> Result<(Option<String>, u64)> {
    if let Ok(id) = raw.parse() {
        return Ok((None, id));
    }
    let bad = || {
        HrnError::Tool(format!(
            "`run` must be a numeric run id or a run URL, got `{raw}`"
        ))
    };
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .ok_or_else(bad)?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [host, owner, repo, "actions", "runs", id, ..] => {
            let id = id.parse().map_err(|_| bad())?;
            let repo = if *host == "github.com" {
                format!("{owner}/{repo}")
            } else {
                format!("{host}/{owner}/{repo}")
            };
            Ok((Some(repo), id))
        }
        _ => Err(bad()),
    }
}

/// Drop control characters and ANSI escape sequences from a CI log.
fn sanitize(log: &str) -> String {
    let mut out = String::with_capacity(log.len());
    let mut chars = log.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.next_if_eq(&'[').is_some() {
                    while chars.next().is_some_and(|c| !c.is_ascii_alphabetic()) {}
                }
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn tail_lines(log: &str, n: usize) -> String {
    let lines: Vec<&str> = log.lines().collect();
    lines
        .get(lines.len().saturating_sub(n)..)
        .unwrap_or_default()
        .join("\n")
}

fn is_success(conclusion: &str) -> bool {
    matches!(conclusion, "success" | "neutral" | "skipped")
}

fn is_failure(job: &Value) -> bool {
    field(job, "status") == "completed" && !is_success(field(job, "conclusion"))
}

/// One run and its jobs.
struct RunState {
    run: Value,
    jobs: Vec<Value>,
}

impl RunState {
    fn id(&self) -> u64 {
        self.run.get("id").and_then(Value::as_u64).unwrap_or(0)
    }

    fn done(&self) -> bool {
        field(&self.run, "status") == "completed"
    }

    fn failed(&self) -> bool {
        (self.done() && !is_success(field(&self.run, "conclusion")))
            || self.jobs.iter().any(is_failure)
    }
}

fn failed_step(job: &Value) -> Option<&str> {
    job.get("steps")
        .and_then(Value::as_array)?
        .iter()
        .find(|s| is_failure(s))
        .map(|s| field(s, "name"))
}

fn snapshot(states: &[RunState]) -> String {
    let mut out = String::new();
    for s in states {
        let state = if s.done() {
            field(&s.run, "conclusion")
        } else {
            field(&s.run, "status")
        };
        let _ = writeln!(
            out,
            "run {} {} [{state}] {}",
            s.id(),
            field(&s.run, "name"),
            field(&s.run, "html_url")
        );
        for job in &s.jobs {
            let mark = match (field(job, "status"), is_failure(job)) {
                ("completed", false) => "✓".to_owned(),
                ("completed", true) => match failed_step(job) {
                    Some(step) => format!("✗ (step: {step})"),
                    None => "✗".to_owned(),
                },
                (other, _) => format!("… {other}"),
            };
            let _ = writeln!(out, "  {mark} {}", field(job, "name"));
        }
    }
    out
}

struct Watch<'a> {
    gh: &'a Gh,
    ctx: &'a ToolContext,
    /// `[host/]owner/repo`.
    repo: String,
}

impl Watch<'_> {
    async fn api(&self, paged: bool, path: impl FnOnce(&str) -> String) -> Result<Value> {
        let cmd = api_args(Some(&self.repo), paged, path);
        self.gh.json(&self.ctx.cwd, &cmd, &self.ctx.cancel).await
    }

    async fn state_of(&self, run: Value) -> Result<RunState> {
        let id = run.get("id").and_then(Value::as_u64).unwrap_or(0);
        let jobs = self
            .api(false, |slug| {
                format!("repos/{slug}/actions/runs/{id}/jobs?per_page=100")
            })
            .await?;
        let jobs = jobs
            .get("jobs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(RunState { run, jobs })
    }

    async fn fetch(&self, target: &Target) -> Result<Vec<RunState>> {
        let runs = match target {
            Target::Run(id) => vec![
                self.api(false, |slug| format!("repos/{slug}/actions/runs/{id}"))
                    .await?,
            ],
            Target::Commit(sha) => {
                let data = self
                    .api(false, |slug| {
                        format!("repos/{slug}/actions/runs?head_sha={sha}&per_page=100")
                    })
                    .await?;
                data.get("workflow_runs")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            }
        };
        let mut states = Vec::with_capacity(runs.len());
        for run in runs {
            states.push(self.state_of(run).await?);
        }
        Ok(states)
    }

    async fn emit(&self, text: String) {
        let delta = AgentEvent::ToolDelta {
            call_id: self.ctx.call_id.clone(),
            delta: text,
        };
        self.ctx.events.emit(delta).await;
    }

    async fn sleep(&self, d: Duration) -> Result {
        tokio::select! {
            () = tokio::time::sleep(d) => Ok(()),
            () = self.ctx.cancel.cancelled() => Err(HrnError::Cancelled),
        }
    }

    /// Log of one job; `Err` carries why it is unavailable.
    async fn job_log(&self, job_id: u64) -> std::result::Result<String, String> {
        let cmd = api_args(Some(&self.repo), false, |slug| {
            format!("repos/{slug}/actions/jobs/{job_id}/logs")
        });
        match self.gh.exec(&self.ctx.cwd, &cmd, &self.ctx.cancel).await {
            Ok(out) if out.code == Some(0) && !out.stdout.trim().is_empty() => {
                Ok(sanitize(&out.stdout))
            }
            Ok(out) if out.code == Some(0) => Err("the log is empty".to_owned()),
            Ok(out) => Err(format!("exit {:?}: {}", out.code, clip(out.stderr.trim()))),
            Err(e) => Err(clip(&gh::reason(&e))),
        }
    }

    async fn report(&self, header: String, states: &[RunState], tail: usize) -> Result<String> {
        let mut out = format!("{header}\n\n{}", snapshot(states));
        let mut artifact = String::new();
        let (mut missing, mut shown) = (false, false);
        for s in states {
            for job in s.jobs.iter().filter(|j| is_failure(j)) {
                let id = job.get("id").and_then(Value::as_u64).unwrap_or(0);
                let name = field(job, "name");
                let _ = write!(out, "\n## Failed job: {name}");
                if let Some(step) = failed_step(job) {
                    let _ = write!(out, " (step: {step})");
                }
                match self.job_log(id).await {
                    Ok(log) => {
                        shown = true;
                        let _ = write!(out, "\n```\n{}\n```\n", tail_lines(&log, tail));
                        let _ = write!(artifact, "===== {name} =====\n{log}\n");
                    }
                    Err(why) => {
                        missing = true;
                        let _ = write!(out, "\nLog tail unavailable: {why}\n");
                        let _ = write!(artifact, "===== {name} =====\nunavailable: {why}\n");
                    }
                }
            }
        }
        if !artifact.is_empty()
            && let Some(url) = self.ctx.urls.spill(self.ctx, &artifact)?
        {
            let label = match (shown, missing) {
                (true, false) => "Full failed-job logs",
                (true, true) => "Partial failed-job logs",
                (false, _) => "Failed-job log diagnostics",
            };
            let _ = write!(out, "\n{label}: {url}");
        }
        out.truncate(out.trim_end().len());
        Ok(out)
    }
}

fn clip(text: &str) -> String {
    text.chars().take(512).collect()
}

enum Target {
    Run(u64),
    Commit(String),
}

/// The checkout's own `owner/repo`.
async fn checkout_repo(gh: &Gh, ctx: &ToolContext) -> Result<String> {
    let args = [
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "-q",
        ".nameWithOwner",
    ];
    gh.text(&ctx.cwd, args, &ctx.cancel).await
}

/// `repo` (`[host/]owner/repo`) is the checkout's repository `local` (`owner/repo`).
fn same_repo(repo: &str, local: &str) -> bool {
    let (repo, local) = (repo.to_lowercase(), local.to_lowercase());
    repo == local || repo.ends_with(&format!("/{local}"))
}

/// What to watch: the repository, the run(s), and the report header.
async fn resolve(
    gh: &Gh,
    ctx: &ToolContext,
    a: &serde_json::Value,
) -> Result<(String, Target, String)> {
    let given = args::opt(a, "repo").map(str::to_owned);
    let (url_repo, run_id) = match args::opt(a, "run") {
        Some(raw) => {
            let (repo, id) = parse_run(raw)?;
            (repo, Some(id))
        }
        None => (None, None),
    };
    if let (Some(g), Some(u)) = (&given, &url_repo)
        && !g.eq_ignore_ascii_case(u)
    {
        return Err(HrnError::Tool(format!(
            "`repo` {g} does not match the run URL's repository {u}"
        )));
    }
    let repo = match given.or(url_repo) {
        Some(r) => r,
        None => checkout_repo(gh, ctx).await?,
    };
    if let Some(id) = run_id {
        let header = format!("# Watched run {id} on {repo}");
        return Ok((repo, Target::Run(id), header));
    }
    let sha = if let Some(branch) = args::opt(a, "branch") {
        let cmd = api_args(Some(&repo), false, |slug| {
            format!("repos/{slug}/branches/{branch}")
        });
        let data = gh.json(&ctx.cwd, &cmd, &ctx.cancel).await?;
        data.pointer("/commit/sha")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| HrnError::Tool(format!("branch `{branch}` has no head commit")))?
    } else {
        let local = checkout_repo(gh, ctx).await?;
        if !same_repo(&repo, &local) {
            return Err(HrnError::Tool(format!(
                "the current checkout is {local}, not {repo}; pass `branch` or `run` to watch another repository"
            )));
        }
        gh::git(&ctx.cwd, ["rev-parse", "HEAD"], &ctx.cancel).await?
    };
    let header = format!(
        "# Watched commit {} on {repo}",
        sha.get(..7).unwrap_or(&sha)
    );
    Ok((repo, Target::Commit(sha), header))
}

/// Watch until the run (or every run of the commit) settles.
pub(crate) async fn run(gh: &Gh, ctx: &ToolContext, a: &serde_json::Value) -> Result<String> {
    let tail = args::count(a, "tail", TAIL_DEFAULT, TAIL_MAX)?;
    let (repo, target, header) = resolve(gh, ctx, a).await?;
    let w = Watch { gh, ctx, repo };
    let start = Instant::now();
    let (mut failures, mut last, mut confirmed) = (0, String::new(), None::<Vec<u64>>);
    loop {
        let states = match w.fetch(&target).await {
            Ok(states) => {
                failures = 0;
                states
            }
            Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
            Err(e) => {
                failures += 1;
                if failures >= MAX_POLL_FAILURES {
                    return Err(HrnError::Tool(format!(
                        "giving up after {failures} failed polls: {}",
                        gh::reason(&e)
                    )));
                }
                w.sleep(SLOW).await?;
                continue;
            }
        };
        let text = snapshot(&states);
        if text != last {
            w.emit(text.clone()).await;
            last = text;
        }
        if states.iter().any(RunState::failed) {
            w.emit("a job failed; collecting logs…\n".to_owned()).await;
            w.sleep(GRACE).await?;
            let states = w.fetch(&target).await?;
            return w.report(format!("{header}: failed"), &states, tail).await;
        }
        if !states.is_empty() && states.iter().all(RunState::done) {
            let ids: Vec<u64> = states.iter().map(RunState::id).collect();
            // A commit may still start late workflows; succeed only if none appeared one poll later.
            if matches!(target, Target::Run(_)) || confirmed.as_ref() == Some(&ids) {
                let n = states.len();
                let plural = if n == 1 { "" } else { "s" };
                let header = format!("{header}: succeeded ({n} run{plural})");
                return w.report(header, &states, tail).await;
            }
            confirmed = Some(ids);
        } else {
            confirmed = None;
            if states.is_empty() && start.elapsed() >= NO_RUNS_GIVE_UP {
                return Ok(format!(
                    "{header}: no workflow runs appeared within {}s",
                    NO_RUNS_GIVE_UP.as_secs()
                ));
            }
        }
        let wait = if start.elapsed() < FAST_WINDOW {
            FAST
        } else {
            SLOW
        };
        w.sleep(wait).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_run_references() {
        assert_eq!(parse_run("42").expect("id"), (None, 42));
        assert_eq!(
            parse_run("https://github.com/cli/cli/actions/runs/7/job/9").expect("url"),
            (Some("cli/cli".to_owned()), 7)
        );
        assert_eq!(
            parse_run("https://ghe.x/o/r/actions/runs/7")
                .expect("ghe")
                .0
                .as_deref(),
            Some("ghe.x/o/r")
        );
        assert!(parse_run("run-1").is_err());
        assert!(parse_run("https://github.com/cli/cli/pull/7").is_err());
    }

    #[test]
    fn sanitizes_logs_and_tails() {
        assert_eq!(sanitize("\u{1b}[31mred\u{1b}[0m\r\nok\u{7}"), "red\nok");
        assert_eq!(tail_lines("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail_lines("a", 5), "a");
    }
}
