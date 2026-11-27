// Embedded JavaScript (Bun/Node) prelude for `hrn` eval kernels (stdlib `node:http` only).
import fs from "node:fs";
import http from "node:http";
import path from "node:path";

let bridgeUrl = "";
let bridgeToken = "";
let maxConcurrency = 32;
let computerEnabled = false;
let currentPhase = null;
let cellStartedMs = 0;
const pendingImages = [];
const registeredTools = new Map();
const hiddenEnvKeys = new Set(["HRN_EVAL_BRIDGE", "HRN_EVAL_TOKEN"]);

function bridgePost(route, payload = {}) {
  if (!bridgeUrl) {
    return Promise.reject(new Error("eval bridge is not configured"));
  }
  const target = new URL(route, bridgeUrl);
  const body = Buffer.from(JSON.stringify(payload), "utf8");
  return new Promise((resolve, reject) => {
    const req = http.request(
      {
        hostname: target.hostname,
        port: target.port,
        path: target.pathname,
        method: "POST",
        headers: {
          Authorization: `Bearer ${bridgeToken}`,
          "Content-Type": "application/json",
          "Content-Length": body.length,
        },
      },
      (res) => {
        const chunks = [];
        res.on("data", (chunk) => chunks.push(chunk));
        res.on("end", () => {
          const text = Buffer.concat(chunks).toString("utf8");
          let parsed = {};
          try {
            parsed = text ? JSON.parse(text) : {};
          } catch {
            parsed = { ok: false, error: text || `HTTP ${res.statusCode}` };
          }
          if ((res.statusCode ?? 500) >= 400) {
            reject(
              new Error(
                `eval bridge error on ${route}: ${parsed.error ?? `HTTP ${res.statusCode}`}`,
              ),
            );
            return;
          }
          resolve(parsed);
        });
      },
    );
    req.on("error", (err) => reject(new Error(`eval bridge connection failed on ${route}: ${err.message}`)));
    req.write(body);
    req.end();
  });
}

function effectiveConcurrency(requested) {
  const host = Math.max(1, Number(maxConcurrency) || 32);
  if (requested === undefined || requested === null) return host;
  return Math.min(Math.max(1, Number(requested) || 1), host);
}

async function callHostTool(name, args = {}) {
  const resp = await bridgePost("/tool", { name, args: args ?? {} });
  if (!resp.ok) {
    throw new Error(resp.error ?? `tool.${name} failed`);
  }
  return resp.content ?? "";
}

async function registerKernelTool(first, second, third) {
  let name;
  let fn;
  let opts = {};
  if (typeof first === "string") {
    name = first;
    fn = second;
    opts = third ?? {};
  } else if (typeof first === "function") {
    fn = first;
    opts = second ?? {};
    name = opts.name || fn.name;
  } else {
    throw new TypeError("tool(fn, opts?) or tool(name, fn, opts?) expects a function");
  }
  if (typeof fn !== "function" || !name) {
    throw new TypeError("tool registration requires a named function");
  }
  const resp = await bridgePost("/register_tool", {
    name,
    description: opts.description ?? "",
    parameters: opts.parameters ?? { type: "object", properties: {} },
  });
  if (!resp.ok) {
    throw new Error(resp.error ?? `failed to register tool \`${name}\``);
  }
  registeredTools.set(name, fn);
  return fn;
}

const toolProxy = new Proxy(registerKernelTool, {
  get(target, prop) {
    if (prop === "register") return registerKernelTool;
    if (typeof prop === "symbol") return target[prop];
    return (args = {}) => callHostTool(String(prop), args);
  },
});

function envHelper(key, defaultValue = undefined) {
  if (key === undefined) {
    const out = {};
    for (const [k, v] of Object.entries(process.env)) {
      if (!hiddenEnvKeys.has(k)) out[k] = v;
    }
    return out;
  }
  if (hiddenEnvKeys.has(key)) return defaultValue;
  return process.env[key] ?? defaultValue;
}
envHelper.get = envHelper;
envHelper.set = (key, value) => {
  if (hiddenEnvKeys.has(key)) throw new Error(`cannot modify reserved variable ${key}`);
  process.env[String(key)] = String(value);
};

class AgentHandle {
  constructor(meta) {
    this.id = String(meta.id);
    this.handle = this.id;
    this.name = meta.name ?? this.id;
    this.profile = meta.profile ?? "scout";
    this._status = meta.status ?? "running";
    this._output = meta.output ?? null;
    this._yieldJson = meta.yield_json ?? null;
  }

  _apply(res) {
    if (!res || typeof res !== "object") return;
    if (res.status !== undefined) this._status = res.status;
    if (res.output !== undefined && res.output !== null) this._output = res.output;
    if (res.yield_json !== undefined) this._yieldJson = res.yield_json;
  }

  get status() {
    return this._status;
  }

  done() {
    return this._status !== "running";
  }

  async wait(timeoutSecs = null) {
    const timeout_ms =
      timeoutSecs === null || timeoutSecs === undefined
        ? null
        : Math.max(0, Math.floor(Number(timeoutSecs) * 1000));
    const resp = await bridgePost("/agent/wait", { ids: [this.id], timeout_ms });
    if (!resp.ok) throw new Error(resp.error ?? `wait failed for ${this.id}`);
    const res = (resp.results ?? [])[0] ?? { id: this.id, status: this._status, output: "" };
    this._apply(res);
    return res;
  }

  send(msg) {
    return callHostTool("write", { path: `agent://${this.id}`, content: String(msg) });
  }

  cancel() {
    return callHostTool("write", { path: `proc://${this.id}/kill`, content: "" });
  }

  async output() {
    if (this._output !== null && this._status !== "running") return this._output;
    return callHostTool("read", { path: `agent://${this.id}` });
  }
}

async function agentFn(prompt, opts = {}) {
  const resp = await bridgePost("/agent/spawn", { prompt: String(prompt), opts });
  if (!resp.ok) throw new Error(resp.error ?? "agent() spawn failed");
  return new AgentHandle(resp.agent);
}

async function waitFn(handles, timeoutSecs = null) {
  const single = handles instanceof AgentHandle || typeof handles === "string";
  const items = single ? [handles] : Array.from(handles);
  if (items.length === 0) return [];
  const ids = items.map((h) => (h instanceof AgentHandle ? h.id : String(h)));
  const timeout_ms =
    timeoutSecs === null || timeoutSecs === undefined
      ? null
      : Math.max(0, Math.floor(Number(timeoutSecs) * 1000));
  const resp = await bridgePost("/agent/wait", { ids, timeout_ms });
  if (!resp.ok) throw new Error(resp.error ?? "wait() failed");
  const results = resp.results ?? [];
  items.forEach((h, i) => {
    if (h instanceof AgentHandle && results[i]) h._apply(results[i]);
  });
  return single ? results[0] : results;
}

async function completionFn(prompt, role = null) {
  const payload = { prompt: String(prompt) };
  if (role) payload.role = String(role);
  const resp = await bridgePost("/complete", payload);
  if (!resp.ok) throw new Error(resp.error ?? "completion failed");
  return resp.text ?? "";
}

function judgeFn(prompt, role = "judge") {
  return completionFn(prompt, role || "judge");
}

async function judgeBatchFn(prompts, role = "judge", opts = {}) {
  const items = Array.from(prompts);
  const pool = workpoolFn(opts.maxConcurrency ?? opts.max_concurrency);
  for (const p of items) pool.push(() => judgeFn(p, role));
  return pool.close();
}

class WorkPool {
  constructor(requested) {
    this.maxConcurrency = effectiveConcurrency(requested);
    this.max_concurrency = this.maxConcurrency;
    this._active = 0;
    this._peakActive = 0;
    this._completed = 0;
    this._queue = [];
    this._results = [];
    this._promises = [];
  }

  _pump() {
    while (this._active < this.maxConcurrency && this._queue.length > 0) {
      const job = this._queue.shift();
      this._active++;
      if (this._active > this._peakActive) this._peakActive = this._active;
      Promise.resolve()
        .then(job.run)
        .then((val) => {
          this._results[job.idx] = val;
          job.resolve(val);
        })
        .catch((err) => job.reject(err))
        .finally(() => {
          this._active--;
          this._completed++;
          this._pump();
        });
    }
  }

  push(target, opts = {}) {
    const idx = this._results.length;
    this._results.push(undefined);
    const run =
      typeof target === "function"
        ? () => target()
        : async () => {
            const h = await agentFn(String(target), opts);
            return h.wait();
          };
    const p = new Promise((resolve, reject) => {
      this._queue.push({ idx, run, resolve, reject });
    });
    this._promises.push(p);
    this._pump();
    return p;
  }

  async map(targetOrPrompts, items = null, opts = {}) {
    if (items === null && typeof targetOrPrompts !== "function") {
      for (const prompt of targetOrPrompts) this.push(prompt, opts);
    } else if (typeof targetOrPrompts === "function" && items !== null) {
      for (const item of items) this.push(() => targetOrPrompts(item));
    } else {
      throw new TypeError("workpool.map() expects (prompts) or (fn, items)");
    }
    return this.close();
  }

  status() {
    return {
      max_concurrency: this.maxConcurrency,
      active: this._active,
      peak_active: this._peakActive,
      queued: this._queue.length,
      completed: this._completed,
      total: this._results.length,
    };
  }

  peek() {
    return this._results.filter((r) => r !== undefined);
  }

  async close() {
    await Promise.all(this._promises);
    return [...this._results];
  }

  wait() {
    return this.close();
  }
}

function workpoolFn(maxConc = null) {
  return new WorkPool(maxConc);
}

function buildComputerFacade() {
  const call = (action, extra = {}) => callHostTool("desktop", { action, ...extra });
  return {
    windows: (opts = {}) => call("list_windows", opts),
    window: (target) => {
      const base = typeof target === "number" ? { window_id: target } : { window: String(target) };
      return {
        ax: (opts = {}) => call("ax", { ...base, ...opts }),
        find: (opts = {}) => call("find", { ...base, ...opts }),
        capture: () => call("window_capture", base),
        screenshot: () => call("screenshot", base),
        zoom: (opts = {}) => call("zoom", { ...base, ...opts }),
        click: (x, y, opts = {}) => call("click", { ...base, x, y, ...opts }),
        double_click: (x, y, opts = {}) => call("double_click", { ...base, x, y, ...opts }),
        right_click: (x, y, opts = {}) => call("right_click", { ...base, x, y, ...opts }),
        type_text: (text, opts = {}) => call("type_text", { ...base, text, ...opts }),
        hotkey: (keys, opts = {}) => call("hotkey", { ...base, keys, ...opts }),
        scroll: (delta_y, opts = {}) => call("scroll", { ...base, delta_y, ...opts }),
      };
    },
    ax: (opts = {}) => call("ax", opts),
    find: (opts = {}) => call("find", opts),
    displays: () => call("displays"),
    capabilities: () => call("capabilities"),
    clipboard_read: () => call("clipboard_read"),
    clipboard_write: (text) => call("clipboard_write", { text: String(text) }),
    screenshot: (opts = {}) => call("screenshot", opts),
    window_capture: (opts = {}) => call("window_capture", opts),
    zoom: (opts = {}) => call("zoom", opts),
    apps_list: () => call("apps_list"),
    open_app: (target, args = []) => call("open_app", { target, args }),
    click: (x, y, opts = {}) => call("click", { x, y, ...opts }),
    double_click: (x, y, opts = {}) => call("double_click", { x, y, ...opts }),
    right_click: (x, y, opts = {}) => call("right_click", { x, y, ...opts }),
    move: (x, y, opts = {}) => call("move", { x, y, ...opts }),
    scroll: (delta_y, opts = {}) => call("scroll", { delta_y, ...opts }),
    type_text: (text, opts = {}) => call("type_text", { text, ...opts }),
    hotkey: (keys, opts = {}) => call("hotkey", { keys, ...opts }),
    el_read: (ref, field = "value") => call("el_read", { ref, field }),
    set_value: (ref, value) => call("set_value", { ref, value }),
    select_text: (ref, opts = {}) => call("select_text", { ref, ...opts }),
    invoke: (ref) => call("invoke", { ref }),
    focus: (ref) => call("focus", { ref }),
    el_type: (ref, text) => call("el_type", { ref, text }),
    close: () => call("close"),
  };
}

export function initPrelude(opts) {
  bridgeUrl = opts.bridgeUrl || "";
  bridgeToken = opts.bridgeToken || "";
  computerEnabled = Boolean(opts.computerEnabled);
  maxConcurrency = Math.max(1, Number(opts.maxConcurrency) || 32);

  globalThis.read = (file) => {
    if (typeof file === "string" && file.includes("://") && bridgeUrl) {
      return callHostTool("read", { path: file });
    }
    return fs.readFileSync(file, "utf8");
  };
  globalThis.write = (file, content) => {
    if (typeof file === "string" && file.includes("://") && bridgeUrl) {
      return callHostTool("write", { path: file, content });
    }
    fs.mkdirSync(path.dirname(path.resolve(file)), { recursive: true });
    fs.writeFileSync(file, content);
  };
  globalThis.env = envHelper;
  globalThis.output = (...args) => {
    const last = args.length > 0 ? args[args.length - 1] : null;
    if (last && typeof last === "object" && typeof last.data_base64 === "string") {
      pendingImages.push(last);
      if (args.length > 1) console.log(...args.slice(0, -1));
      return;
    }
    if (args.length > 0) console.log(...args);
  };
  globalThis.log = (...args) => console.log("[log]", ...args);
  globalThis.phase = (name) => {
    currentPhase = String(name);
    console.log(`[phase] ${currentPhase}`);
    return currentPhase;
  };
  globalThis.budget = () => ({
    max_concurrency: maxConcurrency,
    elapsed_secs: cellStartedMs ? Number(((Date.now() - cellStartedMs) / 1000).toFixed(3)) : 0,
    phase: currentPhase,
  });
  globalThis.tool = toolProxy;
  globalThis.completion = completionFn;
  globalThis.judge = judgeFn;
  globalThis.judgeBatch = judgeBatchFn;
  globalThis.judge_batch = judgeBatchFn;
  globalThis.agent = agentFn;
  globalThis.AgentHandle = AgentHandle;
  globalThis.wait = waitFn;
  globalThis.workpool = workpoolFn;
  globalThis.WorkPool = WorkPool;
  if (computerEnabled) {
    globalThis.computer = buildComputerFacade();
  }
}

export function beginCell() {
  cellStartedMs = Date.now();
  pendingImages.length = 0;
}

export function drainImages() {
  return pendingImages.splice(0, pendingImages.length);
}

export async function invokeRegisteredTool(name, args) {
  const fn = registeredTools.get(name);
  if (!fn) {
    throw new Error(`eval kernel tool \`${name}\` is not registered in this kernel`);
  }
  const res = await fn(args ?? {});
  if (res === undefined || res === null) return "";
  if (typeof res === "string") return res;
  return JSON.stringify(res);
}
