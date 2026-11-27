//! The protocol fronts of `hrn`: NDJSON protocols a host drives over stdio.
//!
//! `rpc` speaks omp's RPC wire format; `acp` speaks Zed's Agent Client Protocol. Both sit on
//! the same [`Backend`] seam, which the binary implements over its agent runtime, so neither front
//! depends on the runtime itself.

#[cfg(feature = "acp")]
mod acp;
mod backend;
mod events;
mod form_io;
mod history;
#[cfg(feature = "rpc")]
mod rpc;
mod wire;

#[cfg(test)]
mod test;

#[cfg(feature = "acp")]
pub use acp::{Options as AcpOptions, serve as serve_acp};
pub use backend::{
    Backend, CommandInfo, CompactionReport, ModelEntry, NewSession, SessionPatch, TurnRequest,
    TurnSummary, model_row_json,
};
#[cfg(feature = "rpc")]
pub use rpc::{Options as RpcOptions, serve as serve_rpc};
