"""Embedded Python prelude for `hrn` eval kernels (stdlib-only, zero third-party deps).

Provides:
- `read`, `write`, `sh`, `ShellError`, `env`, `output`, `log`, `phase`, `budget`
- `tool.<name>(args)` dynamic proxy and `@tool` / `tool(fn)` kernel-tool registration
- `completion(prompt, role=None)`, `judge(prompt)`, `judge_batch(prompts)`
- `agent(prompt, **opts)` -> `AgentHandle` (awaitable), `wait(handles, timeout=None)`, `workpool()`
- `computer.*` facade wrapping `tool.desktop` (only exposed when enabled)
- Runner-side magic transformer (`!cmd`, `%cd/%pwd/%ls/%env/%set_env/%time/%timeit/%who/%whos/%reset/%run/%pip`, `%%bash/%%capture/%%writefile/%%time/%%timeit`; unknown magic -> `NameError`)
"""

import base64
import concurrent.futures
import contextlib
import http.client
import inspect
import io
import json
import os
import re
import shlex
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

_BRIDGE_URL = ""
_BRIDGE_TOKEN = ""
_MAX_CONCURRENCY = 32
_COMPUTER_ENABLED = False
_SHELL = {}
_GLOBALS = None
_LOOP = None
_PRELUDE_KEYS = set()
_REGISTERED_TOOLS = {}
_PENDING_IMAGES = []
_CURRENT_PHASE = None
_CELL_STARTED = 0.0
_HIDDEN_ENV_KEYS = frozenset({"HRN_EVAL_BRIDGE", "HRN_EVAL_TOKEN"})


class ShellError(RuntimeError):
    """`sh()` ran a command that exited non-zero."""

    def __init__(self, code, output):
        super().__init__(f"command exited with code {code}\n{output}")
        self.code = code
        self.output = output


class CompletionResult(str):
    """String result returned by `completion()` / `judge()` with `.text` and `.attempts`."""

    def __new__(cls, text, attempts=1):
        obj = super().__new__(cls, text)
        obj.text = str(text)
        obj.attempts = attempts
        return obj


def _bridge_post(path, payload=None):
    if not _BRIDGE_URL:
        raise RuntimeError("eval bridge is not configured")
    data = json.dumps(payload if payload is not None else {}).encode("utf-8")
    req = urllib.request.Request(
        _BRIDGE_URL + path,
        data=data,
        headers={
            "Authorization": f"Bearer {_BRIDGE_TOKEN}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        try:
            parsed = json.loads(body)
            msg = parsed.get("error") or f"HTTP {exc.code}"
        except Exception:
            msg = f"HTTP {exc.code}: {body.strip()}"
        raise RuntimeError(f"eval bridge error on {path}: {msg}") from None
    except urllib.error.URLError as exc:
        raise RuntimeError(f"eval bridge connection failed on {path}: {exc.reason}") from None


def _watchdog_pause():
    if _BRIDGE_URL:
        try:
            _bridge_post("/watchdog/pause", {})
        except Exception:
            pass


def _watchdog_resume():
    if _BRIDGE_URL:
        try:
            _bridge_post("/watchdog/resume", {})
        except Exception:
            pass


def read(path):
    """Read a UTF-8 file (or internal `scheme://...` URL via `tool.read`)."""
    if isinstance(path, str) and "://" in path and _BRIDGE_URL:
        return tool.read({"path": path})
    with open(path, encoding="utf-8", errors="replace", newline="") as f:
        return f.read()


def write(path, content):
    """Write a UTF-8 file creating parent directories (or internal `scheme://...` URL via `tool.write`)."""
    if isinstance(path, str) and "://" in path and _BRIDGE_URL:
        return tool.write({"path": path, "content": content})
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="") as f:
        f.write(content)


def sh(cmd):
    """Run `cmd` in the POSIX shell; return stdout+stderr, raise ShellError on non-zero exit."""
    argv = _SHELL.get("argv")
    if not argv:
        raise RuntimeError("sh(): " + _SHELL.get("error", "no POSIX shell available"))
    p = subprocess.run(
        [*argv, cmd],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        cwd=os.getcwd(),
    )
    out = p.stdout.decode("utf-8", errors="replace")
    if p.returncode != 0:
        raise ShellError(p.returncode, out)
    return out


class _EnvProxy:
    """Read/write environment helper hiding bridge credentials."""

    def __call__(self, key=None, default=None):
        if key is None:
            return {k: v for k, v in os.environ.items() if k not in _HIDDEN_ENV_KEYS}
        if key in _HIDDEN_ENV_KEYS:
            return default
        return os.environ.get(key, default)

    def get(self, key, default=None):
        return self(key, default)

    def set(self, key, value):
        if key in _HIDDEN_ENV_KEYS:
            raise PermissionError(f"cannot modify reserved variable {key}")
        os.environ[str(key)] = str(value)

    def __getitem__(self, key):
        if key in _HIDDEN_ENV_KEYS:
            raise KeyError(key)
        return os.environ[key]

    def __contains__(self, key):
        return key not in _HIDDEN_ENV_KEYS and key in os.environ

    def keys(self):
        return [k for k in os.environ.keys() if k not in _HIDDEN_ENV_KEYS]

    def items(self):
        return [(k, v) for k, v in os.environ.items() if k not in _HIDDEN_ENV_KEYS]


env = _EnvProxy()


def output(*args, image=None, media_type="image/png", label=None):
    """Emit formatted output values and/or attach an inline image to ToolOutput.images."""
    if args:
        print(*args)
    if image is not None:
        if isinstance(image, (bytes, bytearray)):
            b64 = base64.b64encode(bytes(image)).decode("ascii")
        elif isinstance(image, dict) and "data_base64" in image:
            _PENDING_IMAGES.append(image)
            return
        elif isinstance(image, str):
            b64 = image
        else:
            raise TypeError("output(image=...) expects bytes, base64 str, or image dict")
        att = {"media_type": media_type, "data_base64": b64}
        if label:
            att["label"] = str(label)
        _PENDING_IMAGES.append(att)


def log(*args):
    """Print a structured log line."""
    print("[log]", *args)


def phase(name):
    """Announce and track the active orchestration phase."""
    global _CURRENT_PHASE
    _CURRENT_PHASE = str(name)
    print(f"[phase] {_CURRENT_PHASE}")
    return _CURRENT_PHASE


def budget():
    """Return current concurrency and cell timing budget snapshot."""
    elapsed = max(0.0, time.monotonic() - _CELL_STARTED) if _CELL_STARTED else 0.0
    return {
        "max_concurrency": _MAX_CONCURRENCY,
        "elapsed_secs": round(elapsed, 3),
        "phase": _CURRENT_PHASE,
    }


def _annotation_to_schema(ann):
    if ann is int:
        return {"type": "integer"}
    if ann is float:
        return {"type": "number"}
    if ann is bool:
        return {"type": "boolean"}
    if ann is str:
        return {"type": "string"}
    if ann is list:
        return {"type": "array"}
    if ann is dict:
        return {"type": "object"}
    return {}


def _infer_parameters(fn):
    sig = inspect.signature(fn)
    props = {}
    required = []
    for pname, param in sig.parameters.items():
        if param.kind in (inspect.Parameter.VAR_POSITIONAL, inspect.Parameter.VAR_KEYWORD):
            continue
        props[pname] = _annotation_to_schema(param.annotation)
        if param.default is inspect.Parameter.empty:
            required.append(pname)
    schema = {"type": "object", "properties": props}
    if required:
        schema["required"] = required
    return schema


class _ToolProxy:
    """Dynamic proxy for `tool.<name>(args)` and `@tool` / `tool(fn)` kernel-tool registration."""

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)

        def _invoke(args=None, **kwargs):
            payload = {}
            if isinstance(args, dict):
                payload.update(args)
            elif args is not None:
                raise TypeError(f"tool.{name}() expects a dict or keyword arguments")
            if kwargs:
                payload.update(kwargs)
            resp = _bridge_post("/tool", {"name": name, "args": payload})
            if not resp.get("ok"):
                raise RuntimeError(resp.get("error") or f"tool.{name} failed")
            return resp.get("content", "")

        _invoke.__name__ = name
        return _invoke

    def __call__(self, fn=None, *, name=None, description=None, parameters=None):
        def _register(func):
            if not callable(func):
                raise TypeError("@tool expects a callable")
            tname = name or getattr(func, "__name__", None)
            if not tname:
                raise ValueError("@tool requires a tool name")
            doc = description if description is not None else (inspect.getdoc(func) or "")
            schema = parameters if parameters is not None else _infer_parameters(func)
            resp = _bridge_post(
                "/register_tool",
                {"name": tname, "description": doc, "parameters": schema},
            )
            if not resp.get("ok"):
                raise RuntimeError(resp.get("error") or f"failed to register @tool `{tname}`")
            _REGISTERED_TOOLS[tname] = func
            return func

        if fn is not None:
            return _register(fn)
        return _register


tool = _ToolProxy()


def completion(prompt, role=None):
    """Run a lightweight no-tool completion on `role` (defaults to host judge/smol chain)."""
    payload = {"prompt": str(prompt)}
    if role is not None:
        payload["role"] = str(role)
    resp = _bridge_post("/complete", payload)
    if not resp.get("ok"):
        raise RuntimeError(resp.get("error") or "completion failed")
    return CompletionResult(resp.get("text", ""), attempts=1)


def judge(prompt, role="judge"):
    """Run a lightweight judgment completion using `[roles] judge` (falling back to `@smol`)."""
    return completion(prompt, role=role or "judge")


def judge_batch(prompts, role="judge", max_concurrency=None):
    """Evaluate a sequence of prompts concurrently through `judge()`, preserving order."""
    items = list(prompts)
    if not items:
        return []
    limit = _effective_concurrency(max_concurrency)
    workers = min(limit, len(items))
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as ex:
        futs = [ex.submit(judge, p, role) for p in items]
        return [f.result() for f in futs]


class AgentHandle:
    """Handle to a background subagent spawned from an eval cell via `agent()`."""

    def __init__(self, meta):
        self.id = str(meta["id"])
        self.handle = self.id
        self.name = meta.get("name", self.id)
        self.profile = meta.get("profile", "scout")
        self._status = meta.get("status", "running")
        self._output = meta.get("output")
        self._yield_json = meta.get("yield_json")

    def _apply_result(self, res):
        if isinstance(res, dict):
            if "status" in res:
                self._status = res["status"]
            if "output" in res and res["output"] is not None:
                self._output = res["output"]
            if "yield_json" in res:
                self._yield_json = res["yield_json"]

    @property
    def status(self):
        if self._status == "running" and _BRIDGE_URL:
            try:
                resp = _bridge_post("/agent/wait", {"ids": [self.id], "timeout_ms": 0})
                results = resp.get("results") or []
                if results:
                    self._apply_result(results[0])
            except Exception:
                pass
        return self._status

    def done(self):
        return self.status != "running"

    def wait(self, timeout=None):
        timeout_ms = None if timeout is None else max(0, int(float(timeout) * 1000))
        resp = _bridge_post("/agent/wait", {"ids": [self.id], "timeout_ms": timeout_ms})
        if not resp.get("ok"):
            raise RuntimeError(resp.get("error") or f"wait failed for {self.id}")
        results = resp.get("results") or []
        res = results[0] if results else {"id": self.id, "status": self._status, "output": ""}
        self._apply_result(res)
        return res

    def send(self, msg):
        return tool.write({"path": f"agent://{self.id}", "content": str(msg)})

    def cancel(self):
        return tool.write({"path": f"proc://{self.id}/kill", "content": ""})

    def output(self):
        if self._output is not None and self._status != "running":
            return self._output
        return tool.read({"path": f"agent://{self.id}"})

    def __await__(self):
        async def _resolve():
            return self.wait()

        return _resolve().__await__()

    def __repr__(self):
        return f"AgentHandle(id={self.id!r}, profile={self.profile!r}, status={self._status!r})"


def agent(prompt, **opts):
    """Spawn a background subagent returning an `AgentHandle`. Unwaited agents auto-deliver."""
    resp = _bridge_post("/agent/spawn", {"prompt": str(prompt), "opts": opts})
    if not resp.get("ok"):
        raise RuntimeError(resp.get("error") or "agent() spawn failed")
    return AgentHandle(resp["agent"])


def wait(handles, timeout=None):
    """Wait on one `AgentHandle` (returns dict) or a list of handles (returns list of dicts)."""
    single = isinstance(handles, (AgentHandle, str))
    items = [handles] if single else list(handles)
    if not items:
        return []
    ids = [h.id if isinstance(h, AgentHandle) else str(h) for h in items]
    timeout_ms = None if timeout is None else max(0, int(float(timeout) * 1000))
    resp = _bridge_post("/agent/wait", {"ids": ids, "timeout_ms": timeout_ms})
    if not resp.get("ok"):
        raise RuntimeError(resp.get("error") or "wait() failed")
    results = resp.get("results") or []
    for h, res in zip(items, results):
        if isinstance(h, AgentHandle):
            h._apply_result(res)
    return results[0] if single and results else results


def _effective_concurrency(requested):
    host = max(1, int(_MAX_CONCURRENCY)) if _MAX_CONCURRENCY else 32
    if requested is None:
        return host
    req = max(1, int(requested))
    return min(req, host)


class WorkPool:
    """Concurrency-bounded worker pool capped by `[task] max_concurrency`."""

    def __init__(self, max_concurrency=None):
        self.max_concurrency = _effective_concurrency(max_concurrency)
        self._sem = threading.Semaphore(self.max_concurrency)
        self._lock = threading.Lock()
        self._active = 0
        self._peak_active = 0
        self._queued = 0
        self._completed = 0
        self._total = 0
        self._threads = []
        self._results = []
        self._errors = []

    def push(self, target, *args, **opts):
        with self._lock:
            idx = self._total
            self._total += 1
            self._queued += 1
            self._results.append(None)

        def _worker():
            self._sem.acquire()
            with self._lock:
                self._queued -= 1
                self._active += 1
                if self._active > self._peak_active:
                    self._peak_active = self._active
            try:
                if callable(target):
                    val = target(*args, **opts)
                elif isinstance(target, str):
                    h = agent(target, **opts)
                    val = h.wait()
                else:
                    raise TypeError("workpool.push() expects a callable or prompt string")
                with self._lock:
                    self._results[idx] = val
            except BaseException as exc:
                with self._lock:
                    self._errors.append(exc)
            finally:
                with self._lock:
                    self._active -= 1
                    self._completed += 1
                self._sem.release()

        t = threading.Thread(target=_worker, daemon=True)
        self._threads.append(t)
        t.start()
        return idx

    submit = push

    def map(self, target_or_prompts, items=None, **opts):
        if items is None and not callable(target_or_prompts):
            for prompt in target_or_prompts:
                self.push(prompt, **opts)
        elif callable(target_or_prompts) and items is not None:
            for item in items:
                self.push(target_or_prompts, item, **opts)
        else:
            raise TypeError("workpool.map() expects (prompts) or (fn, items)")
        return self.close()

    def status(self):
        with self._lock:
            return {
                "max_concurrency": self.max_concurrency,
                "active": self._active,
                "peak_active": self._peak_active,
                "queued": self._queued,
                "completed": self._completed,
                "total": self._total,
            }

    def peek(self):
        with self._lock:
            return [r for r in self._results if r is not None]

    def wait(self, timeout=None):
        deadline = None if timeout is None else time.monotonic() + float(timeout)
        for t in self._threads:
            rem = None if deadline is None else max(0.0, deadline - time.monotonic())
            t.join(rem)
        with self._lock:
            if self._errors:
                raise self._errors[0]
            return list(self._results)

    close = wait

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        if exc_type is None:
            self.close()
        return False


def workpool(max_concurrency=None):
    """Create a bounded `WorkPool` capped at `[task] max_concurrency`."""
    return WorkPool(max_concurrency=max_concurrency)


class _WindowFacade:
    """Window-scoped wrapper around `tool.desktop` (always enforces desktop approval gate)."""

    def __init__(self, target):
        self._base = {"window_id": target} if isinstance(target, int) else {"window": str(target)}

    def _call(self, action, **kwargs):
        args = {"action": action, **self._base}
        args.update({k: v for k, v in kwargs.items() if v is not None})
        return tool.desktop(args)

    def ax(self, max_depth=None, max_nodes=None):
        return self._call("ax", max_depth=max_depth, max_nodes=max_nodes)

    def find(self, role=None, title=None, value=None, limit=None):
        return self._call("find", role=role, title=title, value=value, limit=limit)

    def capture(self):
        return self._call("window_capture")

    def screenshot(self):
        return self._call("screenshot")

    def zoom(self, x, y, w, h):
        return self._call("zoom", x=x, y=y, w=w, h=h)

    def click(self, x, y, background=None, takeover=None):
        return self._call("click", x=x, y=y, background=background, takeover=takeover)

    def double_click(self, x, y, background=None, takeover=None):
        return self._call("double_click", x=x, y=y, background=background, takeover=takeover)

    def right_click(self, x, y, background=None, takeover=None):
        return self._call("right_click", x=x, y=y, background=background, takeover=takeover)

    def type_text(self, text, background=None, takeover=None):
        return self._call("type_text", text=text, background=background, takeover=takeover)

    def hotkey(self, keys, background=None, takeover=None):
        return self._call("hotkey", keys=keys, background=background, takeover=takeover)

    def scroll(self, delta_y, x=None, y=None):
        return self._call("scroll", delta_y=delta_y, x=x, y=y)


class _ComputerFacade:
    """`computer.*` prelude wrapping `tool.desktop` (never bypasses approval ladder)."""

    def _call(self, action, **kwargs):
        args = {"action": action}
        args.update({k: v for k, v in kwargs.items() if v is not None})
        return tool.desktop(args)

    def windows(self, app=None, title=None):
        text = self._call("list_windows")
        if not app and not title:
            return text
        lines = []
        for line in text.splitlines():
            low = line.lower()
            if app and app.lower() not in low:
                continue
            if title and title.lower() not in low:
                continue
            lines.append(line)
        return "\n".join(lines) if lines else "No matching windows found."

    def window(self, target):
        return _WindowFacade(target)

    def ax(self, window=None, window_id=None, max_depth=None, max_nodes=None):
        return self._call(
            "ax", window=window, window_id=window_id, max_depth=max_depth, max_nodes=max_nodes
        )

    def find(self, role=None, title=None, value=None, window=None, window_id=None, limit=None):
        return self._call(
            "find",
            role=role,
            title=title,
            value=value,
            window=window,
            window_id=window_id,
            limit=limit,
        )

    def displays(self):
        return self._call("displays")

    def capabilities(self):
        return self._call("capabilities")

    def clipboard_read(self):
        return self._call("clipboard_read")

    def clipboard_write(self, text):
        return self._call("clipboard_write", text=str(text))

    def screenshot(self, **kwargs):
        return self._call("screenshot", **kwargs)

    def window_capture(self, **kwargs):
        return self._call("window_capture", **kwargs)

    def zoom(self, x, y, w, h, **kwargs):
        return self._call("zoom", x=x, y=y, w=w, h=h, **kwargs)

    def apps_list(self):
        return self._call("apps_list")

    def open_app(self, target, args=None):
        return self._call("open_app", target=target, args=args)

    def click(self, x, y, **kwargs):
        return self._call("click", x=x, y=y, **kwargs)

    def double_click(self, x, y, **kwargs):
        return self._call("double_click", x=x, y=y, **kwargs)

    def right_click(self, x, y, **kwargs):
        return self._call("right_click", x=x, y=y, **kwargs)

    def move(self, x, y, **kwargs):
        return self._call("move", x=x, y=y, **kwargs)

    def scroll(self, delta_y, **kwargs):
        return self._call("scroll", delta_y=delta_y, **kwargs)

    def type_text(self, text, **kwargs):
        return self._call("type_text", text=text, **kwargs)

    def hotkey(self, keys, **kwargs):
        return self._call("hotkey", keys=keys, **kwargs)

    def el_read(self, ref, field="value"):
        return self._call("el_read", ref=ref, field=field)

    def set_value(self, ref, value):
        return self._call("set_value", ref=ref, value=value)

    def select_text(self, ref, start=None, end=None):
        return self._call("select_text", ref=ref, start=start, end=end)

    def invoke(self, ref):
        return self._call("invoke", ref=ref)

    def focus(self, ref):
        return self._call("focus", ref=ref)

    def el_type(self, ref, text):
        return self._call("el_type", ref=ref, text=text)

    def close(self):
        return self._call("close")


# ---- Magics runtime helpers & source transformer ----------------------------------------------


class CapturedIO:
    """Result object stored by `%%capture <var>`."""

    def __init__(self, stdout, stderr=""):
        self.stdout = stdout
        self.stderr = stderr

    def show(self):
        print(self.stdout, end="")

    def __str__(self):
        return self.stdout


def _magic_bang(cmd):
    out = sh(cmd)
    if out:
        print(out, end="" if out.endswith("\n") else "\n")


def _magic_bang_capture(cmd):
    return sh(cmd).rstrip("\r\n")


def _magic_cd(arg):
    target = os.path.expanduser(arg.strip() or "~")
    os.chdir(target)
    cwd = os.getcwd()
    print(cwd)
    return cwd


def _magic_pwd():
    cwd = os.getcwd()
    print(cwd)
    return cwd


def _magic_ls(arg):
    target = arg.strip() or "."
    entries = sorted(os.listdir(target))
    for item in entries:
        print(item)
    return entries


def _magic_env(arg):
    rest = arg.strip()
    if not rest:
        return env()
    if "=" in rest:
        k, v = rest.split("=", 1)
        env.set(k.strip(), v.strip())
        return env(k.strip())
    return env(rest)


def _magic_who(verbose=False):
    names = sorted(
        k for k in _GLOBALS.keys() if not k.startswith("_") and k not in _PRELUDE_KEYS
    )
    if not verbose:
        print(" ".join(names))
        return names
    for name in names:
        val = _GLOBALS[name]
        print(f"{name}\t{type(val).__name__}\t{repr(val)[:80]}")
    return names


def _magic_reset():
    for k in list(_GLOBALS.keys()):
        if k not in _PRELUDE_KEYS and not k.startswith("__"):
            del _GLOBALS[k]


def _magic_run(arg):
    parts = shlex.split(arg)
    if not parts:
        raise ValueError("%run requires a script path")
    script_path = parts[0]
    src = read(script_path)
    old_argv = sys.argv
    sys.argv = parts
    try:
        code = compile(src, script_path, "exec")
        exec(code, _GLOBALS)
    finally:
        sys.argv = old_argv


def _magic_pip(arg):
    parts = shlex.split(arg)
    if not parts:
        raise ValueError("%pip requires arguments")
    _watchdog_pause()
    try:
        p = subprocess.run(
            [sys.executable, "-m", "pip", *parts],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            cwd=os.getcwd(),
        )
        out = p.stdout.decode("utf-8", errors="replace")
        if out:
            print(out, end="" if out.endswith("\n") else "\n")
        if p.returncode != 0:
            raise ShellError(p.returncode, out)
    finally:
        _watchdog_resume()


def _magic_time(stmt):
    t0 = time.perf_counter()
    try:
        code = compile(stmt, "<%time>", "eval")
        val = eval(code, _GLOBALS)
    except SyntaxError:
        code = compile(stmt, "<%time>", "exec")
        exec(code, _GLOBALS)
        val = None
    dt = (time.perf_counter() - t0) * 1000.0
    print(f"Wall time: {dt:.2f} ms")
    return val


def _magic_timeit(arg, body=None):
    tokens = shlex.split(arg)
    number = 10
    repeat = 3
    rest = []
    i = 0
    while i < len(tokens):
        tok = tokens[i]
        if tok == "-n" and i + 1 < len(tokens):
            number = max(1, int(tokens[i + 1]))
            i += 2
        elif tok == "-r" and i + 1 < len(tokens):
            repeat = max(1, int(tokens[i + 1]))
            i += 2
        else:
            rest.append(tok)
            i += 1
    src = body if body is not None else " ".join(rest)
    code = compile(src, "<%timeit>", "exec")
    runs = []
    for _ in range(repeat):
        t0 = time.perf_counter()
        for _ in range(number):
            exec(code, _GLOBALS)
        runs.append((time.perf_counter() - t0) / number)
    best_ms = min(runs) * 1000.0
    print(f"{best_ms:.3f} ms per loop (best of {repeat} runs, {number} loops each)")


def _magic_cell_capture(arg, body):
    var = arg.strip()
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        code = compile(body, "<%%capture>", "exec")
        exec(code, _GLOBALS)
    cap = CapturedIO(buf.getvalue())
    if var:
        _GLOBALS[var] = cap


def _magic_cell_writefile(arg, body):
    parts = shlex.split(arg)
    append = False
    if parts and parts[0] == "-a":
        append = True
        parts = parts[1:]
    if not parts:
        raise ValueError("%%writefile requires a file path")
    target = parts[0]
    os.makedirs(os.path.dirname(os.path.abspath(target)), exist_ok=True)
    mode = "a" if append else "w"
    with open(target, mode, encoding="utf-8", newline="") as f:
        f.write(body)
    print(("Appending to " if append else "Writing ") + target)


def _magic_cell_time(body):
    t0 = time.perf_counter()
    code = compile(body, "<%%time>", "exec")
    exec(code, _GLOBALS)
    dt = (time.perf_counter() - t0) * 1000.0
    print(f"Wall time: {dt:.2f} ms")


_ASSIGN_BANG_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*!(.+)$")


def _update_triple_quote_state(line, state):
    i = 0
    n = len(line)
    while i < n:
        if state is None:
            if line[i] == "#":
                break
            if line[i : i + 3] in ('"""', "'''"):
                state = line[i : i + 3]
                i += 3
                continue
            if line[i] in ('"', "'"):
                q = line[i]
                i += 1
                while i < n and line[i] != q:
                    i += 2 if line[i] == "\\" else 1
                i += 1
                continue
            i += 1
        else:
            if line[i] == "\\":
                i += 2
                continue
            if line[i : i + 3] == state:
                state = None
                i += 3
                continue
            i += 1
    return state


def transform_magics(src):
    """Rewrite cell (`%%...`) and line (`%...`, `!cmd`) magics into executable Python."""
    raw_lines = src.splitlines()
    first_idx = next((i for i, l in enumerate(raw_lines) if l.strip()), None)
    if first_idx is None:
        return src
    first_line = raw_lines[first_idx]
    if first_line.startswith("%%"):
        head = first_line[2:].strip()
        name, _, arg = head.partition(" ")
        body = "\n".join(raw_lines[first_idx + 1 :])
        if name == "bash":
            return f"_hrn_magic_bang({body!r})"
        if name == "capture":
            return f"_hrn_magic_cell_capture({arg!r}, {body!r})"
        if name == "writefile":
            return f"_hrn_magic_cell_writefile({arg!r}, {body!r})"
        if name == "time":
            return f"_hrn_magic_cell_time({body!r})"
        if name == "timeit":
            return f"_hrn_magic_timeit({arg!r}, {body!r})"
        raise NameError(f"Unknown cell magic: %%{name}")

    out_lines = []
    quote_state = None
    for line in raw_lines:
        if quote_state is None and not line.startswith((" ", "\t")):
            if line.startswith("!"):
                out_lines.append(f"_hrn_magic_bang({line[1:]!r})")
                continue
            m = _ASSIGN_BANG_RE.match(line)
            if m:
                out_lines.append(f"{m.group(1)} = _hrn_magic_bang_capture({m.group(2)!r})")
                continue
            if line.startswith("%") and not line.startswith("%%"):
                out_lines.append(_transform_line_magic(line[1:]))
                continue
        out_lines.append(line)
        quote_state = _update_triple_quote_state(line, quote_state)
    return "\n".join(out_lines)


def _transform_line_magic(spec):
    name, _, arg = spec.strip().partition(" ")
    arg = arg.strip()
    if name == "cd":
        return f"_hrn_magic_cd({arg!r})"
    if name == "pwd":
        return "_hrn_magic_pwd()"
    if name == "ls":
        return f"_hrn_magic_ls({arg!r})"
    if name in ("env", "set_env"):
        return f"_hrn_magic_env({arg!r})"
    if name == "who":
        return "_hrn_magic_who(False)"
    if name == "whos":
        return "_hrn_magic_who(True)"
    if name == "reset":
        return "_hrn_magic_reset()"
    if name == "run":
        return f"_hrn_magic_run({arg!r})"
    if name == "pip":
        return f"_hrn_magic_pip({arg!r})"
    if name == "time":
        return f"_hrn_magic_time({arg!r})"
    if name == "timeit":
        return f"_hrn_magic_timeit({arg!r})"
    if name == "load":
        return f"exec(compile(read({arg!r}), {arg!r}, 'exec'), globals())"
    raise NameError(f"Unknown magic: %{name}")


# ---- Lifecycle hooks used by kernel.py --------------------------------------------------------


def init_prelude(g, loop, shell_desc, bridge_url, bridge_token, computer_enabled, max_concurrency):
    global _BRIDGE_URL, _BRIDGE_TOKEN, _MAX_CONCURRENCY, _COMPUTER_ENABLED
    global _SHELL, _GLOBALS, _LOOP, _PRELUDE_KEYS
    _GLOBALS = g
    _LOOP = loop
    _SHELL = shell_desc
    _BRIDGE_URL = bridge_url or ""
    _BRIDGE_TOKEN = bridge_token or ""
    _COMPUTER_ENABLED = bool(computer_enabled)
    _MAX_CONCURRENCY = max(1, int(max_concurrency)) if max_concurrency else 32

    bindings = {
        "read": read,
        "write": write,
        "sh": sh,
        "ShellError": ShellError,
        "env": env,
        "output": output,
        "log": log,
        "phase": phase,
        "budget": budget,
        "tool": tool,
        "completion": completion,
        "judge": judge,
        "judge_batch": judge_batch,
        "agent": agent,
        "AgentHandle": AgentHandle,
        "wait": wait,
        "workpool": workpool,
        "WorkPool": WorkPool,
        "_hrn_magic_bang": _magic_bang,
        "_hrn_magic_bang_capture": _magic_bang_capture,
        "_hrn_magic_cd": _magic_cd,
        "_hrn_magic_pwd": _magic_pwd,
        "_hrn_magic_ls": _magic_ls,
        "_hrn_magic_env": _magic_env,
        "_hrn_magic_who": _magic_who,
        "_hrn_magic_reset": _magic_reset,
        "_hrn_magic_run": _magic_run,
        "_hrn_magic_pip": _magic_pip,
        "_hrn_magic_time": _magic_time,
        "_hrn_magic_timeit": _magic_timeit,
        "_hrn_magic_cell_capture": _magic_cell_capture,
        "_hrn_magic_cell_writefile": _magic_cell_writefile,
        "_hrn_magic_cell_time": _magic_cell_time,
    }
    if _COMPUTER_ENABLED:
        bindings["computer"] = _ComputerFacade()
    g.update(bindings)
    _PRELUDE_KEYS = set(g.keys())


def begin_cell():
    global _CELL_STARTED
    _CELL_STARTED = time.monotonic()
    _PENDING_IMAGES.clear()


def drain_images():
    imgs = list(_PENDING_IMAGES)
    _PENDING_IMAGES.clear()
    return imgs


def invoke_registered_tool(name, args):
    fn = _REGISTERED_TOOLS.get(name)
    if fn is None:
        raise RuntimeError(f"eval kernel tool `{name}` is not registered in this kernel")
    if isinstance(args, dict):
        sig = inspect.signature(fn)
        params = list(sig.parameters.values())
        has_var_kw = any(p.kind == inspect.Parameter.VAR_KEYWORD for p in params)
        param_names = {p.name for p in params}
        if has_var_kw or (args and set(args.keys()).issubset(param_names)) or (not args and not params):
            res = fn(**args)
        elif len(params) == 1:
            res = fn(args)
        else:
            res = fn(**args)
    else:
        res = fn(args)
    if inspect.iscoroutine(res):
        res = _LOOP.run_until_complete(res)
    if res is None:
        return ""
    if isinstance(res, str):
        return res
    try:
        return json.dumps(res)
    except TypeError:
        return str(res)
