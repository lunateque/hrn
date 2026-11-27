//! Background work as `proc://` sees it: agents, bash jobs and services behind one port.
//!
//! Each kind of work registers a [`ProcSource`]; a [`ProcTable`] merges them for the URL scheme,
//! `wait` and the front-ends' running indicator. Zero I/O lives here.

use crate::error::Result;
use std::sync::Arc;

/// One row of the process table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcEntry {
    /// The handle in `proc://<id>`; unique across sources.
    pub id: String,
    /// Root session of the tree the work belongs to (visibility scope).
    pub root: String,
    /// Session whose inbox receives the result.
    pub owner: String,
    /// Whether it still runs.
    pub running: bool,
    /// Whether `wait` should block on it: services run until stopped and never settle on their
    /// own, so they are not.
    pub waitable: bool,
    /// When it started (ms since epoch); orders the listing.
    pub started_ms: i64,
    /// When it settled (ms since epoch); `0` while running.
    pub finished_ms: i64,
    /// What a listing shows for it (may span lines).
    pub line: String,
}

/// A kind of background work addressable under `proc://`.
pub trait ProcSource: Send + Sync {
    /// Every entry, running or settled.
    fn entries(&self) -> Vec<ProcEntry>;

    /// Status and output tail of `id`; `None` when `id` is not this source's.
    fn detail(&self, id: &str) -> Option<String>;

    /// Everything `id` produced so far; `None` when `id` is not this source's.
    fn streams(&self, id: &str) -> Option<String>;

    /// Stop `id`; `None` when `id` is not this source's.
    fn kill(&self, id: &str) -> Option<Result>;

    /// Send `content` to the standard input of `id` (the Enter key included); `None` when `id`
    /// is not this source's, an error when the work takes no input.
    fn write(&self, _id: &str, _content: &str) -> Option<Result> {
        None
    }
}

/// The sources of a process, queried together.
#[derive(Default)]
pub struct ProcTable {
    sources: Vec<Arc<dyn ProcSource>>,
}

impl ProcTable {
    /// A table over `sources`; earlier sources win an id clash.
    pub fn new(sources: Vec<Arc<dyn ProcSource>>) -> Self {
        Self { sources }
    }

    fn all(&self) -> Vec<ProcEntry> {
        let mut all: Vec<ProcEntry> = self.sources.iter().flat_map(|s| s.entries()).collect();
        all.sort_by_key(|e| e.started_ms);
        all
    }

    /// The entries of the tree rooted at `root`, oldest first.
    pub fn in_root(&self, root: &str) -> Vec<ProcEntry> {
        let mut all = self.all();
        all.retain(|e| e.root == root);
        all
    }

    /// The entries whose result goes to `owner`, oldest first.
    pub fn owned(&self, owner: &str) -> Vec<ProcEntry> {
        let mut all = self.all();
        all.retain(|e| e.owner == owner);
        all
    }

    /// Ids still running in the tree rooted at `root`.
    pub fn running_ids(&self, root: &str) -> Vec<String> {
        self.in_root(root)
            .into_iter()
            .filter(|e| e.running)
            .map(|e| e.id)
            .collect()
    }

    /// The first source answering `ask` for `id`.
    pub fn ask<T>(&self, id: &str, ask: impl Fn(&dyn ProcSource, &str) -> Option<T>) -> Option<T> {
        self.sources.iter().find_map(|s| ask(s.as_ref(), id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(&'static str, i64, bool);

    impl ProcSource for Fixed {
        fn entries(&self) -> Vec<ProcEntry> {
            vec![ProcEntry {
                id: self.0.to_owned(),
                root: "r".to_owned(),
                owner: "o".to_owned(),
                running: self.2,
                waitable: true,
                started_ms: self.1,
                finished_ms: 0,
                line: self.0.to_owned(),
            }]
        }

        fn detail(&self, id: &str) -> Option<String> {
            (id == self.0).then(|| format!("detail {id}"))
        }

        fn streams(&self, _id: &str) -> Option<String> {
            None
        }

        fn kill(&self, _id: &str) -> Option<Result> {
            None
        }
    }

    #[test]
    fn entries_merge_oldest_first_and_scope_by_root_and_owner() {
        let table = ProcTable::new(vec![
            Arc::new(Fixed("late", 20, true)),
            Arc::new(Fixed("early", 10, false)),
        ]);
        let ids: Vec<_> = table.in_root("r").into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["early", "late"]);
        assert!(table.in_root("other").is_empty());
        assert_eq!(table.owned("o").len(), 2);
        assert_eq!(table.running_ids("r"), ["late"]);
        assert_eq!(
            table.ask("early", |s, id| s.detail(id)).as_deref(),
            Some("detail early")
        );
        assert_eq!(table.ask("nope", |s, id| s.detail(id)), None);
    }
}
