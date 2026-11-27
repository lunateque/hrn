//! Test support: re-exports the kernel's temp dir and no-op tool context.

pub(crate) use hrn_kernel::testing::{TempDir, tool_context as test_ctx};
