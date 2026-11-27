Describe behavior, get the files and line ranges that implement it, each with an absolute
0..1 score. Use it first when you do not know where something lives; for a known string,
regex or symbol use `grep`, for file names `glob`. `query` is plain language, not a regex.
`grep_keywords` are likely verbatim terms from the code (identifiers, strings), `[]` if unsure;
a cheap keyword scan of the live tree picks the candidate files and a fast model then scores
excerpts of them, so there is no index to go stale. Scope `path` to a known subsystem and ask
related questions in one call. Hits come strongest first as `path:start-end score snippet`.
A score below about 0.4 is weak evidence: widen the query or use `grep` before concluding
something is absent.
