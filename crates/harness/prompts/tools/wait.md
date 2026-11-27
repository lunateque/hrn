Blocks on the first of your own background results (agents, `bash` jobs started with `async`,
`notify` lines, other messages); returns at once when nothing is pending. Prefer doing other
work — results arrive on their own; call `wait` only when nothing else is left to do.
