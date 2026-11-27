Delegate scoped work to isolated child agents (`scout` [alias `explore`], `task`, `sonic` [alias `quick_task`], `plan`, `reviewer`, `security-reviewer`, or discovered `agents/*.md` profiles).
Pass required `context` and `tasks[]` (with `solutionSpace` describing complexity). Async mode returns agent ids immediately and auto-delivers results; sync/blocking agents return settled summaries.
Children start blank: `context` + `task` must be self-contained; verify claimed results. Inspect with `read agent://<id>`, steer/revive with `write agent://<id>`, cancel with `write proc://<id>/kill`.
