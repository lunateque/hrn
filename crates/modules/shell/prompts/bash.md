Persistent shell (Git Bash on Windows): `cd`, `export` and functions carry over between calls.
On timeout or cancel only the running command's process tree is killed. No trailing `&`.
- `async: true`: background job, a job id returns at once; the final result arrives as a
  follow-up message — NEVER poll.
- `notify: "<regex>"`: implies async; each matching output line also arrives while the job runs
  (batched; switches itself off after 30 notices).
- `name`: a supervised long-lived service (dev server, watcher); returns once `ready` (a `log`
  regex over its output and/or a TCP `port`) holds, or fails on exit/timeout with its log tail.
  It runs until the session ends or it is stopped; naming a live service restarts it.
- `pty: true`: pseudo-terminal so interactive programs (REPLs, installers, prompts) work; the
  call returns when the command exits or output goes quiet, and a job that keeps running is
  steered with `write proc://<id>` (types) and `read proc://<id>` (inspects).
- A foreground command still running past the auto-background wait (default 60s) becomes a
  background job: the answer is what it printed so far plus the job id.
`timeout_secs` default 120, max 3600; with `async`, 0 means no deadline; ignored for services.
Tool commands of this shell: help://bash/tool-commands.
