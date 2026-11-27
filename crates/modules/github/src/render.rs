//! Compact markdown for issues, pull requests, lists and diffs from `gh --json` payloads.

use hrn_kernel::serde_json::Value;
use std::fmt::Write as _;

/// Changed files listed inline in a PR view; the rest are behind `pr://N/diff`.
const FILE_PREVIEW: usize = 50;

/// String field `key` of `v`, empty when absent.
pub(crate) fn field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn num(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// `@login` of an author object (`author` for `gh --json`, `user` for REST payloads).
pub(crate) fn who(v: &Value) -> String {
    let login = ["author", "user"]
        .iter()
        .filter_map(|k| v.get(k))
        .map(|a| field(a, "login"))
        .find(|l| !l.is_empty());
    login.map_or_else(|| "@ghost".to_owned(), |l| format!("@{l}"))
}

/// The `title` of an issue or PR on one line (titles may carry newlines).
pub(crate) fn title(v: &Value) -> String {
    field(v, "title")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The date part of an ISO timestamp.
pub(crate) fn day(ts: &str) -> &str {
    ts.get(..10).unwrap_or(ts)
}

fn names(v: &Value, key: &str, name: &str) -> String {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| field(item, name))
        .filter(|n| !n.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn body(text: &str) -> &str {
    let text = text.trim();
    if text.is_empty() {
        "_No description provided._"
    } else {
        text
    }
}

fn meta_line(out: &mut String, v: &Value) {
    let labels = names(v, "labels", "name");
    let assignees = names(v, "assignees", "login");
    let milestone = v.get("milestone").map_or("", |m| field(m, "title"));
    for (label, value) in [
        ("Labels", labels.as_str()),
        ("Assignees", assignees.as_str()),
        ("Milestone", milestone),
    ] {
        if !value.is_empty() {
            let _ = writeln!(out, "{label}: {value}");
        }
    }
}

fn comments(out: &mut String, list: Option<&Value>) {
    let items = list
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Comments ({})", items.len());
    for c in items {
        let _ = writeln!(
            out,
            "\n### {} · {}\n{}",
            who(c),
            day(field(c, "createdAt")),
            c.get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
        );
    }
}

/// One issue with its conversation (`with_comments`).
pub(crate) fn issue(v: &Value, with_comments: bool) -> String {
    let mut out = format!(
        "# Issue #{}: {}\n{} · {} · opened {} · updated {}\n{}\n",
        num(v, "number"),
        title(v),
        field(v, "state").to_lowercase(),
        who(v),
        day(field(v, "createdAt")),
        day(field(v, "updatedAt")),
        field(v, "url"),
    );
    meta_line(&mut out, v);
    let _ = write!(out, "\n{}\n", body(field(v, "body")));
    if with_comments {
        comments(&mut out, v.get("comments"));
    }
    out.truncate(out.trim_end().len());
    out
}

/// CI summary of `statusCheckRollup`: `12 passed, 1 failed (lint), 2 pending`.
fn checks(rollup: &[Value]) -> String {
    let (mut passed, mut pending) = (0, 0);
    let mut failed = Vec::new();
    for c in rollup {
        let name = [field(c, "name"), field(c, "context")]
            .into_iter()
            .find(|n| !n.is_empty())
            .unwrap_or("?");
        let verdict = if c.get("conclusion").is_some() {
            if field(c, "status") == "COMPLETED" {
                field(c, "conclusion")
            } else {
                "PENDING"
            }
        } else {
            field(c, "state")
        };
        match verdict {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => passed += 1,
            "PENDING" | "EXPECTED" | "" => pending += 1,
            _ => failed.push(name),
        }
    }
    let mut parts = vec![format!("{passed} passed")];
    if !failed.is_empty() {
        parts.push(format!("{} failed ({})", failed.len(), failed.join(", ")));
    }
    if pending > 0 {
        parts.push(format!("{pending} pending"));
    }
    parts.join(", ")
}

fn reviews(out: &mut String, list: Option<&Value>) {
    let items: Vec<&Value> = list
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| !field(r, "state").is_empty())
        .collect();
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Reviews ({})", items.len());
    for r in items {
        let _ = write!(
            out,
            "\n### {} · {} · {}\n",
            who(r),
            field(r, "state"),
            day(field(r, "submittedAt"))
        );
        let text = field(r, "body").trim();
        if !text.is_empty() {
            let _ = writeln!(out, "{text}");
        }
    }
}

/// Inline review comments from `GET /pulls/N/comments`.
fn review_comments(out: &mut String, list: &Value) {
    let items = list.as_array().map_or(&[][..], Vec::as_slice);
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Review comments ({})", items.len());
    for c in items {
        let line = c
            .get("line")
            .or_else(|| c.get("original_line"))
            .and_then(Value::as_i64)
            .map_or_else(String::new, |l| format!(":{l}"));
        let reply = if c.get("in_reply_to_id").is_some_and(|r| !r.is_null()) {
            " (reply)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "\n### {}{line}{reply} · {} · {}\n{}",
            field(c, "path"),
            who(c),
            day(field(c, "created_at")),
            field(c, "body").trim()
        );
    }
}

/// One pull request with files, checks, reviews and conversation.
pub(crate) fn pull(v: &Value, inline: &Value, with_comments: bool) -> String {
    let state = if field(v, "mergedAt").is_empty() {
        field(v, "state").to_lowercase()
    } else {
        "merged".to_owned()
    };
    let draft = if v.get("isDraft").and_then(Value::as_bool) == Some(true) {
        " · draft"
    } else {
        ""
    };
    let mut out = format!(
        "# PR #{}: {}\n{state}{draft} · {} · {} ← {} · +{} −{} in {} files\n{}\nopened {} · updated {}\n",
        num(v, "number"),
        title(v),
        who(v),
        field(v, "baseRefName"),
        field(v, "headRefName"),
        num(v, "additions"),
        num(v, "deletions"),
        num(v, "changedFiles"),
        field(v, "url"),
        day(field(v, "createdAt")),
        day(field(v, "updatedAt")),
    );
    meta_line(&mut out, v);
    for (label, key) in [("Review", "reviewDecision"), ("Mergeable", "mergeable")] {
        let value = field(v, key);
        if !value.is_empty() {
            let _ = writeln!(out, "{label}: {}", value.to_lowercase());
        }
    }
    if let Some(rollup) = v
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .filter(|r| !r.is_empty())
    {
        let _ = writeln!(out, "Checks: {}", checks(rollup));
    }
    let _ = write!(out, "\n{}\n", body(field(v, "body")));
    let files = v
        .get("files")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if !files.is_empty() {
        let _ = writeln!(out, "\n## Files ({})", files.len());
        for f in files.iter().take(FILE_PREVIEW) {
            let _ = writeln!(
                out,
                "- {} (+{} −{})",
                field(f, "path"),
                num(f, "additions"),
                num(f, "deletions")
            );
        }
        if files.len() > FILE_PREVIEW {
            let _ = writeln!(
                out,
                "- … {} more; `read pr://{}/diff` lists all",
                files.len() - FILE_PREVIEW,
                num(v, "number")
            );
        }
    }
    if with_comments {
        reviews(&mut out, v.get("reviews"));
        review_comments(&mut out, inline);
        comments(&mut out, v.get("comments"));
    }
    out.truncate(out.trim_end().len());
    out
}

/// A browse listing of issues or pull requests.
pub(crate) fn list(kind: &str, repo: Option<&str>, state: &str, items: &Value) -> String {
    let items = items.as_array().map_or(&[][..], Vec::as_slice);
    let mut out = format!(
        "# {kind} in {} ({state}, {} shown)\n",
        repo.unwrap_or("this checkout's repository"),
        items.len()
    );
    if items.is_empty() {
        out.push_str("\n(none)");
        return out;
    }
    for it in items {
        let labels = names(it, "labels", "name");
        let labels = if labels.is_empty() {
            String::new()
        } else {
            format!(" · [{labels}]")
        };
        let head = field(it, "headRefName");
        let head = if head.is_empty() {
            String::new()
        } else {
            let draft = if it.get("isDraft").and_then(Value::as_bool) == Some(true) {
                " draft"
            } else {
                ""
            };
            format!(" · {head}{draft}")
        };
        let _ = writeln!(
            out,
            "- #{} {} ({}) {}{head}{labels} · updated {}",
            num(it, "number"),
            title(it),
            field(it, "state").to_lowercase(),
            who(it),
            day(field(it, "updatedAt")),
        );
    }
    out.truncate(out.trim_end().len());
    out
}

/// One file's slice of a unified diff.
pub(crate) struct FileDiff {
    pub(crate) path: String,
    pub(crate) text: String,
    pub(crate) added: usize,
    pub(crate) removed: usize,
}

/// Split a unified diff at its `diff --git` headers.
pub(crate) fn split_diff(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    for line in diff.split_inclusive('\n') {
        if let Some(header) = line.strip_prefix("diff --git ") {
            let path = header
                .trim_end()
                .rsplit_once(" b/")
                .map_or(header.trim_end(), |(_, p)| p);
            files.push(FileDiff {
                path: path.to_owned(),
                text: String::new(),
                added: 0,
                removed: 0,
            });
            in_hunk = false;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk {
            match line.as_bytes().first() {
                Some(b'+') => file.added += 1,
                Some(b'-') => file.removed += 1,
                _ => {}
            }
        }
        file.text.push_str(line);
    }
    files
}

/// The numbered changed-file listing of `pr://N/diff`.
pub(crate) fn diff_listing(number: u64, files: &[FileDiff]) -> String {
    let (added, removed) = files
        .iter()
        .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    let mut out = format!(
        "# PR #{number} diff: {} files (+{added} −{removed})\n\n",
        files.len()
    );
    for (i, f) in files.iter().enumerate() {
        let _ = writeln!(out, "{}. {} (+{} −{})", i + 1, f.path, f.added, f.removed);
    }
    let _ = write!(
        out,
        "\n`read pr://{number}/diff/<i>` shows one file; `read pr://{number}/diff/all` the whole diff."
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::serde_json::json;

    #[test]
    fn splits_diff_and_counts_lines() {
        let diff = "diff --git a/a.txt b/a.txt\nindex 1..2 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n-old\n+new\n same\ndiff --git a/b b/b\nnew file mode 100644\n--- /dev/null\n+++ b/b\n@@ -0,0 +1 @@\n+x\n";
        let files = split_diff(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(
            (files[0].path.as_str(), files[0].added, files[0].removed),
            ("a.txt", 1, 1)
        );
        assert_eq!((files[1].path.as_str(), files[1].added), ("b", 1));
        assert!(files[0].text.ends_with(" same\n"));
        assert!(diff_listing(9, &files).contains("2. b (+1 −0)"));
    }

    #[test]
    fn summarises_checks() {
        let rollup = json!([
            {"name": "a", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "lint", "status": "COMPLETED", "conclusion": "FAILURE"},
            {"name": "b", "status": "IN_PROGRESS", "conclusion": ""},
            {"context": "ci/x", "state": "SUCCESS"}
        ]);
        let rollup = rollup.as_array().expect("array");
        assert_eq!(checks(rollup), "2 passed, 1 failed (lint), 1 pending");
    }

    #[test]
    fn issue_view_shows_conversation() {
        let v = json!({
            "number": 4, "title": "T", "state": "OPEN", "url": "u", "body": "",
            "author": {"login": "me"}, "createdAt": "2026-01-02T03:04:05Z", "updatedAt": "2026-01-03T00:00:00Z",
            "labels": [{"name": "bug"}],
            "comments": [{"author": {"login": "you"}, "createdAt": "2026-01-04T00:00:00Z", "body": "hi"}]
        });
        let md = issue(&v, true);
        assert!(md.starts_with("# Issue #4: T\nopen · @me · opened 2026-01-02"));
        assert!(md.contains("Labels: bug"));
        assert!(md.contains("_No description provided._"));
        assert!(md.ends_with("### @you · 2026-01-04\nhi"));
        assert!(!issue(&v, false).contains("## Comments"));
    }
}
