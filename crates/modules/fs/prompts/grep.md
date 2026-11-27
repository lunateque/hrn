Search file contents with a regex (Rust syntax; a pattern it rejects is searched literally and
the result says so). `path`: `;`-separated file/dir/glob; default `.`. A `!glob` entry excludes
matches below the searched paths (`.; !vendor`). A single file takes a line window
(`src/foo.ts:50-100`). A pattern containing `\n` (or a real newline) matches across lines.
Default case-sensitive, gitignore respected, dotfiles searched; binary files skipped. Results
group by directory; `*LINE:text` marks a match, `...` a gap. At most 20 files per page (`skip`
= files already seen; take it from `Use skip=N`), 10 matches per file when searching several
(50 for one file).
Behavior/unknown symbol -> `find`; file names -> `glob`.
