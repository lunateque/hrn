//! `hrn-relay`: optional local/LAN relay server for sealed share blobs and live collab rooms.

use clap::Parser;
use hrn_kernel::{HrnError, Result};
use hrn_relay::{
    DEFAULT_MAX_BLOB_BYTES, DEFAULT_MAX_BLOBS, DEFAULT_MAX_CONNECTIONS,
    DEFAULT_MAX_GUESTS_PER_ROOM, DEFAULT_MAX_ROOMS, DEFAULT_MAX_TOTAL_BLOB_BYTES,
    DEFAULT_MAX_WS_FRAME_BYTES, DEFAULT_ROOM_IDLE_TIMEOUT, DEFAULT_SEND_QUEUE_CAP, RelayConfig,
    parse_ttl, serve,
};
use std::io::Write as _;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;
use tokio::net::TcpListener;

/// Optional local/LAN relay for `hrn` sealed shares and live collaboration rooms.
#[derive(Debug, Parser)]
#[command(name = "hrn-relay", version, about)]
struct Cli {
    /// Listen address (default loopback `127.0.0.1:8787`; pass `0.0.0.0:8787` for LAN).
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: String,
    /// Retention TTL for uploaded sealed share blobs (e.g. `30d`, `24h`, `60m`).
    #[arg(long, default_value = "30d")]
    blob_ttl: String,
    /// Maximum size of a single blob in bytes (default 1 MiB).
    #[arg(long, default_value_t = DEFAULT_MAX_BLOB_BYTES)]
    max_blob_bytes: usize,
    /// Maximum aggregate memory across all blobs in bytes (default 64 MiB).
    #[arg(long, default_value_t = DEFAULT_MAX_TOTAL_BLOB_BYTES)]
    max_total_blob_bytes: usize,
    /// Maximum stored blob count.
    #[arg(long, default_value_t = DEFAULT_MAX_BLOBS)]
    max_blobs: usize,
    /// Maximum concurrent collaboration rooms.
    #[arg(long, default_value_t = DEFAULT_MAX_ROOMS)]
    max_rooms: usize,
    /// Maximum concurrent guests per room.
    #[arg(long, default_value_t = DEFAULT_MAX_GUESTS_PER_ROOM)]
    max_guests_per_room: usize,
    /// Maximum WebSocket frame payload in bytes (default 256 KiB).
    #[arg(long, default_value_t = DEFAULT_MAX_WS_FRAME_BYTES)]
    max_ws_frame_bytes: usize,
    /// Idle room timeout in seconds before automatic teardown.
    #[arg(long, default_value_t = DEFAULT_ROOM_IDLE_TIMEOUT.as_secs())]
    room_idle_secs: u64,
    /// Maximum concurrent TCP connections accepted by the relay.
    #[arg(long, default_value_t = DEFAULT_MAX_CONNECTIONS)]
    max_connections: usize,
}

fn write_stdout(line: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

fn write_stderr(line: &str) {
    let mut err = std::io::stderr();
    let _ = err.write_all(line.as_bytes());
    let _ = err.flush();
}

async fn run(cli: Cli) -> Result<()> {
    let addr: SocketAddr = cli
        .listen
        .parse()
        .map_err(|e| HrnError::Config(format!("invalid --listen `{}`: {e}", cli.listen)))?;
    let blob_ttl = parse_ttl(&cli.blob_ttl)?;
    let cfg = RelayConfig {
        max_blob_bytes: cli.max_blob_bytes.max(1024),
        max_total_blob_bytes: cli.max_total_blob_bytes.max(cli.max_blob_bytes),
        max_blobs: cli.max_blobs.max(1),
        blob_ttl,
        max_rooms: cli.max_rooms.max(1),
        max_guests_per_room: cli.max_guests_per_room.max(1),
        max_ws_frame_bytes: cli.max_ws_frame_bytes.max(4096),
        send_queue_cap: DEFAULT_SEND_QUEUE_CAP,
        room_idle_timeout: Duration::from_secs(cli.room_idle_secs.max(10)),
        max_connections: cli.max_connections.max(1),
        on_frame: None,
    };
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| HrnError::Io(format!("bind `{addr}`: {e}")))?;
    let handle = serve(listener, cfg).await?;
    write_stdout(&format!(
        "hrn-relay listening on {}\nPress Ctrl+C to stop.\n",
        handle.base_url()
    ));
    let _ = tokio::signal::ctrl_c().await;
    write_stdout("shutting down hrn-relay...\n");
    handle.shutdown().await;
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            write_stderr(&format!("error: runtime init failed: {err}\n"));
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            write_stderr(&format!("error: {err}\n"));
            ExitCode::FAILURE
        }
    }
}
