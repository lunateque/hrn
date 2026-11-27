//! The async runtime as modules see it. Module crates may not depend on external crates, and
//! the runtime (tasks, timers, sockets, process and file I/O) is infrastructure every I/O-doing
//! module needs, so it enters the workspace here, at the edge, once.

pub use tokio;
