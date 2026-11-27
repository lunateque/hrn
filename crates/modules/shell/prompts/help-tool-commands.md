# Tool commands of the `bash` shell

Every registered tool is also a command of the `bash` tool's shell (`read notes.txt`,
`grep --pattern x --path a.rs`); `<tool> --help` prints its usage page. Exit codes: `0` ok,
`1` the tool failed, `2` a bad command line.

A name the shell itself owns (`read`, `test`, `type`, …) keeps its shell meaning unless the
first argument is `-j`/`--json`, a flag, or an `@file`/`*://` argument; a name an external
command owns keeps that command.

## Argument shapes

- Required top-level scalar/enum fields are positionals, in schema order.
- Every field also takes `--flag value`; underscores become dashes, a nested field is
  `--a.b.value`.
- A boolean is `--flag` / `--no-flag`; an array flag repeats.
- `-j` / `--json <JSON>` merges a raw arguments object.
- `@FILE` or `-` merges a JSON object from the file/stdin, or binds its text to the next
  positional.

Commands run through the same session as the agent: the same permission mode, approval prompt
and hooks gate them. While the owning `bash` call is live the command addresses that session;
otherwise it falls back to the standalone behaviour of the same tool.
