Edit existing files with line-anchored patches. `input` holds one or more sections; each starts
with `[PATH#TAG]` copied from the latest `read` (or the fresh header an earlier `edit`
returned), then operations. Line numbers always refer to that tagged snapshot, never to earlier
operations of the same call. Refused when the file changed since your last read: re-read, retry.
Copy `[FILENAME#TAG]` verbatim; NEVER invent the tag.
Operations:
`PUT N.=M:` replace inclusive lines N..M with the `+TEXT` rows that follow (`N.=N` for one line).
`PUT N*:` replace the multi-line block (function, class, struct, heading section...) that opens on line N;
anchor its first line (first decorator/attribute), never a closing line or inner statement.
`PUT <N:` / `PUT >N:` insert the rows before / after line N; `PUT <1:` is the file head, `PUT >$:` appends;
`PUT >N*:` inserts after the block opening on line N.
`CUT N.=M` / `CUT N*` delete the lines and capture them; add ` @name` to name the register.
`PUT <N` / `PUT >N` / `PUT >$` (no colon, no body) paste the last cut; `PUT <N @name` pastes a named register;
`PUT N.=M @name` / `PUT N* @name` replace a range or block with it. Registers persist for the session.
`REM` deletes the file. `MV DEST` writes the edited file to DEST and deletes the source.
Every body row is `+TEXT` (final content, not a diff); `+` alone is a blank line; literal text starting
with `+` or `-` is written `++...` / `+-...`. `CUT`, pastes, `REM` and `MV` take no body.
Keep ranges tight; use separate hunks for separate changes. Several files may be edited in one call.
