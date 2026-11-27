Run one cell of code in a persistent Python (`py`) or JavaScript (`js`, Bun) interpreter. State
(variables, imports, functions) persists per (session, language). Prefer this over `python -c` /
`bun -e` through `bash`. Exceptions return a traceback without losing prior state.

- Top-level `await` works in both; `js` also supports `return`, `import` and redeclaring globals.
- Prelude helpers: `read`, `write`, `sh`, `env`, `output`, `log`, `phase`, `budget`;
  `tool.<name>(args)` (approval-gated; `checkpoint`/`rewind`/`yield` refused); `@tool`/`tool(fn)`
  for kernel tools shareable to `task` children; `completion()`, `judge()`/`judge_batch()`;
  `agent()`/`wait()`/`workpool()`; and `computer.*` when enabled. Magics: `%pip`, `%load` (incl.
  `local://`), `!cmd`, `%cd/%pwd/%env/%time/%timeit/%who/%reset/%run`, `%%bash/%%capture/%%writefile`.
  Full reference in `help://eval`.
- `timeout` defaults to 30s (clamp 1..=3600, 0 disables; paused during `wait`/`judge`/`%pip`/`task`).
  `reset: true` restarts only this language. Output >50 KiB spills to `local://N.log`.
