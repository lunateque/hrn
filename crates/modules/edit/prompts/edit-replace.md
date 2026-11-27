Replace `old` with `new` in an existing file. `old` must match exactly one place (add
surrounding lines to disambiguate) unless `replace_all` is set; trailing-whitespace,
indentation, and fuzzy similarity fallbacks are applied when enabled. On a miss the nearest
snippets are returned. Preserves BOM and CRLF/LF; refuses if the file changed since your last read.
