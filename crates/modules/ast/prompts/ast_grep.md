Structural code search via ast-grep. Use when syntax shape matters more than text (calls, declarations, language constructs).

- Narrow each call to one language. `pat` is ONE AST pattern; separate calls for unrelated patterns.
- Set `lang` when extension inference is ambiguous; without it every file is parsed in the language its extension maps to.
- `$NAME` captures one node; `$_` matches without binding; `$$$NAME` zero-or-more; `$$$` zero-or-more unbound.
  - Use `$$$NAME`, NOT `$$NAME` (invalid). Names UPPERCASE, whole node — `prefix$VAR` fails.
- Same metavariable twice → MUST match identical code (`$A == $A` matches `x == x`, not `x == y`).
- Patterns MUST parse as a single AST node. Non-standalone → wrap: `class $_ { … }`.
- TS: tolerate annotations — `async function $NAME($$$ARGS): $_ { $$$BODY }`.
- Declaration forms are distinct — `function foo`, method `foo()`, `const foo = () => {}`; search the right form before concluding absence.
- Loosest existence check: `pat: "executeBash"` with narrow `path`.
- AVOID repo-root scans — narrow `path` first.
- Parse issues mean query failure, not absence: fix the pattern or tighten `path` before concluding "no matches".
