Create a file or replace its whole content (parent directories are created). Prefer `edit` for
changes to existing files; overwriting refuses if the file changed since you last read it.
Address a part of a file with `read`'s selectors: an archive member `a.zip:inner/path`, a SQLite
row `db.sqlite:table[:key]` (a JSON5 object of scalars; empty `content` deletes the keyed row),
or a writable URL (`local://notes.md` session scratch, `conflict://N|*` to resolve a merge hunk
— `@ours` / `@theirs` / `@base` / `@both` as a whole line take that side).
