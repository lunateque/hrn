GitHub through the `gh` CLI (read-only): `repo_view` summaries, `file_read` of repository
files (prefer it over curl), `search_issues` / `search_prs` / `search_code` /
`search_commits` / `search_repos`, `pr_checks` for one PR, and `run_watch`, which follows
GitHub Actions runs of a commit or branch until they finish (`run` picks a run id or URL,
`tail` how many failed-log lines per job). A single issue or pull request is a `read` URL, not
an op here (see the Internal URLs block); creating, commenting, merging and checking out live
in `github_write`.
