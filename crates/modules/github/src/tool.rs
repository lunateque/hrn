//! The `github` tool: read-only repository, file, search, check and Actions operations.

use crate::args;
use crate::gh::Gh;
use crate::render::field;
use crate::search::{self, Kind};
use crate::urls::repo_flag;
use crate::{files, watch};
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{
    BoxFuture, CancelFlag, HrnError, Result, Tool, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::fmt::Write as _;
use std::path::Path;

const REPO_FIELDS: &str = "nameWithOwner,description,url,defaultBranchRef,visibility,\
    viewerPermission,primaryLanguage,stargazerCount,forkCount,isArchived,isFork,updatedAt,\
    homepageUrl,repositoryTopics,licenseInfo";

/// The `github` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/github.md");

/// The read-only `github` tool.
pub(crate) struct GithubTool {
    pub(crate) gh: Gh,
}

impl Tool for GithubTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "github".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": [
                        "repo_view", "file_read", "pr_checks", "search_issues", "search_prs",
                        "search_code", "search_commits", "search_repos", "run_watch"
                    ] },
                    "repo": { "type": "string", "description": "[host/]owner/repo; defaults to the current checkout's repository (search_repos ignores it)." },
                    "branch": { "type": "string", "description": "repo_view / file_read ref; run_watch: watch that branch's head (default: local HEAD)." },
                    "path": { "type": "string", "description": "file_read: repository-relative file path." },
                    "pr": { "type": "string", "description": "pr_checks: PR number, branch or URL (default: the current branch's PR)." },
                    "query": { "type": "string", "description": "search_*: GitHub search query. Required for search_code; the others accept since/until instead." },
                    "since": { "type": "string", "description": "search_* lower date bound: 3d, 12h, 2w, 2mo, 1y, YYYY-MM-DD or ISO datetime. Not for search_code." },
                    "until": { "type": "string", "description": "search_* upper date bound; same formats as since." },
                    "dateField": { "type": "string", "enum": ["created", "updated"], "description": "Field since/until filter on (default created)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "search_* result count (default 10)." },
                    "run": { "type": "string", "description": "run_watch: Actions run id or run URL; omit to watch every run of a commit." },
                    "tail": { "type": "integer", "minimum": 1, "maximum": 200, "description": "run_watch: failed-log lines per job (default 15)." }
                },
                "required": ["op"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn summarize_call(&self, args: &Value) -> String {
        let op = args::opt(args, "op").unwrap_or("?");
        let what = ["repo", "path", "query", "pr", "run", "branch"]
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
            let op = args::required(&args, "op", "github")?;
            let (cwd, cancel) = (&ctx.cwd, &ctx.cancel);
            let text = match op {
                "repo_view" => repo_view(&gh, &args, cwd, cancel).await?,
                "file_read" => files::read(&gh, &args, cwd, cancel).await?,
                "pr_checks" => pr_checks(&gh, &args, cwd, cancel).await?,
                "run_watch" => watch::run(&gh, &ctx, &args).await?,
                "search_issues" => search::run(&gh, Kind::Issues, &args, cwd, cancel).await?,
                "search_prs" => search::run(&gh, Kind::Prs, &args, cwd, cancel).await?,
                "search_code" => search::run(&gh, Kind::Code, &args, cwd, cancel).await?,
                "search_commits" => search::run(&gh, Kind::Commits, &args, cwd, cancel).await?,
                "search_repos" => search::run(&gh, Kind::Repos, &args, cwd, cancel).await?,
                other => {
                    return Err(HrnError::Tool(format!(
                        "unknown github op `{other}` (mutating ops are in `github_write`)"
                    )));
                }
            };
            Ok(ToolOutput::ok(text))
        })
    }
}

async fn repo_view(gh: &Gh, a: &Value, cwd: &Path, cancel: &CancelFlag) -> Result<String> {
    let mut cmd = vec!["repo".to_owned(), "view".to_owned()];
    cmd.extend(
        args::opt(a, "repo")
            .map(|r| args::no_flag(r, "repo").map(str::to_owned))
            .transpose()?,
    );
    if let Some(branch) = args::opt(a, "branch") {
        cmd.extend(["--branch".to_owned(), branch.to_owned()]);
    }
    cmd.extend(["--json".to_owned(), REPO_FIELDS.to_owned()]);
    let v = gh.json(cwd, &cmd, cancel).await?;
    let mut out = format!("# {}\n", field(&v, "nameWithOwner"));
    let description = field(&v, "description");
    if !description.is_empty() {
        let _ = writeln!(out, "{description}");
    }
    let _ = writeln!(out, "{}", field(&v, "url"));
    let str_at = |ptr: &str| {
        v.pointer(ptr)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let num = |key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
    let flag = |key: &str| v.get(key).and_then(Value::as_bool) == Some(true);
    let mut rows = vec![
        ("Default branch", str_at("/defaultBranchRef/name")),
        ("Visibility", field(&v, "visibility").to_lowercase()),
        ("Permission", field(&v, "viewerPermission").to_lowercase()),
        ("Language", str_at("/primaryLanguage/name")),
        ("License", str_at("/licenseInfo/name")),
        ("Stars", num("stargazerCount").to_string()),
        ("Forks", num("forkCount").to_string()),
        ("Updated", field(&v, "updatedAt").to_owned()),
        ("Homepage", field(&v, "homepageUrl").to_owned()),
    ];
    if let Some(branch) = args::opt(a, "branch") {
        rows.insert(1, ("Requested branch", branch.to_owned()));
    }
    for (label, value) in rows.into_iter().filter(|(_, v)| !v.is_empty()) {
        let _ = writeln!(out, "- {label}: {value}");
    }
    for (key, word) in [("isArchived", "archived"), ("isFork", "fork")] {
        if flag(key) {
            let _ = writeln!(out, "- {word}");
        }
    }
    let topics: Vec<&str> = v
        .get("repositoryTopics")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.pointer("/name").and_then(Value::as_str))
        .collect();
    if !topics.is_empty() {
        let _ = writeln!(out, "- Topics: {}", topics.join(", "));
    }
    out.truncate(out.trim_end().len());
    Ok(out)
}

async fn pr_checks(gh: &Gh, a: &Value, cwd: &Path, cancel: &CancelFlag) -> Result<String> {
    let mut cmd = vec!["pr".to_owned(), "checks".to_owned()];
    cmd.extend(
        args::opt(a, "pr")
            .map(|p| args::no_flag(p, "pr").map(str::to_owned))
            .transpose()?,
    );
    cmd.extend(repo_flag(args::opt(a, "repo")));
    cmd.extend([
        "--json".to_owned(),
        "name,state,bucket,workflow,link,description".to_owned(),
    ]);
    let out = gh.exec(cwd, &cmd, cancel).await?;
    // Exit 8 means "checks pending"; the JSON is still complete.
    if !matches!(out.code, Some(0 | 8)) {
        if out.stderr.contains("no checks reported") {
            return Ok("No checks reported.".to_owned());
        }
        return Err(out.failure(&cmd, "gh"));
    }
    let checks: Value = serde_json::from_str(out.stdout.trim())
        .map_err(|e| HrnError::Tool(format!("gh returned invalid JSON: {e}")))?;
    let items = checks.as_array().map_or(&[][..], Vec::as_slice);
    let count = |bucket: &str| {
        items
            .iter()
            .filter(|c| field(c, "bucket") == bucket)
            .count()
    };
    let mut text = format!(
        "# Checks: {} pass, {} fail, {} pending, {} skipped\n\n",
        count("pass"),
        count("fail"),
        count("pending"),
        count("skipping") + count("cancel"),
    );
    for c in items {
        let workflow = field(c, "workflow");
        let workflow = if workflow.is_empty() {
            String::new()
        } else {
            format!(" ({workflow})")
        };
        let _ = writeln!(
            text,
            "- {}: {}{workflow} {}",
            field(c, "bucket"),
            field(c, "name"),
            field(c, "link")
        );
    }
    text.truncate(text.trim_end().len());
    Ok(text)
}
