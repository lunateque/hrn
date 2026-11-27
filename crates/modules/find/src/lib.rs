//! `find`: semantic code search. A keyword scan of the live tree picks candidate files, a fast
//! model scores line-ranged excerpts of them, and the tool answers with ranked
//! `path:start-end score snippet` lines. There is no index to build or keep fresh.
//!
//! ```toml
//! [find]
//! role = "find"          # model role that scores excerpts: `[roles] find`, else `fast`, else default
//! max_files = 30         # files kept after the keyword scan
//! max_chunk_lines = 40   # longest excerpt, in lines
//! max_chunks = 48        # excerpts scored per call
//! batch_chunks = 8       # excerpts per model request
//! concurrency = 4        # model requests in flight at once
//! max_results = 10       # hits returned
//! min_score = 0.15       # hits below this are dropped
//! ```

#[cfg(test)]
#[path = "e2e_tests.rs"]
mod e2e_tests;
mod score;
mod tool;

use hrn_kernel::serde::Deserialize;
use hrn_kernel::{HrnError, Registry, Result};
use std::sync::Arc;

/// The `[find]` config table.
#[derive(Clone, Debug, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields, default)]
pub(crate) struct FindConfig {
    pub(crate) role: String,
    pub(crate) max_files: usize,
    pub(crate) max_chunk_lines: usize,
    pub(crate) max_chunks: usize,
    pub(crate) batch_chunks: usize,
    pub(crate) concurrency: usize,
    pub(crate) max_results: usize,
    pub(crate) min_score: f64,
}

impl Default for FindConfig {
    fn default() -> Self {
        Self {
            role: "find".to_owned(),
            max_files: 30,
            max_chunk_lines: 40,
            max_chunks: 48,
            batch_chunks: 8,
            concurrency: 4,
            max_results: 10,
            min_score: 0.15,
        }
    }
}

impl FindConfig {
    fn validate(&self) -> Result {
        let counts = [
            ("max_files", self.max_files),
            ("max_chunk_lines", self.max_chunk_lines),
            ("max_chunks", self.max_chunks),
            ("batch_chunks", self.batch_chunks),
            ("concurrency", self.concurrency),
            ("max_results", self.max_results),
        ];
        for (name, value) in counts {
            if value == 0 {
                return Err(HrnError::Config(format!(
                    "[find] {name} must be at least 1"
                )));
            }
        }
        if self.role.trim().is_empty() {
            return Err(HrnError::Config(
                "[find] role must name a model or role".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.min_score) {
            return Err(HrnError::Config(
                "[find] min_score must be within 0..1".into(),
            ));
        }
        Ok(())
    }
}

/// Register the `find` tool.
///
/// # Errors
///
/// When `[find]` is malformed or the tool name is taken.
pub fn register(registry: &mut Registry) -> Result {
    let cfg: FindConfig = registry.config()?;
    cfg.validate()?;
    registry.tool(Arc::new(tool::FindTool::new(cfg, registry.llm())))
}
