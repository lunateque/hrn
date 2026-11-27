# `read` rich sources

Full selector syntax for the formats `read` renders as text. The line selectors
(`:50`, `:50-80`, `:-20`, `:raw`) apply wherever they make sense.

## Archives

`a.zip`, `a.tar`, `a.tar.gz`/`a.tgz`, and the zip-family containers `jar`, `war`, `apk`,
`whl`, `vsix`, `nupkg`, `cbz`, `xpi`: the bare path lists members; `a.zip:inner/path` reads
one member with the same selectors; `a.zip:inner/path?q=.a.b` filters a JSON member. Writing
`a.zip:inner/path` (see `write`) replaces that member and keeps the rest.

## SQLite

- `db.sqlite` lists tables with row counts.
- `db.sqlite:table` shows the schema and first rows; `db.sqlite:table:key` reads one row by key.
- `db.sqlite:table?limit=20&offset=40&order=col:desc` pages a table.
- `db.sqlite?q=SELECT ...` runs a read-only query (single SELECT, no semicolons needed).

## JSON / JSONL

`file.json?q=<filter>` (also inside archives) runs a jq subset: `.a.b`, `.[n]`, `.[]`,
`.[a:b]`, `|`, `keys`, `length`, `type`, `first`, `last`, `map(..)`,
`select(.x==1 and .y!="z")`. Add `&raw=true` for unquoted strings, `&compact=true`,
`&limit=`, `&offset=`.

## Documents and notebooks

PDF, DOCX, XLSX, PPTX and EPUB extract as text. `.ipynb` renders as `# %% [kind] cell:N`
blocks; `:raw` returns the notebook JSON.

## Executables and media

A native executable (`.exe`/`.dll`/`.so`) reads as its metadata: arch, subsystem, sections,
imports, exports. Images (PNG/JPEG/WebP/GIF/BMP) return as a visual attachment. SVG reads as
text (`.svgz` is decompressed); `:img` forces raster reading and refuses plain SVG.

## Merge conflicts

A file with conflict markers registers ids when read: `path:conflicts` lists them,
`conflict://1` (or `/ours`, `/theirs`, `/base`) shows one with the file's line numbers, and
writing `conflict://1` resolves the hunk (`@ours` / `@theirs` / `@base` / `@both` as the whole
content take that side; `conflict://*` resolves every registered hunk).
