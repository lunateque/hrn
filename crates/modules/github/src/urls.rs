//! `issue://` and `pr://`: GitHub issues and pull requests through the `gh` CLI.

use crate::gh::Gh;
use crate::render::{self, FileDiff};
use crate::target::{self, DiffSel, Target};
use hrn_edge_rt::tokio;
use hrn_kernel::serde_json::Value;
use hrn_kernel::{CancelFlag, HrnError, Result, ToolContext, UrlBody, UrlScheme};
use std::fmt::Write as _;
use std::path::Path;

const ISSUE_FIELDS: &str = "number,title,state,stateReason,author,labels,assignees,milestone,\
    createdAt,updatedAt,closedAt,body,comments,url";
const PR_FIELDS: &str = "number,title,state,isDraft,author,baseRefName,headRefName,url,body,\
    labels,assignees,milestone,createdAt,updatedAt,mergedAt,mergeable,reviewDecision,additions,\
    deletions,changedFiles,files,comments,reviews,statusCheckRollup";
const ISSUE_LIST_FIELDS: &str = "number,title,state,author,labels,updatedAt";
const PR_LIST_FIELDS: &str = "number,title,state,author,labels,updatedAt,headRefName,isDraft";
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;
/// The files API stops here, as in omp.
const MAX_DIFF_FILES: usize = 3000;

/// Which of the two schemes a [`GithubScheme`] serves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Issue,
    Pr,
}

impl Kind {
    /// The scheme name, which is also the `gh` subcommand.
    fn noun(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Pr => "pr",
        }
    }
}

/// `issue://` or `pr://`.
pub(crate) struct GithubScheme {
    pub(crate) gh: Gh,
    pub(crate) kind: Kind,
}

impl UrlScheme for GithubScheme {
    fn scheme(&self) -> &'static str {
        self.kind.noun()
    }

    fn about(&self) -> &'static str {
        match self.kind {
            Kind::Issue => {
                "GitHub issues via `gh`; `issue://<N>` is one issue with its comments \
                 (`?comments=0` drops them), `issue://<owner>/<repo>/<N>` another repository's, \
                 bare `issue://` or `issue://<owner>/<repo>` lists recent ones \
                 (`?state=open|closed|all&limit=&author=&label=`)"
            }
            Kind::Pr => {
                "GitHub pull requests via `gh`; `pr://<N>` is one PR with checks, reviews and \
                 comments (`?comments=0` drops them), `pr://<owner>/<repo>/<N>` another \
                 repository's, bare `pr://` or `pr://<owner>/<repo>` lists recent ones \
                 (`?state=open|closed|merged|all&limit=&author=&label=`); `pr://<N>/diff` lists \
                 changed files, `pr://<N>/diff/<i>` shows file i (1-indexed), `pr://<N>/diff/all` \
                 the whole diff"
            }
        }
    }

    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        let target = target::parse(self.kind.noun(), rest, self.kind == Kind::Pr)?;
        let (gh, kind, cwd, cancel) = (&self.gh, self.kind, &ctx.cwd, &ctx.cancel);
        crate::gh::block_on(async {
            match (target.number, target.diff) {
                (None, _) => list(gh, kind, &target, cwd, cancel).await,
                (Some(n), Some(sel)) => pr_diff(gh, &target, n, sel, cwd, cancel).await,
                (Some(n), None) => view(gh, kind, &target, n, cwd, cancel).await,
            }
        })?
        .map(UrlBody::Text)
    }
}

/// `--repo <repo>` when the address names one.
pub(crate) fn repo_flag(repo: Option<&str>) -> Vec<String> {
    repo.map_or_else(Vec::new, |r| vec!["--repo".to_owned(), r.to_owned()])
}

/// `gh api [--hostname H] [--paginate --slurp] <path>` for `repo`; `{owner}/{repo}` resolves from
/// the checkout. `path` receives the `owner/repo` slug.
pub(crate) fn api_args(
    repo: Option<&str>,
    paged: bool,
    path: impl FnOnce(&str) -> String,
) -> Vec<String> {
    let mut args = vec!["api".to_owned()];
    let slug = match repo.and_then(|r| r.split_once('/').filter(|_| r.matches('/').count() == 2)) {
        Some((host, slug)) => {
            args.extend(["--hostname".to_owned(), host.to_owned()]);
            slug
        }
        None => repo.unwrap_or("{owner}/{repo}"),
    };
    if paged {
        args.extend(["--paginate".to_owned(), "--slurp".to_owned()]);
    }
    args.push(path(slug));
    args
}

/// The items of every page of a `--paginate --slurp` response.
pub(crate) fn flatten_pages(pages: &Value) -> Vec<Value> {
    pages
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .flatten()
        .cloned()
        .collect()
}

async fn list(gh: &Gh, kind: Kind, t: &Target, cwd: &Path, cancel: &CancelFlag) -> Result<String> {
    let state = t.param("state").unwrap_or("open");
    let allowed: &[&str] = match kind {
        Kind::Issue => &["open", "closed", "all"],
        Kind::Pr => &["open", "closed", "merged", "all"],
    };
    if !allowed.contains(&state) {
        return Err(HrnError::Tool(format!(
            "`state` must be one of {}, got `{state}`",
            allowed.join(", ")
        )));
    }
    let limit = match t.param("limit") {
        None => DEFAULT_LIMIT,
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                HrnError::Tool(format!("`limit` must be a positive integer, got `{raw}`"))
            })?
            .min(MAX_LIMIT),
    };
    let (title, fields) = match kind {
        Kind::Issue => ("Issues", ISSUE_LIST_FIELDS),
        Kind::Pr => ("Pull requests", PR_LIST_FIELDS),
    };
    let mut args = vec![
        kind.noun().to_owned(),
        "list".to_owned(),
        "--state".to_owned(),
        state.to_owned(),
        "--limit".to_owned(),
        limit.to_string(),
        "--json".to_owned(),
        fields.to_owned(),
    ];
    for (flag, key) in [("--author", "author"), ("--label", "label")] {
        if let Some(value) = t.param(key) {
            args.extend([flag.to_owned(), value.to_owned()]);
        }
    }
    args.extend(repo_flag(t.repo.as_deref()));
    let items = gh.json(cwd, &args, cancel).await?;
    Ok(render::list(title, t.repo.as_deref(), state, &items))
}

async fn view(
    gh: &Gh,
    kind: Kind,
    t: &Target,
    number: u64,
    cwd: &Path,
    cancel: &CancelFlag,
) -> Result<String> {
    let with_comments = t.param("comments") != Some("0");
    let fields = match kind {
        Kind::Issue => ISSUE_FIELDS,
        Kind::Pr => PR_FIELDS,
    };
    let fields = if with_comments {
        fields.to_owned()
    } else {
        fields
            .split(',')
            .filter(|f| !matches!(*f, "comments" | "reviews"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut args = vec![
        kind.noun().to_owned(),
        "view".to_owned(),
        number.to_string(),
        "--json".to_owned(),
        fields,
    ];
    args.extend(repo_flag(t.repo.as_deref()));
    if kind == Kind::Issue {
        let issue = gh.json(cwd, &args, cancel).await?;
        return Ok(render::issue(&issue, with_comments));
    }
    let inline = async {
        if !with_comments {
            return Ok(Value::Null);
        }
        let api = api_args(t.repo.as_deref(), true, |slug| {
            format!("repos/{slug}/pulls/{number}/comments?per_page=100")
        });
        let pages = gh.json(cwd, &api, cancel).await?;
        Ok::<_, HrnError>(Value::Array(flatten_pages(&pages)))
    };
    let (pr, inline) = tokio::join!(gh.json(cwd, &args, cancel), inline);
    Ok(render::pull(&pr?, &inline?, with_comments))
}

/// The unified diff of a PR, falling back to the paginated files API when GitHub refuses an
/// oversized aggregate diff.
pub(crate) async fn fetch_diff(
    gh: &Gh,
    repo: Option<&str>,
    number: u64,
    cwd: &Path,
    cancel: &CancelFlag,
) -> Result<String> {
    let mut args = vec!["pr".to_owned(), "diff".to_owned(), number.to_string()];
    args.extend(repo_flag(repo));
    let first = gh.exec(cwd, &args, cancel).await?;
    if first.code == Some(0) {
        return Ok(first.stdout);
    }
    let refused = first.stderr.contains("exceeded the maximum") || first.stderr.contains("406");
    let err = first.failure(&args, "gh");
    if !refused {
        return Err(err);
    }
    let api = api_args(repo, true, |slug| {
        format!("repos/{slug}/pulls/{number}/files?per_page=100")
    });
    let pages = gh.json(cwd, &api, cancel).await?;
    let mut files = flatten_pages(&pages);
    files.truncate(MAX_DIFF_FILES);
    if files.is_empty() {
        return Err(err);
    }
    let mut out = String::new();
    for f in &files {
        let name = render::field(f, "filename");
        let _ = write!(
            out,
            "diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n"
        );
        match f.get("patch").and_then(Value::as_str) {
            Some(patch) => {
                out.push_str(patch);
                out.push('\n');
            }
            None => out.push_str("[patch unavailable: binary or too large]\n"),
        }
    }
    Ok(out)
}

async fn pr_diff(
    gh: &Gh,
    t: &Target,
    number: u64,
    sel: DiffSel,
    cwd: &Path,
    cancel: &CancelFlag,
) -> Result<String> {
    let diff = fetch_diff(gh, t.repo.as_deref(), number, cwd, cancel).await?;
    if sel == DiffSel::All {
        return Ok(diff);
    }
    let files: Vec<FileDiff> = render::split_diff(&diff);
    match sel {
        DiffSel::File(i) => files
            .into_iter()
            .nth(i.saturating_sub(1))
            .map(|f| f.text)
            .ok_or_else(|| {
                HrnError::Tool(format!(
                    "pr://{number}/diff has no file {i}; list them with `pr://{number}/diff`"
                ))
            }),
        DiffSel::Files | DiffSel::All => Ok(render::diff_listing(number, &files)),
    }
}
