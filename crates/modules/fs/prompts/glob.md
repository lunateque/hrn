Find files and directories by glob, newest first. `path` is a glob, file or directory, or
several joined with `;` (`src/**/*.ts; test/**/*.ts`); default `.`. A bare glob (`*.ts`)
matches at any depth, `dir/*.ts` only direct children, `dir/**/*.ts` recurses; a trailing `/`
matches directories only. Results group by directory (`# a/b/` headers, directories end in
`/`). Respects .gitignore, skips .git and heavy build/cache directories; hidden files are
included. Content search -> `grep`; behavior search -> `find`.
