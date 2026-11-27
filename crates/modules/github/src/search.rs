//! `search_issues`, `search_prs`, `search_code`, `search_commits`, `search_repos`.

use crate::args;
use crate::dates;
use crate::gh::Gh;
use crate::render::{day, field, title, who};
use hrn_kernel::serde_json::{self, Value};
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::fmt::Write as _;
use std::path::Path;

const LIMIT_DEFAULT: usize = 10;
const LIMIT_MAX: usize = 50;

/// What a search looks for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Issues,
    Prs,
    Code,
    Commits,
    Repos,
}

impl Kind {
    fn endpoint(self) -> &'static str {
        match self {
            Self::Issues | Self::Prs => "issues",
            Self::Code => "code",
            Self::Commits => "commits",
            Self::Repos => "repositories",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Issues => "issues",
            Self::Prs => "pull requests",
            Self::Code => "code",
            Self::Commits => "commits",
            Self::Repos => "repositories",
        }
    }
}

/// The date qualifier for `since` / `until`, or `None` when neither is given.
fn date_qualifier(kind: Kind, a: &serde_json::Value) -> Result<Option<String>> {
    let (since, until) = (args::opt(a, "since"), args::opt(a, "until"));
    if kind == Kind::Code {
        return if since.is_some() || until.is_some() {
            Err(HrnError::Tool(
                "`since` / `until` do not apply to search_code".to_owned(),
            ))
        } else {
            Ok(None)
        };
    }
    let field = match (kind, args::opt(a, "dateField")) {
        (Kind::Commits, _) => "committer-date",
        (Kind::Repos, Some("updated")) => "pushed",
        (_, Some("updated")) => "updated",
        (_, Some("created") | None) => "created",
        (_, Some(other)) => {
            return Err(HrnError::Tool(format!(
                "`dateField` must be `created` or `updated`, got `{other}`"
            )));
        }
    };
    let now = dates::now_secs();
    let since = since.map(|s| dates::bound(s, now)).transpose()?;
    let until = until.map(|s| dates::bound(s, now)).transpose()?;
    Ok(match (since, until) {
        (None, None) => None,
        (Some(s), Some(u)) => Some(format!("{field}:{s}..{u}")),
        (Some(s), None) => Some(format!("{field}:>={s}")),
        (None, Some(u)) => Some(format!("{field}:<={u}")),
    })
}

fn has_scope_qualifier(query: &str) -> bool {
    query.split_whitespace().any(|t| {
        let t = t.trim_start_matches('-');
        ["repo:", "org:", "user:", "owner:"]
            .iter()
            .any(|q| t.starts_with(q))
    })
}

/// Run one search and render it.
pub(crate) async fn run(
    gh: &Gh,
    kind: Kind,
    a: &serde_json::Value,
    cwd: &Path,
    cancel: &CancelFlag,
) -> Result<String> {
    let limit = args::count(a, "limit", LIMIT_DEFAULT, LIMIT_MAX)?;
    let query = args::opt(a, "query").unwrap_or_default();
    let date = date_qualifier(kind, a)?;
    if query.is_empty() && (kind == Kind::Code || date.is_none()) {
        return Err(HrnError::Tool(if kind == Kind::Code {
            "`query` is required".to_owned()
        } else {
            "`query` is required (or pass `since`/`until` to filter by date)".to_owned()
        }));
    }
    let mut parts: Vec<String> = vec![query.to_owned()];
    parts.extend(date);
    let mut hostname = None;
    let mut scope = None;
    if kind != Kind::Repos {
        let repo = match args::opt(a, "repo") {
            Some(r) => Some(r.to_owned()),
            None if !has_scope_qualifier(query) => current_repo(gh, cwd, cancel).await,
            None => None,
        };
        if let Some(repo) = repo {
            let slug = match repo
                .split_once('/')
                .filter(|_| repo.matches('/').count() == 2)
            {
                Some((host, slug)) => {
                    hostname = Some(host.to_owned());
                    slug.to_owned()
                }
                None => repo,
            };
            parts.push(format!("repo:{slug}"));
            scope = Some(slug);
        }
    }
    match kind {
        Kind::Issues => parts.push("is:issue".to_owned()),
        Kind::Prs => parts.push("is:pr".to_owned()),
        _ => {}
    }
    let q = parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut cmd = vec!["api".to_owned(), "-X".to_owned(), "GET".to_owned()];
    cmd.extend(
        hostname
            .into_iter()
            .flat_map(|h| ["--hostname".to_owned(), h]),
    );
    cmd.push(format!("/search/{}", kind.endpoint()));
    cmd.extend([
        "-f".to_owned(),
        format!("q={q}"),
        "-F".to_owned(),
        format!("per_page={limit}"),
    ]);
    if kind == Kind::Code {
        cmd.extend([
            "-H".to_owned(),
            "Accept: application/vnd.github.text-match+json".to_owned(),
        ]);
    }
    let response = gh.json(cwd, &cmd, cancel).await?;
    let items = response
        .get("items")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let total = response
        .get("total_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut out = format!("# GitHub {} search\nQuery: `{q}`\n", kind.title());
    if let Some(scope) = scope {
        let _ = writeln!(out, "Repo: {scope}");
    }
    let _ = writeln!(out, "Results: {} of {total}\n", items.len());
    for item in items {
        item_line(&mut out, kind, item);
    }
    out.truncate(out.trim_end().len());
    Ok(out)
}

/// `owner/repo` of the checkout, or `None` when `gh` cannot tell (outside a GitHub checkout the
/// search simply is not scoped, as in omp).
async fn current_repo(gh: &Gh, cwd: &Path, cancel: &CancelFlag) -> Option<String> {
    let args = [
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "-q",
        ".nameWithOwner",
    ];
    gh.text(cwd, args, cancel)
        .await
        .ok()
        .filter(|s| !s.is_empty())
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}

fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", text.get(..cut).unwrap_or_default()),
        None => text.to_owned(),
    }
}

fn str_at<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn item_line(out: &mut String, kind: Kind, it: &Value) {
    match kind {
        Kind::Issues | Kind::Prs => issue_line(out, it),
        Kind::Code => code_line(out, it),
        Kind::Commits => commit_line(out, it),
        Kind::Repos => repo_line(out, it),
    }
}

fn issue_line(out: &mut String, it: &Value) {
    let repo = field(it, "repository_url")
        .rsplitn(3, '/')
        .collect::<Vec<_>>();
    let repo = match repo.as_slice() {
        [name, owner, _] => format!("{owner}/{name}"),
        _ => String::new(),
    };
    let merged = it
        .pointer("/pull_request/merged_at")
        .is_some_and(|m| !m.is_null());
    let state = if merged { "merged" } else { field(it, "state") };
    let labels = it
        .get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|l| field(l, "name"))
        .collect::<Vec<_>>()
        .join(", ");
    let labels = if labels.is_empty() {
        String::new()
    } else {
        format!(" · [{labels}]")
    };
    let _ = writeln!(
        out,
        "- {repo}#{} {} ({state}) {}{labels} · created {} · updated {}\n  {}",
        it.get("number").and_then(Value::as_u64).unwrap_or(0),
        title(it),
        who(it),
        day(field(it, "created_at")),
        day(field(it, "updated_at")),
        field(it, "html_url"),
    );
}

fn code_line(out: &mut String, it: &Value) {
    let sha = field(it, "sha");
    let _ = writeln!(
        out,
        "- {} — {} (Commit {})\n  {}",
        field(it, "path"),
        str_at(it, "/repository/full_name"),
        sha.get(..7).unwrap_or(sha),
        field(it, "html_url"),
    );
    let fragment =
        Some(first_line(str_at(it, "/text_matches/0/fragment"))).filter(|l| !l.is_empty());
    if let Some(line) = fragment {
        let _ = writeln!(out, "  > {}", clip(line, 200));
    }
}

fn commit_line(out: &mut String, it: &Value) {
    let sha = field(it, "sha");
    let author = Some(str_at(it, "/author/login"))
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| str_at(it, "/commit/author/name"));
    let _ = writeln!(
        out,
        "- {} {} — {} · {author} · {}\n  {}",
        sha.get(..7).unwrap_or(sha),
        clip(first_line(str_at(it, "/commit/message")), 120),
        str_at(it, "/repository/full_name"),
        day(str_at(it, "/commit/author/date")),
        field(it, "html_url"),
    );
}

fn repo_line(out: &mut String, it: &Value) {
    let flag = |key: &str, word: &str| {
        if it.get(key).and_then(Value::as_bool) == Some(true) {
            format!(" · {word}")
        } else {
            String::new()
        }
    };
    let num = |key: &str| it.get(key).and_then(Value::as_u64).unwrap_or(0);
    let _ = writeln!(
        out,
        "- {} — {} · {} · ★{} · forks {} · issues {} · {}{}{} · updated {}\n  {}",
        field(it, "full_name"),
        clip(first_line(field(it, "description")), 160),
        it.get("language").and_then(Value::as_str).unwrap_or("-"),
        num("stargazers_count"),
        num("forks_count"),
        num("open_issues_count"),
        field(it, "visibility"),
        flag("archived", "archived"),
        flag("fork", "fork"),
        day(field(it, "updated_at")),
        field(it, "html_url"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::serde_json::json;

    #[test]
    fn date_qualifiers() {
        let both = json!({ "since": "2024-01-01", "until": "2024-02-01" });
        assert_eq!(
            date_qualifier(Kind::Issues, &both).expect("q").as_deref(),
            Some("created:2024-01-01..2024-02-01")
        );
        let upd = json!({ "since": "2024-01-01", "dateField": "updated" });
        assert_eq!(
            date_qualifier(Kind::Repos, &upd).expect("q").as_deref(),
            Some("pushed:>=2024-01-01")
        );
        assert_eq!(
            date_qualifier(Kind::Commits, &upd).expect("q").as_deref(),
            Some("committer-date:>=2024-01-01")
        );
        assert!(date_qualifier(Kind::Code, &upd).is_err());
        assert_eq!(date_qualifier(Kind::Prs, &json!({})).expect("none"), None);
    }

    #[test]
    fn scope_qualifier_detection() {
        assert!(has_scope_qualifier("bug repo:cli/cli"));
        assert!(has_scope_qualifier("-org:x bug"));
        assert!(!has_scope_qualifier("bug label:repo"));
    }

    #[test]
    fn renders_an_issue_hit() {
        let item = json!({
            "number": 3, "title": "Boom", "state": "open", "html_url": "https://x/3",
            "repository_url": "https://api.github.com/repos/cli/cli", "user": {"login": "a"},
            "created_at": "2024-01-02T00:00:00Z", "updated_at": "2024-01-03T00:00:00Z",
            "labels": [{"name": "bug"}]
        });
        let mut out = String::new();
        item_line(&mut out, Kind::Issues, &item);
        assert_eq!(
            out,
            "- cli/cli#3 Boom (open) @a · [bug] · created 2024-01-02 · updated 2024-01-03\n  https://x/3\n"
        );
    }
}
