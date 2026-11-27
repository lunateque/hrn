# `eval` runtime reference

One `eval` cell runs in the live interpreter of its language (`language`: `py` or `js`).

## What a cell sees

- Top-level state persists between calls of the same language: imports, variables, functions,
  classes. A new session starts empty; `reset: true` wipes that language's interpreter first.
- The value of the last expression is echoed like a REPL (`py`: `repr`, `js`: `util.inspect`);
  assignments and declarations echo nothing. stdout and stderr are captured together, in order.
- An exception prints its traceback and keeps prior state intact.

## Predefined helpers (Python and JavaScript)

- `read(path)` / `write(path, content)` — UTF-8 files (relative to cwd) or internal URLs (`local://`, `agent://`, etc.).
- `sh(cmd)` — run a POSIX shell command; returns stdout+stderr, raises on non-zero exit.
- `env` — read environment variables (`env("KEY", default)` or `env.get(...)`).
- `output(*args, image=...)` — print values or attach inline images (`ToolOutput.images`).
- `log(*args)`, `phase(name)`, `budget()` — structured cell progress and concurrency/timeout budget.
- `tool.<name>(args)` — invoke any registered harness tool through the full approval ladder and
  hooks (`py`: sync; `js`: async/await). `checkpoint`, `rewind`, and `yield` are refused.
- `@tool` (`py`) / `tool(fn)` (`js`) — register a kernel-defined tool shareable with `task`
  subagents via `tools: ["<name>"]` (gated by `[eval] tools_enabled`).
- `completion(prompt, role=None)` — lightweight no-tool LLM completion.
- `judge(prompt)` / `judge_batch(prompts)` (`judgeBatch` in `js`) — completion using `[roles] judge`
  (falling back to `@smol`).
- `agent(prompt, **opts)` — spawn a background subagent returning an `AgentHandle`
  (`.id`, `.status`, `.done()`, `.wait(timeout=None)`, `.send(msg)`, `.cancel()`, `.output()`;
  awaitable in `py`). Unwaited agents auto-deliver to the session inbox.
- `wait(handles, timeout=None)` — wait on one or more `AgentHandle`s and consume their deliveries.
- `workpool(max_concurrency=None)` — bounded fan-out pool capped at `[task] max_concurrency`
  (`.push`, `.map`, `.status`, `.peek`, `.wait`, `.close`).
- `computer.*` — desktop automation facade (`windows`, `window`, `ax`, `find`, `displays`,
  `capabilities`, `clipboard_read`, `clipboard_write`, `screenshot`, etc.) wrapping `tool.desktop`
  with its full approval gate; active only when `[desktop] eval_prelude = true` and `desktop` is registered.

## Magics (Python)

- Host-side standalone cells: `%pip <args>` (runs pip with watchdog paused) and
  `%load <path-or-local://>` (reads file or internal URL and executes in the kernel).
- Runner-side line magics: `!cmd`, `%cd`, `%pwd`, `%ls`, `%env`, `%set_env`, `%time`, `%timeit`,
  `%who`, `%whos`, `%reset`, `%run`.
- Runner-side cell magics: `%%bash`, `%%capture [var]`, `%%writefile [-a] <path>`, `%%time`, `%%timeit`.
- Unknown `%magic` / `%%magic` raises `NameError`.

## Timeouts, watchdog, and output spill

- `[eval]` defaults: `py = true`, `js = true`, `tools_enabled = true`, `default_timeout_secs = 30`
  (clamped `1..=3600`, `0` disables).
- The cell timeout watchdog pauses automatically during `%pip`, `wait()`, `judge()`, `completion()`,
  and `tool.task()`.
- Output exceeding 50 KiB (or lines >4 KiB) keeps a head+tail inline window and spills full output
  to `local://N.log`, recorded in `ToolOutput.meta`.
