# `debug` actions and their arguments

One call takes `action` plus that action's arguments. `session` selects the debug target
(`debug-1`; `debug-1.1` is a child the adapter started); the default is the target that last
stopped, else the running one. `timeout` is a per-request deadline in seconds (5-300).

## Start and stop

- `launch` - `program` (path), `args` (argv array), `cwd`, `adapter` (`debugpy`, `dlv`,
  `codelldb`, `gdb`, `lldb-dap`, `js-debug-adapter`, ...; otherwise chosen from the target),
  `json` (extra DAP launch arguments merged over the adapter defaults).
- `attach` - `pid`, or `port` + `host` (default `127.0.0.1`); same `adapter`/`json` options.
- `terminate` - end the session; `sessions` lists the live targets.

## Breakpoints

- `set_breakpoint` / `remove_breakpoint` - `file` + `line`, or `function` (a function name
  instead of a location); optional `condition` (an expression) and `hit_condition` (stop after
  N hits).
- `set_instruction_breakpoint` / `remove_instruction_breakpoint` - `instruction_reference`
  (an address) with `offset`.
- `data_breakpoint_info` (`name` or `expression`, `frame_id`) returns a `data_id`; pass it to
  `set_data_breakpoint` with `access_type` (`read`, `write`, `readWrite`), and to
  `remove_data_breakpoint`.

## Stepping

`continue`, `step_over`, `step_in`, `step_out`, `pause`. Each returns the stop location when
the target stops.

## Inspecting

- `threads` then `stack_trace` (`levels` = max frames) for the call stack of a thread.
- `scopes` (`frame_id`) lists variable scopes; `variables` (`scope_id` or the preferred
  `variable_ref`) expands one.
- `evaluate` - `expression` in a `context` (default `repl`) at `frame_id`.
- `disassemble` - `memory_reference` (address) around `instruction_offset` for
  `instruction_count`, optionally `resolve_symbols`.
- `read_memory` - `memory_reference` + `count` bytes; `write_memory` - `memory_reference`,
  `offset`, `data` (base64), `allow_partial`.
- `modules` (`start_module`, `module_count`), `loaded_sources`, `output` (captured console).

`custom_request` sends any other DAP `command` with an `arguments` object.
