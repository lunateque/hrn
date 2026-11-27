// hrn eval kernel (Bun).
//
// Protocol: one JSON request per stdin line: {id, op, code, cwd, name, args}. Everything the cell
// prints (console.* and process.stdout/stderr, merged in order) goes to stdout as it happens; the
// reply is one line MARK + JSON {id, value, error, images} written after the cell finishes.
//
// A cell runs as the body of an async function, so top-level `await` and `return` work. To keep
// state across cells, top-level declarations are rewritten into assignments to globals.
import fs from "node:fs";
import path from "node:path";
import util from "node:util";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { beginCell, drainImages, initPrelude, invokeRegisteredTool } from "./prelude.js";

const MARK = "\x01HRN\x01";
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;

function writeOut(text) {
  const buf = Buffer.from(text);
  for (let off = 0; off < buf.length; ) {
    try {
      off += fs.writeSync(1, buf, off);
    } catch (e) {
      if (e.code !== "EAGAIN") throw e;
    }
  }
}
const emit = (...args) => writeOut(util.format(...args) + "\n");
for (const name of ["log", "info", "debug", "warn", "error"]) console[name] = emit;
const rawWrite = (chunk) => (writeOut(typeof chunk === "string" ? chunk : Buffer.from(chunk).toString()), true);
process.stdout.write = rawWrite;
process.stderr.write = rawWrite;
globalThis.print = emit;
for (const event of ["unhandledRejection", "uncaughtException"]) {
  process.on(event, (err) => emit(`${event}: ${fmtError(err)}`));
}

// Pop bridge credentials from process.env before any cell or subprocess executes.
const bridgeUrl = process.env.HRN_EVAL_BRIDGE || "";
const bridgeToken = process.env.HRN_EVAL_TOKEN || "";
const computerEnabled = process.env.HRN_EVAL_COMPUTER === "1";
const maxConcurrency = Number(process.env.HRN_EVAL_MAX_CONCURRENCY || "32") || 32;
delete process.env.HRN_EVAL_BRIDGE;
delete process.env.HRN_EVAL_TOKEN;
delete process.env.HRN_EVAL_COMPUTER;
delete process.env.HRN_EVAL_MAX_CONCURRENCY;

const shell = JSON.parse(process.env.HRN_EVAL_SHELL || "{}");

initPrelude({ bridgeUrl, bridgeToken, computerEnabled, maxConcurrency });

globalThis.sh = (cmd) => {
  if (!shell.argv) throw new Error("sh(): " + (shell.error ?? "no POSIX shell available"));
  const run = Bun.spawnSync([...shell.argv, `{ ${cmd}\n} 2>&1`], {
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
    cwd: process.cwd(),
  });
  const out = run.stdout.toString() + run.stderr.toString();
  if (run.exitCode !== 0) {
    const err = new Error(`command exited with code ${run.exitCode}\n${out}`);
    err.code = run.exitCode;
    err.output = out;
    throw err;
  }
  return out;
};
globalThis.__import = async (spec) => {
  if (spec.startsWith(".") || path.isAbsolute(spec)) {
    return import(pathToFileURL(path.resolve(process.cwd(), spec)).href);
  }
  let target = spec;
  try {
    target = Bun.resolveSync(spec, process.cwd());
  } catch {
    // Not resolvable from the cwd: the plain import below reports the real error.
  }
  return import(target);
};

// ---- source scanning -------------------------------------------------------------------------

function skipString(src, i) {
  const quote = src[i++];
  while (i < src.length && src[i] !== quote && src[i] !== "\n") i += src[i] === "\\" ? 2 : 1;
  return i + 1;
}

function skipRegex(src, i) {
  let inClass = false;
  for (i++; i < src.length && src[i] !== "\n"; i++) {
    const c = src[i];
    if (c === "\\") i++;
    else if (c === "[") inClass = true;
    else if (c === "]") inClass = false;
    else if (c === "/" && !inClass) break;
  }
  return i + 1;
}

function skipTemplate(src, i) {
  while (i < src.length) {
    const c = src[i];
    if (c === "\\") i += 2;
    else if (c === "`") return i + 1;
    else if (c === "$" && src[i + 1] === "{") i = skipBraces(src, i + 2);
    else i++;
  }
  return i;
}

function skipBraces(src, i) {
  for (let depth = 1; i < src.length && depth > 0; ) {
    const c = src[i];
    if (c === "`") i = skipTemplate(src, i + 1);
    else if (c === '"' || c === "'") i = skipString(src, i);
    else {
      if (c === "{") depth++;
      else if (c === "}") depth--;
      i++;
    }
  }
  return i;
}

/** Offsets where a top-level statement begins: after a newline or a depth-0 `;`, outside strings and comments. */
function topLevelStarts(src) {
  const starts = [];
  let depth = 0;
  let lineStart = true;
  let prev = "";
  for (let i = 0; i < src.length; ) {
    const c = src[i];
    if (c === "\n") {
      lineStart = true;
      i++;
      continue;
    }
    if (lineStart && (c === " " || c === "\t" || c === "\r")) {
      i++;
      continue;
    }
    const comment = c === "/" && (src[i + 1] === "/" || src[i + 1] === "*");
    if (lineStart && depth === 0 && !comment) starts.push(i);
    lineStart = false;
    if (comment) {
      if (src[i + 1] === "/") {
        while (i < src.length && src[i] !== "\n") i++;
      } else {
        const end = src.indexOf("*/", i + 2);
        i = end < 0 ? src.length : end + 2;
      }
      continue;
    }
    if (c === '"' || c === "'") i = skipString(src, i);
    else if (c === "`") i = skipTemplate(src, i + 1);
    else if (c === "/" && (prev === "" || "(,=:[!&|?{};".includes(prev))) i = skipRegex(src, i);
    else {
      if ("({[".includes(c)) depth++;
      else if (")}]".includes(c)) depth--;
      i++;
    }
    if (src[i - 1].trim()) prev = src[i - 1];
    if (c === ";" && depth === 0) lineStart = true;
  }
  return starts;
}

// ---- rewriting top-level declarations into global assignments --------------------------------

const IMPORT = /import\s+(?:([^"'\n]*?)\s+from\s+)?(["'][^"'\n]+["'])[ \t]*;?/y;
const DECL = /(?:const|let|var)\s+/y;
const FUNC = /(async\s+)?function(\s*\*)?\s*([\w$]+)/y;
const CLASS = /class\s+([\w$]+)/y;
const NAMES_ONLY = /([\w$]+(?:\s*,\s*[\w$]+)*)\s*;?[ \t]*(?:\/\/.*)?(?=\n|$)/y;

function importStatement(clause, spec) {
  const load = `await __import(${spec})`;
  if (!clause) return `${load};`;
  const m = /^(?:([\w$]+)\s*,?\s*)?(?:\*\s*as\s+([\w$]+)|\{([^}]*)\})?$/.exec(clause.trim());
  if (!m || (!m[1] && !m[2] && m[3] === undefined)) return null;
  const [, def, ns, named] = m;
  if (ns && !def) return `${ns} = ${load};`;
  if (!named && !ns) return `${def} = (${load}).default;`;
  const fields = (named ?? "").split(",").map((f) => f.trim().replace(/\s+as\s+/, ": ")).filter(Boolean);
  if (def) fields.unshift(`default: ${def}`);
  const target = ns ? `${ns} = ${load}, ` : "";
  return `${target}0, { ${fields.join(", ")} } = ${ns ? ns : load};`;
}

function matchAt(re, src, pos) {
  re.lastIndex = pos;
  return re.exec(src);
}

/** The replacement for the top-level statement head at `pos`: [matched length, new text]. */
function rewriteHead(src, pos) {
  let m = matchAt(IMPORT, src, pos);
  if (m) {
    const text = importStatement(m[1], m[2]);
    if (text) return [m[0].length, text];
  }
  if ((m = matchAt(DECL, src, pos))) {
    const next = src[pos + m[0].length];
    if (next === "{" || next === "[") return [m[0].length, "0, "];
    const names = matchAt(NAMES_ONLY, src, pos + m[0].length);
    if (names) return [m[0].length + names[0].length, names[1].split(/\s*,\s*/).join(" = ") + " = undefined;"];
    return [m[0].length, ""];
  }
  if ((m = matchAt(FUNC, src, pos))) return [m[0].length, `${m[3]} = ${m[1] ?? ""}function${m[2] ?? ""} ${m[3]}`];
  if ((m = matchAt(CLASS, src, pos))) return [m[0].length, `${m[1]} = class ${m[1]}`];
  return null;
}

function rewrite(src) {
  let out = "";
  let last = 0;
  for (const pos of topLevelStarts(src)) {
    const head = rewriteHead(src, pos);
    if (!head) continue;
    out += src.slice(last, pos) + head[1];
    last = pos + head[0];
  }
  return out + src.slice(last);
}

// ---- running a cell --------------------------------------------------------------------------

const STATEMENT = /^(?:const|let|var|function|async\s+function|class|import|export|if|for|while|do|switch|try|return|throw|break|continue|with)\b/;
const ASSIGNMENT = /^[\w$.\[\]"']+\s*(?:[-+*\/%&|^]|\*\*|\?\?|\|\||&&|<<|>>>?)?=(?![=>])/;

const compiles = (body) => {
  try {
    new AsyncFunction(body);
    return true;
  } catch {
    return false;
  }
};

/** Split off a trailing expression statement (its value is the cell's result), if there is one. */
function splitTail(src) {
  for (const pos of topLevelStarts(src).slice(-12).reverse()) {
    const tail = src.slice(pos).replace(/[;\s]+$/, "");
    if (STATEMENT.test(tail) || ASSIGNMENT.test(tail)) return [src, null];
    if (tail && compiles(`return (${tail}\n)`) && compiles(rewrite(src.slice(0, pos)))) {
      return [src.slice(0, pos), tail];
    }
  }
  return [src, null];
}

async function runCell(src) {
  const [head, tail] = splitTail(src);
  const body = rewrite(head) + (tail === null ? "" : `\nreturn (${tail}\n)`);
  return new AsyncFunction(body)();
}

function fmtError(err) {
  if (!(err instanceof Error)) return util.inspect(err);
  const own = import.meta.file;
  return String(err.stack ?? err.message)
    .split("\n")
    .filter((line) => !line.includes(own))
    .join("\n");
}

let lastReqCwd = null;

async function handle(req) {
  const reply = { id: req.id, value: null, error: null, images: [] };
  beginCell();
  try {
    if (req.cwd && req.cwd !== lastReqCwd) {
      process.chdir(req.cwd);
      lastReqCwd = req.cwd;
    }
    globalThis.require = createRequire(path.join(process.cwd(), "cell.js"));
    if (req.op === "call_tool") {
      reply.value = await invokeRegisteredTool(req.name, req.args ?? {});
    } else {
      const value = await runCell(req.code);
      if (value !== undefined) {
        reply.value = typeof value === "string" ? value : util.inspect(value, { depth: 4, maxArrayLength: 100 });
      }
    }
  } catch (err) {
    reply.error = fmtError(err);
  }
  reply.images = drainImages();
  writeOut(MARK + JSON.stringify(reply) + "\n");
}

const decoder = new TextDecoder();
let pending = "";
for await (const chunk of Bun.stdin.stream()) {
  pending += decoder.decode(chunk, { stream: true });
  for (let nl = pending.indexOf("\n"); nl >= 0; nl = pending.indexOf("\n")) {
    const line = pending.slice(0, nl);
    pending = pending.slice(nl + 1);
    if (line.trim()) await handle(JSON.parse(line));
  }
}
