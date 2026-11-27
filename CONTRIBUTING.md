# Contributing to `hrn`

Thanks for your interest in contributing to `hrn`!

`hrn` is a standalone, modular Rust coding harness inspired by [`oh-my-pi`](https://github.com/can1357/oh-my-pi). The workspace enforces strict architectural boundaries and code hygiene checks so the codebase stays fast to build, easy to reason about, and safe to extend.

## Prerequisites

- **Rust stable** (Edition 2024; see `rust-version` in [`Cargo.toml`](Cargo.toml)) with `rustfmt` and `clippy` components.
- **`git`** (required by `cargo xtask check` and worktree/checkpoint tests).

## Workspace Architecture & Rules

All architectural invariants are enforced mechanically by `cargo xtask check`:

1. **Layered dependency graph** (see [`deps.allow`](deps.allow)):
   - `hrn-kernel` — zero-I/O canonical IR, DAG types, and port traits.
   - `hrn-harness` — core agent loop, compaction, subagents, discovery, scope enforcement.
   - `hrn-edge-*` — I/O adapters (HTTP, LLM, MCP, OAuth, IPC, DAP, AST, SQLite store, processes).
   - `hrn-mod-*` — removable feature modules registering tools/hooks/commands via `pub fn register(&mut Registry)`.
   - `hrn-rpc` / `hrn-tui` — protocol and terminal frontends.
   - `crates/bins/*` — thin binary wiring (`hrn`, `hrn-ast`, `hrn-relay`).
2. **File size budget (`<= 600` LOC):**
   - Every tracked `.rs` file under `crates/` and `xtask/src/` must stay at or below **600 lines**. Split large files into focused submodules rather than growing monoliths.
3. **Removable modules:**
   - Never wire optional module features manually outside `crates/bins/harness/src/modules.rs`.
   - Use `cargo xtask module new <id>` and `cargo xtask module remove <id>` to add or remove modules.
   - If you add or change a module's wiring, verify `cargo xtask removability` and `cargo check -p hrn --no-default-features`.

## Before Opening a Pull Request

Run the local checks from the repository root (or `just ci` if you have [`just`](https://github.com/casey/just)):

```bash
# 1. Formatting
cargo fmt --all -- --check

# 2. Architectural gate (crate graph, <= 600 LOC/file, feature isolation)
cargo xtask check

# 3. Clippy lints (zero warnings)
cargo clippy --workspace --all-targets -- -D warnings

# 4. Workspace tests
cargo test --workspace
```

### Cross-Platform Tips (Windows & Linux)

- Tests run on both `ubuntu-latest` and `windows-latest` in GitHub Actions.
- For temporary `git init` test repositories, explicitly configure `core.autocrlf false` and `core.eol lf` so tests do not fail under Windows CRLF defaults.
- Avoid timing-dependent sleeps in async tests; wait on explicit state transitions or notifications instead.

## Security & Hygiene Guidelines

- **Respect credential & ToS guardrails:** as many providers are starting to ban users for such activities, it's best not to steal credentials from third-party CLI stores (`.gemini/`, `.claude/`, `.codex/`). Do not add consumer Google Gemini CLI / Antigravity OAuth client IDs or endpoints, as that would lead to the account getting banned (`crates/edge/http/src/guard.rs`).
- **Keep PRs focused:** one logical change per pull request, with unit or integration tests covering the behavior or bug fix.
- **Commit messages:** prefer [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`).

## Pull Request Process

1. Fork the repository and create a branch off `main`.
2. Make your changes and add/update tests.
3. Ensure `cargo fmt`, `cargo xtask check`, `cargo clippy`, and `cargo test --workspace` all pass.
4. Open a pull request describing what changed, why, and how it was verified.
