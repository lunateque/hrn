//! Loopback multi-agent HTTP+SSE daemon IPC, git worktree manager, and SSH-tunneled remoting.

pub mod auth;
pub mod bridge;
pub mod client;
pub mod collab;
pub mod mobile_web;
pub mod mobile_web_auth;
pub mod native_msg;
pub mod protocol;
pub mod qr;
pub mod relay;
pub mod relay_ws;
pub mod remote;
pub mod server;
pub mod worktree;

pub use auth::{
    HostIdentity, RunFile, ct_eq, ensure_host_identity, generate_token, read_run_file,
    remove_run_file_if_owned, sha256, sha256_hex, write_run_file,
};
pub use bridge::{BridgeServer, Reply, ToolBridge, call_bridge, tool_shim};
pub use client::{DaemonEventStream, IpcClient};
pub use mobile_web::{
    MOBILE_MANIFEST_JSON, MOBILE_PWA_HTML, MobileApproveRequest, MobileAskAnswerRequest,
    MobileHostInfo, MobileInfoResponse, MobileWebBackend, MobileWebHandle, MobileWebServer,
    PairRequest, PairResponse, SessionTranscriptDto,
};
pub use mobile_web_auth::{MobileRole, PairingStore, discover_lan_addrs, is_tailscale_ip};
pub use native_msg::{
    MAX_NATIVE_MESSAGE_BYTES, MAX_NATIVE_READ_BYTES, MAX_NATIVE_WRITE_BYTES, read_native_message,
    write_native_message, write_native_message_chunked,
};
pub use protocol::{
    ApproveRequest, CreateSessionRequest, DaemonEvent, ErrorResponse, HealthResponse, OkResponse,
    RestoreSessionRequest, RestoreSessionResponse, SessionSummaryDto, StartTurnRequest,
    SteerRequest,
};
pub use qr::{QrMatrix, render_half_blocks, render_half_blocks_inverted};
pub use relay_ws::{
    ServerWs, ServerWsEvent, ServerWsReader, ServerWsWriter, accept_loopback_ws, base64_decode,
    ws_accept_key,
};
pub use remote::{
    DoctorReport, HostTrustStore, MAX_SSH_READ_BYTES, RemoteFileRead, RemoteTarget, SshTunnel,
    TrustedHost, doctor_remote, fetch_remote_host_key, fetch_remote_run_file, list_dir, list_hosts,
    parse_host_spec, parse_remote_target, posix_single_quote, read_file, validate_fingerprint,
};
pub use server::{DaemonBackend, DaemonHandle, DaemonServer};
pub use worktree::{
    WorktreeInfo, add_worktree, add_worktree_on_branch, apply_patch, capture_patch, changed_files,
    check_diff_scope, commit_worktree_branch, is_onedrive_path, list_worktrees, remove_worktree,
};
