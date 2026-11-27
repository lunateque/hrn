//! `hrn remote serve | host-key | print-run-file | trust | hosts | setup | doctor | forget`.

use crate::config::Config;
use crate::daemon;
use crate::term::{err_line, out};
use hrn_edge_ipc::{
    HostTrustStore, doctor_remote, ensure_host_identity, fetch_remote_host_key, parse_host_spec,
    read_run_file,
};
use hrn_kernel::Result;
use std::path::Path;

/// `hrn remote serve [--listen 127.0.0.1:PORT]`: start the loopback daemon for SSH tunneling.
pub(crate) async fn serve(
    cfg: &Config,
    db_path: &Path,
    run_file_path: &Path,
    host_key_path: &Path,
    listen: &str,
) -> Result<()> {
    daemon::run_daemon(cfg, db_path, run_file_path, host_key_path, listen).await
}

/// `hrn remote host-key`: print this host's persistent `SHA256:<hex>` daemon fingerprint.
pub(crate) fn host_key(host_key_path: &Path) -> Result<()> {
    let identity = ensure_host_identity(host_key_path)?;
    out(&format!("{}\n", identity.fingerprint));
    Ok(())
}

/// `hrn remote print-run-file`: print the local `run.json` to stdout (invoked over SSH stdout by
/// remote clients so bearer tokens never appear on `argv`).
pub(crate) fn print_run_file(run_file_path: &Path) -> Result<()> {
    let rf = read_run_file(run_file_path)?;
    let json = serde_json::to_string(&rf)?;
    out(&format!("{json}\n"));
    Ok(())
}

/// `hrn remote trust <host[:port]> <SHA256:...>`: pin a remote daemon's host-key fingerprint.
pub(crate) fn trust(trust_store_path: &Path, host: &str, fingerprint: &str) -> Result<()> {
    let mut store = HostTrustStore::load(trust_store_path)?;
    let entry = store.trust(host, fingerprint)?;
    store.save(trust_store_path)?;
    out(&format!("trusted `{}` ({})\n", entry.host, entry.host_key));
    Ok(())
}

/// `hrn remote hosts`: list all pinned remote daemon hosts.
pub(crate) fn hosts(trust_store_path: &Path) -> Result<()> {
    let store = HostTrustStore::load(trust_store_path)?;
    let list = store.list();
    if list.is_empty() {
        out(
            "no trusted remote hosts (use `hrn remote setup <host>` or `hrn remote trust <host> <SHA256:...>`) \n",
        );
        return Ok(());
    }
    out("HOST\tHOST_KEY\n");
    for h in list {
        out(&format!("{}\t{}\n", h.host, h.host_key));
    }
    Ok(())
}

/// `hrn remote forget <host[:port]>`: remove a host from the trust store.
pub(crate) fn forget(trust_store_path: &Path, host: &str) -> Result<()> {
    let mut store = HostTrustStore::load(trust_store_path)?;
    if store.forget(host)? {
        store.save(trust_store_path)?;
        out(&format!("forgot `{host}`\n"));
    } else {
        out(&format!("host `{host}` was not in trust store\n"));
    }
    Ok(())
}

/// `hrn remote setup <[user@]host[:port]>`: query remote `hrn remote host-key` over SSH and pin it.
pub(crate) async fn setup(trust_store_path: &Path, spec: &str) -> Result<()> {
    let target = parse_host_spec(spec)?;
    let fp = fetch_remote_host_key(&target).await?;
    let mut store = HostTrustStore::load(trust_store_path)?;
    let entry = store.trust(&target.host_id(), &fp)?;
    store.save(trust_store_path)?;
    out(&format!(
        "pinned `{}` -> {}\nNext: run `hrn remote serve` on `{}` and `hrn attach ssh://{}` locally.\n",
        entry.host,
        entry.host_key,
        target.ssh_dest(),
        target.ssh_dest()
    ));
    Ok(())
}

/// `hrn remote doctor <[user@]host[:port]>`: verify SSH, remote `hrn`, trust pin, and daemon.
pub(crate) async fn doctor(trust_store_path: &Path, spec: &str) -> Result<()> {
    let target = parse_host_spec(spec)?;
    let store = HostTrustStore::load(trust_store_path)?;
    let report = doctor_remote(&target, &store).await;
    out(&format!(
        "remote doctor: {}\n  ssh on PATH: {}\n  remote hrn: {}\n  remote host-key: {}\n  host-key pinned: {}\n  daemon active: {}\n",
        target.ssh_dest(),
        if report.ssh_found { "ok" } else { "missing" },
        report.remote_version.as_deref().unwrap_or("unreachable"),
        report.remote_host_key.as_deref().unwrap_or("unknown"),
        if report.host_key_trusted { "yes" } else { "no" },
        if report.daemon_running { "yes" } else { "no" },
    ));
    for note in &report.notes {
        err_line(&format!("  note: {note}"));
    }
    Ok(())
}
