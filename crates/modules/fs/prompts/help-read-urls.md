# `read` path grammar

One `path` string addresses everything `read` can open; everything after the final `:` or `?`
is a selector, not part of the file name.

## Shapes

- `notes.txt` - a bare file: at most 300 numbered lines, a footer names the selector that
  continues a cut read.
- `notes.txt:50`, `:50-80`, `:50-`, `:50+30`, `:-20`, `:5-16,960-973`, `:19,59` - line
  ranges and single lines; `:raw` verbatim without line numbers; combinable
  (`notes.txt:raw:2-4`).
- `dir/` - a directory tree (two levels, 12 entries per sub-directory); `dir:20-40` pages it.
- `https://…`, `www.…` - a web page through the reader; `:raw` keeps the original HTML.
- `scheme://rest:selectors` - an internal URL (listed in the system prompt's Internal URLs
  block); the same selectors apply.
- `cfg://`, `cfg://<section>`, `cfg://<key>` - inspect effective layered settings (secrets
  show `<redacted>`); `write cfg://<key>` sets a session override and `write cfg://<key>/save`
  persists to global config (top-level interactive approval required).
- `out://<label>` - write-only mid-turn assistant note rendered directly to the user while the
  turn continues (top-level only, <= 64 KiB, enabled by `[output] out_enabled`).
- `ssh://`, `ssh://<host>/<path>` - read-only listing and files on pinned SSH hosts (`~/` expands
  against remote `$HOME`, capped at 1 MiB).
- `mcp://`, `mcp://<uri>` - read-only listing and contents of advertised MCP server resources.
- `attachment://`, `attachment://<n>-<name>` - read-only user-attached files/images for this
  session (supports `:img`, line selectors, and `?q=`).
- `a.zip:inner/path:2-4` - a member of an archive, itself selectable.
- `db.sqlite:table?limit=&offset=&order=` and `db.sqlite?q=SELECT …` - a table or a query.
- `file.json?q=.a.b|keys` - a jq filter (`&raw=true`, `&compact=true`, `&limit=`, `&offset=`).
- `src/a.rs:conflicts` - the merge-conflict index of a file (`conflict://N` shows one).

## Queries

`?` starts a query string on archive members, SQLite paths, JSON files and internal URLs
(`issue://?comments=0`, `history://?limit=5`). A query keeps the trailing line selector:
`db.sqlite:users?limit=20:1-5`.
