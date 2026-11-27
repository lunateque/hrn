# `hrn`

> **Experimental Rust reimplementation of the [oh-my-pi (`omp`)](https://github.com/can1357/oh-my-pi) architecture.**

`hrn` is a standalone, strictly modular, Windows-first multi-agent coding harness written in Rust. It reimplements key parts of [`oh-my-pi`](https://github.com/can1357/oh-my-pi)'s agent loop, tool contracts, configuration shapes, and terminal UX with fast startup, native MCP support, mechanical subagent scope enforcement, and strict compile-time crate boundaries.

> **Status & Disclaimer:** This project is experimental and under active development. It is an independent Rust rewrite inspired by [`oh-my-pi`](https://github.com/can1357/oh-my-pi) and is not affiliated with or endorsed by the upstream project.

## Credits

The architecture, tool and UI contracts (`edit` hashline protocol, HTML `ask` forms, `<provider>/<id>` model catalog, `.omp/` configuration discovery, memory/checkpoint workflows, and NDJSON RPC) are directly inspired by [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi).

## Architecture

The workspace enforces strict layer boundaries and a `<= 600` LOC per-file budget via `cargo xtask check`:

| Layer | Crates | Responsibility |
| --- | --- | --- |
| **Kernel** | `hrn-kernel` | Zero-I/O canonical IR, event-sourced DAG types, port traits (`LlmProvider`, `Tool`, `HarnessHook`, `ApprovalGate`, `AskHandler`, `SessionStore`) |
| **Core Harness** | `hrn-harness` | Agent turn loop, parallel read-only tool batching, 3-tier context compaction, extensibility discovery (`agents`, `skills`, `commands`), subagents, and `ScopePolicy` / `ScopeGuardHook` |
| **Edges** | `hrn-edge-ast`, `hrn-edge-dap`, `hrn-edge-http`, `hrn-edge-ipc`, `hrn-edge-llm`, `hrn-edge-mcp`, `hrn-edge-oauth`, `hrn-edge-proc`, `hrn-edge-rt`, `hrn-edge-store`, `hrn-edge-tools` | Tree-sitter AST helpers, DAP client, HTTP/SSE + loopback RFC 6455 WS, loopback IPC daemon + git worktrees + SSH tunnel remoting, multimodal multi-provider LLM router, native MCP client/server, RFC 8252 PKCE OAuth2, job-object process & background job runner, runtime helpers, SQLite DAG store, and builtin tools + image downscaling |
| **Modules** | `hrn-mod-agy`, `hrn-mod-ast`, `hrn-mod-browser`, `hrn-mod-checkpoint`, `hrn-mod-debug`, `hrn-mod-desktop`, `hrn-mod-edit`, `hrn-mod-eval`, `hrn-mod-find`, `hrn-mod-fs`, `hrn-mod-github`, `hrn-mod-hooks`, `hrn-mod-lsp`, `hrn-mod-memory`, `hrn-mod-meta`, `hrn-mod-shell`, `hrn-mod-web` | Removable tool families exposing `pub fn register(&mut Registry)` (tools, deferred tools, URL schemes, hooks, `proc://` sources, prompts, slash commands, typed `[<id>]` config). Wired via per-module Cargo features in generated `crates/bins/harness/src/modules.rs` |
| **Frontends & Binaries** | `hrn-rpc`, `hrn-tui`, `hrn` (`crates/bins/harness`), `hrn-ast` (`crates/bins/ast`), `hrn-relay` (`crates/bins/relay`) | NDJSON RPC / ACP frontends, inline Ratatui TUI (terminal scrollback, themes, inline half-block image previews, `/tree`, `/fork`, `/plan`, inline `ask` forms), main CLI binary, tree-sitter AST sidecar, and remote relay binary |

## Building & Quick Start

### Prerequisites

- Rust stable toolchain (edition 2024; see `Cargo.toml`)
- Optional external tools for specific modules: `git`, `gh` (`github`), `python` / `bun` (`eval`), Chrome/Edge/Brave (`browser`), language servers (`lsp`), `agy` (`agy`)

### Build

```bash
# Build the main CLI binary
cargo build --release -p hrn

# Optional: build the tree-sitter sidecar used by the `ast` module
cargo build --release -p hrn-ast

# Optional: build the bare core without optional modules
cargo build --release -p hrn --no-default-features
```

### Configure & Run

Copy `hrn.example.toml` to your harness config directory and set your provider API keys (or sign in via `hrn login`):

```bash
# Linux / macOS
mkdir -p ~/.harness
cp hrn.example.toml ~/.harness/config.toml
```

```powershell
# Windows (PowerShell)
New-Item -ItemType Directory -Force "$HOME\.harness" | Out-Null
Copy-Item hrn.example.toml "$HOME\.harness\config.toml"
```

Run `hrn`:

```bash
# Interactive inline TUI
cargo run -p hrn

# Single-shot non-interactive execution (text / json / stream-json)
hrn exec "Summarize the workspace architecture"
hrn exec --model fast --json "List public structs in hrn-kernel"

# Validate configuration and provider auth
hrn config check
hrn auth status

# Import sessions or providers/config from OMP (read-only)
hrn sessions import omp --latest
hrn chat --from-omp latest
hrn providers import omp --dry-run
```

## Key Features

- **Multi-provider LLM routing & auth:** Anthropic Messages, OpenAI `/responses`, OpenAI-compatible `/chat/completions`, official Gemini Developer API, generic RFC 8252 PKCE OAuth2 (`hrn login <provider>`), subscription logins (`hrn login claude`, `hrn login chatgpt`, `hrn login kimi-code`), built-in `<provider>/<id>` catalog (`models.yml`), role presets (`default`, `fast`, `plan`, `reviewer`), fallback chains, and local Ollama discovery. Guardrails refuse third-party CLI secret files and Google Gemini CLI / Antigravity OAuth client IDs.
- **OMP-compatible discovery & extensibility:** Auto-discovers agents, skills (`skills/*/SKILL.md`), rules (`rules/*.md|.mdc`), custom slash commands (`commands/*.md`), hooks, custom tools, and `models.yml` / `mcp.json` across `~/.harness/`, `.harness/`, and compatibility layers (`.omp/`, `.claude/`, `.cursor/`, `.agents/`, `.codex/`).
- **Native MCP client & strict stdio server:** Cached server tool discovery, token-budgeted deferred tool loading via `tool_search`, and `hrn mcp serve` exposing `run_agent`, `spawn_agent`, `wait_agent`, `list_jobs` under mechanical `ScopePolicy` enforcement (`ScopeGuardHook`).
- **Inline Ratatui TUI & protocol frontends:** Scrollback-native TUI (no alternate screen), customizable themes and keybindings, multimodal clipboard/image previews, interactive `/tree` and `/fork` on the SQLite session DAG, `/plan` mode, HTML `ask` forms, hashline `edit`, background `bash` / PTY jobs (`proc://`), background subagents (`agent://`), plus `hrn rpc` (NDJSON) and `hrn acp` (Agent Client Protocol).
- **Removable modules:** `fs`, `edit`, `shell`, `find`, `ast` (`hrn-ast` sidecar), `lsp`, `debug` (DAP), `browser` (CDP), `eval` (Python/JS persistent kernels), `desktop` (Windows UI automation), `checkpoint` (shadow git snapshots), `memory` (SQLite+FTS5), `github`, `hooks`, `web`, `meta`, and `agy`. Add or remove modules with `cargo xtask module new <id>` / `cargo xtask module remove <id>`.
- **Daemon, worktrees & SSH remoting:** Loopback HTTP/1.1 + SSE daemon (`hrn daemon`), `hrn attach`, isolated git worktrees (`hrn worktree`), and SSH-tunneled remote hosts (`hrn remote`).

## Development & Verification

See [CONTRIBUTING.md](CONTRIBUTING.md) for workspace layer rules, module tooling, and the pull request checklist. Run the architectural gate and test suite locally before submitting changes:

```bash
# Formatting check
cargo fmt --all -- --check

# Architectural gate (crate dependency graph, <= 600 LOC per .rs file, module feature isolation)
cargo xtask check

# Lints & docs
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# Workspace tests
cargo test --workspace --all-features

# Verify minimal core compiles without optional modules
cargo check -p hrn --no-default-features

# Verify every module can be cleanly removed (slow: builds/tests once per module)
cargo xtask removability
```

## License

Licensed under the [MIT License](LICENSE).
