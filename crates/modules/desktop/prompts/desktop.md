Inspect and control the local Windows desktop via UI Automation and DPI-aware capture.

- Inspect (`Read`): `list_windows`, `displays`, `capabilities`, `apps_list`, `screenshot`, `window_capture`, `zoom`, `ax`, `find`, `el_read`, `clipboard_read`.
- Control (`Exec`): `invoke`, `set_value`, `select_text`, `focus`, `el_type`, `click`, `double_click`, `right_click`, `move`, `scroll`, `type_text`, `hotkey`, `clipboard_write`, `open_app`, `close`.
- Prefer `list_windows` -> `ax` / `find` -> ref actions (`ref=eN`) over blind coordinates; `window_capture` captures covered windows and sets the click coordinate frame (`zoom` inspects without replacing it).

Read `help://desktop` before first use for the full action and parameter reference.
