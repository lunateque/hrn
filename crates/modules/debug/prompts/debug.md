Drive a DAP debug session (debugpy, codelldb, lldb-dap, gdb, delve, js-debug, ...): `action`
picks the operation, the other fields its arguments.

- Start: `launch` (`program`, `args`) or `attach` (`pid`, or `port`/`host`).
- Breakpoints: source (`set_breakpoint` by `file` + `line`, or `function`; conditions
  `condition` / `hit_condition`), instruction, and data (`data_breakpoint_info` first).
- Run: `continue`, `step_over`, `step_in`, `step_out`, `pause`, `terminate`.
- Inspect: `threads`, `stack_trace`, `scopes` / `variables`, `evaluate`, `disassemble`,
  `read_memory` / `write_memory`, `modules`, `loaded_sources`, `output`.
- `custom_request` (`command`, `arguments`) sends any other DAP request.

The session persists between calls; at most one runs at a time, listed under `proc://` as
`debug-<n>`; `sessions` lists targets and `session` picks one (`debug-1.1` is a child the
adapter started; the default is the target that last stopped, else the running one).
Per-action argument reference: read help://debug before first use.
