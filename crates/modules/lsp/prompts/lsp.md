Query a native Language Server (built-ins + user `lsp.toml`) for semantic code intelligence:
`diagnostics`, `definition`, `type_definition`, `implementation`, `references`, `hover`, `symbols`,
`incoming_calls`, `outgoing_calls`, `code_actions`, `rename` (plan preview), `format` (writes
formatted file), `status`, and `reload` (re-reads `lsp.toml`). Line and col are 1-based.
Symbol-aware work MUST use `lsp` when a server answers; text searches silently miss callsites.
