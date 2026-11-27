"""hrn eval kernel (Python).

Protocol: one JSON request per stdin line: {"id", "op": "eval"|"call_tool", ...}. Everything the
cell prints (stdout and stderr, merged in order) flows to stdout as it happens; the reply is one
line MARK + JSON {"id", "value", "error", "images"} written after the cell finishes.
"""

import ast
import asyncio
import importlib.util
import inspect
import io
import json
import linecache
import os
import sys
import traceback

MARK = "\x01HRN\x01"

_in = sys.stdin
_out = sys.stdout
_in.reconfigure(encoding="utf-8", errors="replace")
_out.reconfigure(encoding="utf-8", errors="replace", newline="\n", line_buffering=True)
sys.stderr = _out
os.dup2(_out.fileno(), 2)  # native writes to stderr stay ordered with stdout
sys.stdin = io.StringIO("")  # cells must not consume the request stream

# Pop bridge credentials from os.environ BEFORE loading prelude or executing any cell so child
# subprocesses (!cmd / sh / %pip) and os.environ inspections can never inherit or leak the token.
_bridge_url = os.environ.pop("HRN_EVAL_BRIDGE", "")
_bridge_token = os.environ.pop("HRN_EVAL_TOKEN", "")
_computer_enabled = os.environ.pop("HRN_EVAL_COMPUTER", "0") == "1"
_max_concurrency = int(os.environ.pop("HRN_EVAL_MAX_CONCURRENCY", "32") or "32")
_shell = json.loads(os.environ.get("HRN_EVAL_SHELL") or "{}")

_prelude_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "prelude.py")
_spec = importlib.util.spec_from_file_location("_hrn_eval_prelude", _prelude_path)
_prelude = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_prelude)

G = {
    "__name__": "__main__",
    "__doc__": None,
}
FLAGS = ast.PyCF_ALLOW_TOP_LEVEL_AWAIT
loop = asyncio.new_event_loop()
asyncio.set_event_loop(loop)
_prelude.init_prelude(
    G,
    loop,
    _shell,
    _bridge_url,
    _bridge_token,
    _computer_enabled,
    _max_concurrency,
)
# Only expose cwd on sys.path AFTER prelude & stdlib imports complete so workspace files
# (e.g. ./subprocess.py) can never hijack kernel bootstrap imports.
sys.path[0] = ""
_hidden = (
    os.path.abspath(__file__),
    os.path.abspath(_prelude_path),
    os.path.dirname(asyncio.__file__),
)
_last_req_cwd = None


def run_cell(src, fname):
    """Execute `src` in G; return the value of a trailing expression (REPL style) or None."""
    transformed = _prelude.transform_magics(src)
    linecache.cache[fname] = (len(transformed), None, transformed.splitlines(True), fname)
    tree = compile(transformed, fname, "exec", flags=ast.PyCF_ONLY_AST | FLAGS)
    tail = None
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        tail = ast.Expression(tree.body.pop().value)
    codes = [compile(tree, fname, "exec", flags=FLAGS)]
    if tail is not None:
        codes.append(compile(tail, fname, "eval", flags=FLAGS))
    value = None
    for code in codes:
        value = eval(code, G)
        if code.co_flags & inspect.CO_COROUTINE:
            value = loop.run_until_complete(value)
    return value if tail is not None else None


def format_error(exc):
    frames = [
        f
        for f in traceback.extract_tb(exc.__traceback__)
        if not f.filename.startswith(_hidden)
    ]
    text = ""
    if frames:
        text = "Traceback (most recent call last):\n" + "".join(traceback.format_list(frames))
    return text + "".join(traceback.format_exception_only(type(exc), exc))


def handle(req, n):
    global _last_req_cwd
    reply = {"id": req["id"], "value": None, "error": None, "images": []}
    _prelude.begin_cell()
    try:
        req_cwd = req.get("cwd")
        if req_cwd and req_cwd != _last_req_cwd:
            os.chdir(req_cwd)
            _last_req_cwd = req_cwd
        op = req.get("op", "eval")
        if op == "call_tool":
            reply["value"] = _prelude.invoke_registered_tool(req["name"], req.get("args") or {})
        else:
            value = run_cell(req["code"], f"<eval-{n}>")
            if value is not None:
                reply["value"] = repr(value)
    except BaseException as exc:  # SystemExit/KeyboardInterrupt must not kill the kernel
        reply["error"] = format_error(exc)
    reply["images"] = _prelude.drain_images()
    _out.flush()
    _out.write(MARK + json.dumps(reply) + "\n")
    _out.flush()


for count, line in enumerate(_in):
    if line.strip():
        handle(json.loads(line), count)
