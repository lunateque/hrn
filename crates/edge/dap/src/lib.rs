//! Debug Adapter Protocol client: `Content-Length` framing, request/response/event correlation
//! over stdio or TCP, adapter descriptors with detection and selection, and debug sessions
//! that survive between tool calls.

pub mod breakpoints;
pub mod catalog;
pub mod client;
pub mod frame;
pub mod ops;
pub mod reverse;
pub mod session;
pub mod state;

pub use breakpoints::{Breakpoint, BreakpointResult, Edit, Kind};
pub use catalog::{AttachTarget, Catalog, Connect, ProgramKind, Resolved, Selection};
pub use client::{DapClient, Inbound, PendingResponse, SpawnSpec};
pub use frame::{read_message, write_message};
pub use session::{Outcome, Resume, RunState, Session, StartRequest, StartSpec, Summary};
pub use state::Status;
