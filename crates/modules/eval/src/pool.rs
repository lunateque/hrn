//! Lazily created kernels, one per (session, language), bounded by least-recently-used eviction.

use crate::dyn_tools::KernelToolStore;
use crate::kernel::Kernel;
use crate::lang::Lang;
use hrn_edge_rt::tokio;
use hrn_kernel::{HrnError, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

/// Kernels alive at once across all sessions; the least recently used one is killed beyond this.
const MAX_KERNELS: usize = 8;

/// A kernel slot: empty until the first call, locked for the duration of every call.
pub(crate) type KernelSlot = Arc<tokio::sync::Mutex<Option<Kernel>>>;
/// Non-owning handle to a [`KernelSlot`], preventing `Arc` cycles between a kernel and its bridge
/// or registered `@tool` callbacks so dropping the pool immediately kills and reaps the child.
pub(crate) type WeakKernelSlot = Weak<tokio::sync::Mutex<Option<Kernel>>>;

struct Entry {
    session: String,
    lang: Lang,
    slot: KernelSlot,
    used: u64,
}

#[derive(Default)]
struct Registry {
    entries: Vec<Entry>,
    clock: u64,
}

/// Owns the runner + prelude scripts and every live kernel.
pub(crate) struct Pool {
    data_dir: PathBuf,
    registry: Mutex<Registry>,
    dyn_tools: Arc<KernelToolStore>,
}

impl Pool {
    pub(crate) fn new(data_dir: PathBuf, dyn_tools: Arc<KernelToolStore>) -> Self {
        Self {
            data_dir,
            registry: Mutex::new(Registry::default()),
            dyn_tools,
        }
    }

    /// Shared store of kernel-defined `@tool` functions.
    pub(crate) fn dyn_tools(&self) -> &Arc<KernelToolStore> {
        &self.dyn_tools
    }

    /// The slot for `session` + `lang`, created (evicting the stalest) when missing.
    pub(crate) fn slot(&self, session: &str, lang: Lang) -> KernelSlot {
        let mut reg = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        reg.clock += 1;
        let now = reg.clock;
        if let Some(entry) = reg
            .entries
            .iter_mut()
            .find(|e| e.lang == lang && e.session == session)
        {
            entry.used = now;
            return Arc::clone(&entry.slot);
        }
        let oldest = if reg.entries.len() >= MAX_KERNELS {
            reg.entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.used)
                .map(|(i, _)| i)
        } else {
            None
        };
        let evicted = oldest.map(|i| reg.entries.swap_remove(i));
        let slot = KernelSlot::default();
        reg.entries.push(Entry {
            session: session.to_owned(),
            lang,
            slot: Arc::clone(&slot),
            used: now,
        });
        drop(reg);
        if let Some(old) = evicted {
            self.dyn_tools.clear_kernel(&old.session, old.lang);
        }
        slot
    }

    /// Materialize both the runner script and embedded prelude for `lang` inside `<data_dir>/eval/`.
    pub(crate) async fn script(&self, lang: Lang) -> Result<PathBuf> {
        let dir = self.data_dir.join("eval");
        write_if_stale(&dir, lang.prelude_name(), lang.prelude_source()).await?;
        write_if_stale(&dir, lang.script_name(), lang.script_source()).await
    }
}

async fn write_if_stale(dir: &Path, name: &str, source: &str) -> Result<PathBuf> {
    let path = dir.join(name);
    if tokio::fs::read_to_string(&path)
        .await
        .is_ok_and(|current| current == source)
    {
        return Ok(path);
    }
    let io_err = |what: &str, e: std::io::Error| {
        HrnError::Io(format!("eval: {what} {}: {e}", path.display()))
    };
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| io_err("create dir for", e))?;
    tokio::fs::write(&path, source)
        .await
        .map_err(|e| io_err("write", e))?;
    Ok(path)
}
