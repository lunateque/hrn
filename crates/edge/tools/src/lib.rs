//! The engines behind `hrn`'s built-in tools: reading, editing, patching and searching files,
//! HTML-to-text, the web-search backends, internal-URL schemes and the real-filesystem
//! [`WorkspaceFs`](hrn_kernel::WorkspaceFs).
//!
//! The tools themselves (schemas, effects, policies) live in the `fs`, `shell`, `web` and `meta`
//! modules; they call into the blocking functions here through [`args::blocking`].

mod apply_patch;
pub mod args;
mod attachments;
mod checkpoint;
pub mod command_policy;
mod conflicts;
mod dir_tree;
mod edit_flags;
mod edit_ladder;
pub mod eval_flags;
mod find;
mod fs_edit;
mod fs_read;
mod glob;
mod grep;
mod group;
mod hashline;
pub mod html;
pub mod img;
pub mod notify;
pub mod paths;
mod read_fmt;
mod read_tracker;
mod scope;
mod seen_lines;
mod selector;
mod snap;
mod snap_font;
mod text_codec;
mod time;
mod urls;
mod walk;
mod web_fetch;
mod web_search;
mod workspace_fs;

#[cfg(test)]
mod testutil;

pub use apply_patch::{apply_patch_sync, apply_patch_sync_with_opts};
pub use attachments::{
    DiskAttachmentStore, MAX_ATTACHMENT_BYTES, MAX_SESSION_ATTACHMENT_BYTES,
    MAX_SESSION_ATTACHMENTS,
};
pub use checkpoint::{Change, Checkpoints, Kind, Restored, Snapshot};
pub use conflicts::{read as conflict_read, write as conflict_write};
pub use edit_flags::{EditModeHint, match_generated_glob, parse_toml_to_json};
pub use edit_ladder::DEFAULT_FUZZY_THRESHOLD;
pub use eval_flags::{
    DEFAULT_EVAL_MAX_CONCURRENCY, DynamicToolLookup, EvalDynamicToolsSlot, EvalRuntimeHints,
};
pub use find::{Chunk, FindOpts, Prefilter, prefilter};
pub use fs_edit::{
    EditOpts, check_generated_guard, edit_file_sync, edit_file_sync_with_opts, unified_diff,
    verify_diff_lines_seen, write_file_sync, write_file_sync_with_opts, write_target_sync,
    write_target_sync_with_opts,
};
pub use fs_read::{FileReadTracker, is_web_url, read_sync, read_sync_with_anchors, read_url};
pub use glob::{DEFAULT_LIMIT, GlobRequest, MAX_LIMIT, run_glob};
pub use grep::{GrepRequest, GrepStyle, run_grep};
pub use hashline::{HashlineEditor, HashlineOpts, header_paths, tag_of};
pub use scope::scope_files;
pub use snap::PngSnapRenderer;
pub use text_codec::{TextFile, normalize_eol, write_atomic};
pub use urls::{LocalScheme, SkillResolver, SkillScheme};
pub use web_fetch::PageFetcher;
pub use web_search::{DEFAULT_RESULTS, MAX_RESULTS, SearchBackend, WebSearchConfig, search_web};
pub use workspace_fs::StdWorkspaceFs;
