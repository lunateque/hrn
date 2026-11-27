Inspect this repository and create or refresh a concise, human-editable `AGENTS.md` at the repository root.

## How to work
1. Inspect the real repository before writing anything: top-level directory layout, build/package manifests, test/lint/CI configs (`Makefile`, `justfile`, `Cargo.toml`, `package.json`, `pyproject.toml`, `.github/workflows`, etc.), and any existing `AGENTS.md`, `CLAUDE.md`, or `README.md`.
2. Preserve valid project-specific guidance already in an existing `AGENTS.md`; update stale paths or commands and remove outdated notes.
3. Verify commands exist in the repository manifests/scripts rather than inventing them.

## What `AGENTS.md` must include
- **Overview**: 1-2 sentences on what the repository builds and its primary stack.
- **Commands**: exact build, test, lint, format, and single-test commands used in this repo.
- **Layout**: concise map of the main directories/crates/packages and module boundaries.
- **Conventions & constraints**: concrete architecture rules, invariants, feature flags, or style gates enforced by CI.
- **Gotchas**: real non-obvious pitfalls discovered in the codebase (platform quirks, generated files, test fixtures, etc.).

## Strict rules
- Keep it short and scannable (roughly 50-140 lines of crisp Markdown; prefer compact lists over prose).
- Write ONLY concrete, verifiable facts about this codebase.
- Do NOT include generic LLM boilerplate ("write clean code", "add tests for new features", "follow best practices", "handle errors gracefully", "keep functions small" unless quoting an explicit numeric CI gate from the repo).
- Do NOT restate obvious language basics or invent tooling not present in the repository.
