//! The `github_write` tool: every operation that changes GitHub or the local checkout. Each call
//! asks for confirmation.

use crate::gh::{Gh, reason};
use crate::render::{self, day, field, who};
use crate::urls::repo_flag;
use crate::{args, checkout};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, CancelFlag, HrnError, Result, Tool, ToolApproval, ToolContext, ToolOutput, ToolSpec,
};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The `github_write` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/github_write.md");

/// The confirmation-gated `github_write` tool.
pub(crate) struct GithubWriteTool {
    pub(crate) gh: Gh,
}

/// A body written to a temp file for `--body-file` (long, multi-line text never rides a command
/// line), removed again by [`TempBody::finish`].
struct TempBody(PathBuf);

impl TempBody {
    fn create(text: &str) -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path =
            std::env::temp_dir().join(format!("hrn-gh-body-{}-{nanos}.md", std::process::id()));
        std::fs::write(&path, text)?;
        Ok(Self(path))
    }

    /// Delete the file; a failure becomes a note for the tool output.
    fn finish(self) -> Option<String> {
        std::fs::remove_file(&self.0).err().map(|e| {
            format!(
                "\n(warning: could not remove temp file {}: {e})",
                self.0.display()
            )
        })
    }
}

/// `--body-file <tmp>` for a non-empty body, `--body ""` otherwise (suppresses the editor).
fn body_flags(body: Option<&str>) -> Result<(Vec<String>, Option<TempBody>)> {
    match body.filter(|b| !b.trim().is_empty()) {
        Some(text) => {
            let tmp = TempBody::create(text)?;
            let flags = vec![
                "--body-file".to_owned(),
                tmp.0.to_string_lossy().into_owned(),
            ];
            Ok((flags, Some(tmp)))
        }
        None => Ok((vec!["--body".to_owned(), String::new()], None)),
    }
}

fn repeated(flag: &str, values: Vec<String>) -> impl Iterator<Item = String> {
    values.into_iter().flat_map(move |v| [flag.to_owned(), v])
}

/// An issue / PR number given as an integer or a numeric string (or a URL).
fn target(a: &Value, key: &str, op: &str) -> Result<String> {
    let raw = match a.get(key) {
        Some(Value::Number(n)) => n.to_string(),
        _ => args::required(a, key, op)?.to_owned(),
    };
    args::no_flag(&raw, key)?;
    Ok(raw)
}

impl Tool for GithubWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "github_write".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": [
                        "pr_create", "issue_create", "issue_comment", "pr_comment", "pr_merge",
                        "pr_checkout", "pr_push"
                    ] },
                    "repo": { "type": "string", "description": "[host/]owner/repo; defaults to the current checkout's repository." },
                    "number": { "type": "string", "description": "issue_comment / pr_comment / pr_merge: issue or PR number (or URL)." },
                    "pr": { "type": "array", "items": { "type": "string" }, "description": "pr_checkout: PR numbers, branches or URLs (default: the current branch's PR)." },
                    "title": { "type": "string", "description": "pr_create (unless fill) / issue_create." },
                    "body": { "type": "string", "description": "pr_create (excludes fill) / issue_create / comments: markdown body; pr_merge: commit body." },
                    "base": { "type": "string", "description": "pr_create: base branch." },
                    "head": { "type": "string", "description": "pr_create: head branch." },
                    "draft": { "type": "boolean", "description": "pr_create: open as draft." },
                    "fill": { "type": "boolean", "description": "pr_create: take title and body from the commits (excludes title and body)." },
                    "reviewer": { "type": "array", "items": { "type": "string" }, "description": "pr_create reviewers." },
                    "assignee": { "type": "array", "items": { "type": "string" }, "description": "pr_create / issue_create assignees." },
                    "label": { "type": "array", "items": { "type": "string" }, "description": "pr_create / issue_create labels." },
                    "method": { "type": "string", "enum": ["merge", "squash", "rebase"], "description": "pr_merge strategy (default merge)." },
                    "delete_branch": { "type": "boolean", "description": "pr_merge: delete the head branch afterwards." },
                    "auto": { "type": "boolean", "description": "pr_merge: merge once requirements are met." },
                    "force": { "type": "boolean", "description": "pr_checkout: reset an existing `pr-<N>` branch that has no worktree." },
                    "branch": { "type": "string", "description": "pr_push: local branch (default: the current one)." },
                    "forceWithLease": { "type": "boolean", "description": "pr_push: git push --force-with-lease." }
                },
                "required": ["op"]
            }),
        }
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::exec()
    }

    fn summarize_call(&self, args: &Value) -> String {
        let op = args::opt(args, "op").unwrap_or("?");
        let what = ["repo", "number", "title", "branch"]
            .iter()
            .filter_map(|k| args::opt(args, k))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{op} {what}").trim_end().to_owned()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let gh = self.gh.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let op = args::required(&args, "op", "github_write")?;
            let (cwd, cancel) = (&ctx.cwd, &ctx.cancel);
            let text = match op {
                "pr_create" => pr_create(&gh, &args, cwd, cancel).await?,
                "issue_create" => issue_create(&gh, &args, cwd, cancel).await?,
                "issue_comment" => comment(&gh, "issue", &args, cwd, cancel).await?,
                "pr_comment" => comment(&gh, "pr", &args, cwd, cancel).await?,
                "pr_merge" => pr_merge(&gh, &args, cwd, cancel).await?,
                "pr_checkout" => checkout::checkout(&gh, &args, cwd, cancel).await?,
                "pr_push" => checkout::push(&args, cwd, cancel).await?,
                other => {
                    return Err(HrnError::Tool(format!(
                        "unknown github_write op `{other}` (reads are in `github`)"
                    )));
                }
            };
            Ok(ToolOutput::ok(text))
        })
    }
}

/// Run a mutating `gh` command; gh prints confirmations to either stream.
async fn run_write(
    gh: &Gh,
    cmd: &[String],
    cwd: &Path,
    cancel: &CancelFlag,
    tmp: Option<TempBody>,
) -> Result<String> {
    let result = gh.exec(cwd, cmd, cancel).await;
    let note = tmp.and_then(TempBody::finish);
    let out = result?;
    if out.code != Some(0) {
        return Err(out.failure(cmd, "gh"));
    }
    let text = [out.stdout.trim(), out.stderr.trim()]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("done");
    Ok(format!("{text}{}", note.unwrap_or_default()))
}

/// The created resource's URL: the last line of `gh … create` output.
fn last_url(text: &str) -> &str {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with("http"))
        .unwrap_or(text)
}

async fn pr_create(gh: &Gh, a: &Value, cwd: &Path, cancel: &CancelFlag) -> Result<String> {
    let fill = args::flag(a, "fill");
    let (title, body) = (args::opt(a, "title"), a.get("body").and_then(Value::as_str));
    if fill && (title.is_some() || body.is_some()) {
        return Err(HrnError::Tool(
            "`fill` excludes `title` and `body`".to_owned(),
        ));
    }
    if !fill && title.is_none() {
        return Err(HrnError::Tool(
            "`pr_create` requires `title` unless `fill` is true".to_owned(),
        ));
    }
    let mut cmd = vec!["pr".to_owned(), "create".to_owned()];
    cmd.extend(repo_flag(args::opt(a, "repo")));
    let mut tmp = None;
    if fill {
        cmd.push("--fill".to_owned());
    } else {
        cmd.extend(["--title".to_owned(), title.unwrap_or_default().to_owned()]);
        let (flags, body_file) = body_flags(body)?;
        cmd.extend(flags);
        tmp = body_file;
    }
    for (flag, key) in [("--base", "base"), ("--head", "head")] {
        if let Some(v) = args::opt(a, key) {
            cmd.extend([flag.to_owned(), v.to_owned()]);
        }
    }
    if args::flag(a, "draft") {
        cmd.push("--draft".to_owned());
    }
    cmd.extend(repeated("--reviewer", args::list(a, "reviewer")));
    cmd.extend(repeated("--assignee", args::list(a, "assignee")));
    cmd.extend(repeated("--label", args::list(a, "label")));
    let created = run_write(gh, &cmd, cwd, cancel, tmp).await?;
    let url = last_url(&created).to_owned();
    let view = [
        "pr",
        "view",
        &url,
        "--json",
        "number,title,url,state,isDraft,baseRefName,headRefName,author,createdAt,labels",
    ];
    match gh.json(cwd, view, cancel).await {
        Ok(v) => {
            let labels = v
                .get("labels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|l| field(l, "name"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut out = format!(
                "# Created Pull Request #{}: {}\n{}\n{}{} · {} ← {} · {} · {}",
                v.get("number").and_then(Value::as_u64).unwrap_or(0),
                render::title(&v),
                field(&v, "url"),
                field(&v, "state").to_lowercase(),
                if v.get("isDraft").and_then(Value::as_bool) == Some(true) {
                    " · draft"
                } else {
                    ""
                },
                field(&v, "baseRefName"),
                field(&v, "headRefName"),
                who(&v),
                day(field(&v, "createdAt")),
            );
            if !labels.is_empty() {
                let _ = write!(out, "\nLabels: {labels}");
            }
            Ok(out)
        }
        Err(e) => Ok(format!(
            "Created pull request {url}\n(summary unavailable: {})",
            reason(&e)
        )),
    }
}

async fn issue_create(gh: &Gh, a: &Value, cwd: &Path, cancel: &CancelFlag) -> Result<String> {
    let title = args::required(a, "title", "issue_create")?;
    let mut cmd = vec!["issue".to_owned(), "create".to_owned()];
    cmd.extend(repo_flag(args::opt(a, "repo")));
    cmd.extend(["--title".to_owned(), title.to_owned()]);
    let (flags, tmp) = body_flags(a.get("body").and_then(Value::as_str))?;
    cmd.extend(flags);
    cmd.extend(repeated("--assignee", args::list(a, "assignee")));
    cmd.extend(repeated("--label", args::list(a, "label")));
    let created = run_write(gh, &cmd, cwd, cancel, tmp).await?;
    Ok(format!("Created issue {}", last_url(&created)))
}

async fn comment(
    gh: &Gh,
    noun: &str,
    a: &Value,
    cwd: &Path,
    cancel: &CancelFlag,
) -> Result<String> {
    let number = target(a, "number", &format!("{noun}_comment"))?;
    let body = args::required(a, "body", &format!("{noun}_comment"))?;
    let tmp = TempBody::create(body)?;
    let mut cmd = vec![noun.to_owned(), "comment".to_owned(), number.clone()];
    cmd.extend([
        "--body-file".to_owned(),
        tmp.0.to_string_lossy().into_owned(),
    ]);
    cmd.extend(repo_flag(args::opt(a, "repo")));
    let out = run_write(gh, &cmd, cwd, cancel, Some(tmp)).await?;
    Ok(format!("Commented on {noun} {number}: {}", last_url(&out)))
}

async fn pr_merge(gh: &Gh, a: &Value, cwd: &Path, cancel: &CancelFlag) -> Result<String> {
    let number = target(a, "number", "pr_merge")?;
    let method = match args::opt(a, "method").unwrap_or("merge") {
        m @ ("merge" | "squash" | "rebase") => format!("--{m}"),
        other => {
            return Err(HrnError::Tool(format!(
                "`method` must be merge, squash or rebase, got `{other}`"
            )));
        }
    };
    let mut cmd = vec!["pr".to_owned(), "merge".to_owned(), number, method];
    for (flag, key) in [("--delete-branch", "delete_branch"), ("--auto", "auto")] {
        if args::flag(a, key) {
            cmd.push(flag.to_owned());
        }
    }
    cmd.extend(repo_flag(args::opt(a, "repo")));
    let mut tmp = None;
    if let Some(body) = a
        .get("body")
        .and_then(Value::as_str)
        .filter(|b| !b.trim().is_empty())
    {
        let file = TempBody::create(body)?;
        cmd.extend([
            "--body-file".to_owned(),
            file.0.to_string_lossy().into_owned(),
        ]);
        tmp = Some(file);
    }
    run_write(gh, &cmd, cwd, cancel, tmp).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_output_yields_its_url() {
        assert_eq!(
            last_url("\nCreating pull request…\nhttps://github.com/o/r/pull/9\n"),
            "https://github.com/o/r/pull/9"
        );
        assert_eq!(last_url("no url"), "no url");
    }

    #[test]
    fn targets_accept_numbers_and_reject_flags() {
        assert_eq!(
            target(&json!({ "number": 12 }), "number", "x").expect("int"),
            "12"
        );
        assert_eq!(
            target(&json!({ "number": " 7 " }), "number", "x").expect("str"),
            "7"
        );
        assert!(target(&json!({ "number": "--web" }), "number", "x").is_err());
        assert!(target(&json!({}), "number", "x").is_err());
    }

    #[test]
    fn body_goes_through_a_temp_file_unless_empty() {
        let (flags, tmp) = body_flags(Some("  ")).expect("empty");
        assert_eq!(flags, ["--body", ""]);
        assert!(tmp.is_none());
        let (flags, tmp) = body_flags(Some("hello\nworld")).expect("body");
        let tmp = tmp.expect("temp file");
        assert_eq!(flags.first().map(String::as_str), Some("--body-file"));
        assert_eq!(
            std::fs::read_to_string(&tmp.0).expect("read"),
            "hello\nworld"
        );
        assert_eq!(tmp.finish(), None);
    }
}
