# `desktop` — Windows UI Automation & Computer Use

`desktop` drives local Windows applications through a persistent DPI-aware PowerShell/C# bridge
with Windows UI Automation (`UIAutomationClient`), covered-window capture (`PrintWindow`), stable
numeric window handles, `ref=eN` element references, and background/takeover input.

## Approval Tiers

- **`Read` (inspection):** `list_windows`, `displays`, `capabilities`, `apps_list`, `screenshot`,
  `window_capture`, `zoom`, `ax`, `find`, `el_read`, `clipboard_read`.
- **`Exec` (mutation/input):** `invoke`, `set_value`, `select_text`, `focus`, `el_type`, `click`,
  `double_click`, `right_click`, `move`, `scroll`, `type_text`, `hotkey`, `clipboard_write`,
  `open_app`, `close`.

## Window Handles, Element Refs & Coordinate Frames

- `list_windows` assigns stable numeric window handles (`id=1, 2, ...`) reused whenever the same
  `HWND` reappears. Pass `window_id` (or `id`) or a title substring in `window`.
- `ax` walks the UIA tree (default `max_depth=24`, capped at `800` nodes) and prints compact lines:
  `- role "label" [ref=eN]: "value" (states)`.
- Element refs (`e1`, `e2`, ...) remain valid across the current and previous snapshot of their
  window and expire with `[StaleRef]` once absent from both or when the window closes.
- `screenshot` and `window_capture` establish the active coordinate frame mapping image pixels to
  physical screen pixels. If a captured window moves or resizes before a coordinate action,
  `[InvalidCoordinateFrame]` is raised — recapture first.
- `zoom` crops a region `(x, y, w, h)` in the active frame's coordinate space **without** replacing
  the active click coordinate frame.

## Actions

### Discovery & System State (`Read`)
- `list_windows` — list visible top-level windows with stable `id`, `pid`, `hwnd`, rect, and title.
- `displays` — list connected displays (`id`, `name`, `primary`, bounds).
- `capabilities` — report UIA/background availability and `screenLocked` status (disconnected or UAC).
- `apps_list` — list running GUI applications with window counts.
- `clipboard_read` — read current plain text from the Windows clipboard.

### Visual Capture (`Read`)
- `screenshot` — capture a display (`display`: `"active"`, `"primary"`, `"all"`, index, or name;
  `all_monitors`: bool) or window (`window_id`/`id`/`window`). Updates active coordinate frame.
- `window_capture` — capture a specific window (`window_id`/`id`/`window`) via `PrintWindow`, even
  when covered by another window; reports `covered=true|false` and updates the coordinate frame.
- `zoom` — crop `(x, y, w, h)` in current image space (or physical pixels if no frame is active)
  without replacing the active click coordinate frame.

### UI Automation Inspection (`Read`)
- `ax` — dump the UIA tree for `window_id`/`id`/`window` (default foreground window). Optional
  `max_depth` (default `24`) and `max_nodes` (default/max `800`).
- `find` — search elements in `window_id`/`id`/`window` by `role` (case-insensitive alias matching,
  ignoring `_` and `-`), `title` (substring), and/or `value` (substring). Optional `limit` (<= 5000).
- `el_read` — read a field of `ref`: `field` in `"value" | "bounds" | "actions" | "parent" | "children"`.

### UI Automation Control (`Exec`)
- `invoke` — trigger default action on `ref` (`Invoke`, `Toggle`, `Select`, or `ExpandCollapse`).
- `set_value` — set text value on `ref` via `ValuePattern` and verify by read-back; fails with
  `[AxUnconfirmed]` if unsupported or unchanged.
- `select_text` — select text in `ref` (full text, or character range `start`..`end`).
- `focus` — move keyboard focus to `ref`.
- `el_type` — focus `ref` and type Unicode `text`.

### Pointer, Keyboard & Background Input (`Exec`)
- `click`, `double_click`, `right_click` — click `(x, y)` in active frame space.
- `move` — move cursor to `(x, y)` in active frame space.
- `scroll` — wheel scroll by `delta_y` (positive = up, negative = down), optionally at `(x, y)`.
- `type_text` — type Unicode `text`.
- `hotkey` — press key chord `keys` (string `"ctrl+shift+p"` or array `["ctrl", "shift", "p"]`).
- Background targeting: pass `window_id`/`id`/`window` or `background: true` on `click`,
  `double_click`, `right_click`, `type_text`, or `hotkey` to deliver via `PostMessage` without
  stealing focus. If unavailable and `[desktop] background_fallback = "takeover"` (default), the
  tool temporarily focuses the window, drains input (<= 5 s), restores prior focus, and reports
  the takeover. With `"refuse"`, it returns `[BackgroundUnavailable]`.

### Apps, Clipboard & Lifecycle (`Exec`)
- `clipboard_write` — write `text` to the Windows clipboard.
- `open_app` — launch `target` (executable or shell target) with optional `args` array.
- `close` — shut down the persistent bridge and reset session handles/refs/frames.

## Configuration (`[desktop]`)

```toml
[desktop]
display = "active"              # "active" | "primary" | "all" | display index/name
max_width = 3840                # max captured image width
max_height = 2400               # max captured image height
background_fallback = "takeover" # "takeover" | "refuse"
```
