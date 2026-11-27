GitHub changes through the `gh` CLI: `pr_create`, `issue_create`, `issue_comment`,
`pr_comment`, `pr_merge` (`method` merge|squash|rebase, `delete_branch`, `auto`),
`pr_checkout` (a PR into a `.harness/wt/pr-<N>` worktree on branch `pr-<N>`; `force` resets an
existing branch that has no worktree) and `pr_push` (push a checked-out PR branch back;
`forceWithLease`). Always confirmed by the user; confirm before every push. Read-only lookups
are `github` and the issue/PR URLs.
