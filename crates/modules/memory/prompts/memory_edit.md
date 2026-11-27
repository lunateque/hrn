Update, forget or invalidate a long-term memory by id (ids come from `recall`). `update`
replaces the text wholesale, so `read memory://<id>` first; a recall preview may be clipped.
Prefer `invalidate` for memories that went stale but whose history may matter; use `forget`
only to delete one outright.
