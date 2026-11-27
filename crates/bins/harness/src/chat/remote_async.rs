//! `/remote` command dispatch, non-blocking HTTPS tunnel startup, QR view building, and teardown.

use super::ChatBackend;
use super::collab::{build_host_snapshot, spawn_host_inbound_pump};
use super::remote::{ActiveTransport, lock};
use crate::collab::CollabHostBridge;
use crate::remote_web::build_display_urls;
use hrn_edge_ipc::collab::{CollabHost, HostInbound};
use hrn_edge_ipc::relay::{AutoRelay, cloudflared_available};
use hrn_edge_ipc::{MobileRole, MobileWebBackend, MobileWebServer, QrMatrix};
use hrn_kernel::{DagNode, HrnError, Result};
use hrn_tui::{RemoteCommandOutcome, RemoteSessionView, UiEvent};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_PAIR_TTL: Duration = Duration::from_mins(10);
const STARTING_REMOTE_NOTICE: &str =
    "starting remote HTTPS tunnel (waiting for DNS & health check)…";
const STARTING_COLLAB_NOTICE: &str =
    "starting collab HTTPS tunnel (waiting for DNS & health check)…";

#[derive(Clone, Copy, Debug)]
enum AsyncTarget {
    RemoteQr { control: bool },
    CollabNotice { view_only: bool },
}

pub(super) struct PendingRemoteFinish {
    generation: u64,
    outcome: AsyncRemoteOutcome,
}

struct AsyncRemoteReady {
    auto: AutoRelay,
    host: CollabHost,
    inbound_rx: tokio::sync::mpsc::Receiver<HostInbound>,
    session_id: String,
    initial_nodes: Vec<DagNode>,
    target: AsyncTarget,
}

enum AsyncRemoteOutcome {
    Ready(Box<AsyncRemoteReady>),
    FallbackDirect { role: MobileRole, reason: String },
    Failed(HrnError),
}

impl ChatBackend {
    /// Whether starting a relay room needs a slow background `cloudflared` tunnel launch.
    pub(super) fn needs_async_tunnel_start(&self) -> bool {
        self.cfg.collab.relay.trim().is_empty()
            && self.auto_relay.is_none()
            && cloudflared_available()
    }

    /// `/remote [view|lan|lan view|relay|relay view|status|stop]` live handler.
    pub(super) fn remote_command_live(
        &mut self,
        arg: Option<&str>,
    ) -> Result<RemoteCommandOutcome> {
        let raw = arg.map(str::trim).unwrap_or_default();
        let tokens: Vec<String> = raw
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        let parts: Vec<&str> = tokens.iter().map(String::as_str).collect();
        match parts.as_slice() {
            [] | ["control"] => self.open_default_remote(true),
            ["view" | "readonly" | "read-only"] => self.open_default_remote(false),
            ["lan" | "local"] | ["lan" | "local", "control"] => {
                self.open_direct_remote(MobileRole::Control, "LAN / Tailscale")
            }
            ["lan" | "local", "view" | "readonly" | "read-only"] => {
                self.open_direct_remote(MobileRole::View, "LAN / Tailscale")
            }
            ["relay"] | ["relay", "control"] => self.open_relay_remote(true, false, false),
            ["relay", "view" | "readonly" | "read-only"] => {
                self.open_relay_remote(false, false, false)
            }
            ["status"] => Ok(self.remote_status_live()),
            ["stop"] => Ok(RemoteCommandOutcome::Notice(self.remote_stop_live())),
            _ => Err(HrnError::Config(format!(
                "unknown /remote argument `{raw}` · usage: /remote [view|lan|relay|status|stop]"
            ))),
        }
    }

    /// Default `/remote` / `/remote view`: prefer an HTTPS E2E tunnel when configured or when
    /// `cloudflared` is installed (falling back cleanly to LAN/Tailscale if the tunnel fails).
    fn open_default_remote(&mut self, control: bool) -> Result<RemoteCommandOutcome> {
        let relay_alive = self
            .collab_host
            .as_ref()
            .is_some_and(|b| !b.host.is_closed());
        if relay_alive
            || self.remote.is_starting()
            || !self.cfg.collab.relay.trim().is_empty()
            || self.auto_relay.is_some()
            || cloudflared_available()
        {
            return self.open_relay_remote(control, false, true);
        }
        let role = if control {
            MobileRole::Control
        } else {
            MobileRole::View
        };
        self.open_direct_remote(role, "LAN / Tailscale")
    }

    /// Start `/collab` asynchronously when a cold `cloudflared` tunnel is required.
    pub(super) fn start_async_collab_host(&mut self, view_only: bool) -> Result<String> {
        if self.remote.is_starting() {
            return Ok(STARTING_COLLAB_NOTICE.to_owned());
        }
        self.spawn_async_tunnel_room(
            "collab session",
            AsyncTarget::CollabNotice { view_only },
            false,
        )?;
        Ok(STARTING_COLLAB_NOTICE.to_owned())
    }

    /// Apply a completed background tunnel startup delivered via [`UiEvent::RemoteReady`].
    pub(super) fn finish_remote_live(&mut self) -> Option<Result<RemoteCommandOutcome>> {
        let finished = lock(&self.remote.pending_result).take()?;
        self.remote.pending_task = None;
        if finished.generation != self.remote.pending_gen {
            return None;
        }
        match finished.outcome {
            AsyncRemoteOutcome::Ready(ready) => {
                let AsyncRemoteReady {
                    auto,
                    host,
                    inbound_rx,
                    session_id,
                    initial_nodes,
                    target,
                } = *ready;
                if self.session.as_deref() != Some(session_id.as_str()) {
                    host.close("host switched session".to_owned());
                    drop(auto);
                    return None;
                }
                spawn_host_inbound_pump(
                    &self.handle,
                    inbound_rx,
                    self.tx.clone(),
                    Arc::clone(&self.remote.hub),
                );
                let mode_short = auto.mode().short_label();
                let bridge = CollabHostBridge::new(host, session_id, &initial_nodes);
                self.remote.hub.set_collab_host(Some(bridge.clone()));
                self.collab_host = Some(bridge);
                self.auto_relay = Some(auto);
                match target {
                    AsyncTarget::RemoteQr { control } => {
                        self.remote.relay_owned = true;
                        Some(
                            self.build_relay_view(control)
                                .map(RemoteCommandOutcome::Open),
                        )
                    }
                    AsyncTarget::CollabNotice { view_only } => {
                        let Some(bridge) = self.collab_host.as_ref() else {
                            return Some(Err(HrnError::Config(
                                "collab relay room unavailable".to_owned(),
                            )));
                        };
                        let link = if view_only {
                            bridge.host.view_only_link()
                        } else {
                            bridge.host.interactive_link()
                        };
                        let kind = if view_only {
                            "view-only"
                        } else {
                            "interactive"
                        };
                        Some(Ok(RemoteCommandOutcome::Notice(format!(
                            "collab room `{}` open ({kind} · {mode_short}): {link}",
                            bridge.host.room()
                        ))))
                    }
                }
            }
            AsyncRemoteOutcome::FallbackDirect { role, reason } => {
                let _ = self.tx.send(UiEvent::Report(format!(
                    "HTTPS tunnel unavailable ({reason}) · fell back to LAN / Tailscale"
                )));
                Some(self.open_direct_remote(role, "LAN / Tailscale (fallback)"))
            }
            AsyncRemoteOutcome::Failed(err) => Some(Err(err)),
        }
    }

    /// Rotate the active `/remote` QR code or toggle between control and view-only access.
    pub(super) fn remote_rotate_live(&mut self, control: bool) -> Result<RemoteSessionView> {
        if self.remote.active_transport == ActiveTransport::Relay
            && self
                .collab_host
                .as_ref()
                .is_some_and(|b| !b.host.is_closed())
        {
            let same_role = self
                .remote
                .last_view
                .as_ref()
                .is_some_and(|v| v.can_control == control);
            return match self.open_relay_remote(control, same_role, false)? {
                RemoteCommandOutcome::Open(view) => Ok(view),
                RemoteCommandOutcome::Notice(msg) => Err(HrnError::Config(msg)),
            };
        }
        let role = if control {
            MobileRole::Control
        } else {
            MobileRole::View
        };
        match self.open_direct_remote(role, "LAN / Tailscale")? {
            RemoteCommandOutcome::Open(view) => Ok(view),
            RemoteCommandOutcome::Notice(msg) => Err(HrnError::Config(msg)),
        }
    }

    /// Revoke all remote mobile pairings and shut down active `/remote` transports.
    pub(super) fn remote_stop_live(&mut self) -> String {
        let mut stopped = self.remote.cancel_pending();
        if let Some(web) = self.remote.web.take() {
            web.revoke_all();
            self.handle.block_on(web.shutdown());
            stopped = true;
        }
        if self.remote.relay_owned && (self.collab_host.is_some() || self.auto_relay.is_some()) {
            self.close_collab_host("host stopped /remote");
            stopped = true;
        }
        self.remote.relay_owned = false;
        self.remote.active_transport = ActiveTransport::None;
        self.remote.last_view = None;
        if stopped {
            "remote stopped (all mobile pairings revoked)".to_owned()
        } else {
            "remote is not running".to_owned()
        }
    }

    /// Non-blocking shutdown invoked from `Drop for ChatBackend`.
    pub(super) fn shutdown_remote(&mut self) {
        let _ = self.remote.cancel_pending();
        if let Some(web) = self.remote.web.take() {
            web.revoke_all();
            self.handle.spawn(async move {
                web.shutdown().await;
            });
        }
        self.remote.hub.set_collab_host(None);
    }

    fn open_direct_remote(
        &mut self,
        role: MobileRole,
        transport_label: &str,
    ) -> Result<RemoteCommandOutcome> {
        let _ = self.remote.cancel_pending();
        if self.session.is_none() {
            let _ = self.session_for("remote session")?;
        }
        if self.remote.web.is_none() {
            let addr = SocketAddr::from(([0, 0, 0, 0], 0));
            let backend: Arc<dyn MobileWebBackend> =
                Arc::clone(&self.remote.hub) as Arc<dyn MobileWebBackend>;
            let web = self.handle.block_on(MobileWebServer::bind(addr, backend))?;
            self.remote.web = Some(web);
        }
        let Some(web) = self.remote.web.as_ref() else {
            return Err(HrnError::Internal(
                "mobile web server unavailable".to_owned(),
            ));
        };
        let code = web.create_pairing(role, DEFAULT_PAIR_TTL)?;
        let (url, alt_urls) = build_display_urls(web.addr(), None, &format!("pair={code}"));
        let qr_lines = QrMatrix::encode(&url)?.render_half_blocks_lines(2, false);
        let can_control = role == MobileRole::Control;
        let view = RemoteSessionView {
            mode_label: if can_control {
                "control".to_owned()
            } else {
                "view-only".to_owned()
            },
            transport_label: transport_label.to_owned(),
            url,
            qr_lines,
            connected: web.connected_count(),
            can_control,
            alt_urls,
        };
        self.remote.active_transport = ActiveTransport::Direct;
        self.remote.last_view = Some(view.clone());
        Ok(RemoteCommandOutcome::Open(view))
    }

    fn open_relay_remote(
        &mut self,
        control: bool,
        rotate_room: bool,
        auto_lan_fallback: bool,
    ) -> Result<RemoteCommandOutcome> {
        if self.collab_guest.is_some() {
            return Err(HrnError::Config(
                "already joined a collab room as a guest · run `/leave` first".to_owned(),
            ));
        }
        if rotate_room && self.collab_host.is_some() {
            self.close_collab_host_keep_relay("host rotated /remote relay key");
        }
        let already_open = self
            .collab_host
            .as_ref()
            .is_some_and(|b| !b.host.is_closed());
        if !already_open {
            if self.needs_async_tunnel_start() {
                if self.remote.is_starting() {
                    return Ok(RemoteCommandOutcome::Notice(
                        STARTING_REMOTE_NOTICE.to_owned(),
                    ));
                }
                self.spawn_async_tunnel_room(
                    "remote session",
                    AsyncTarget::RemoteQr { control },
                    auto_lan_fallback,
                )?;
                return Ok(RemoteCommandOutcome::Notice(
                    STARTING_REMOTE_NOTICE.to_owned(),
                ));
            }
            let _ = self.start_collab_host_sync(!control, false)?;
            self.remote.relay_owned = true;
        }
        self.build_relay_view(control)
            .map(RemoteCommandOutcome::Open)
    }

    fn spawn_async_tunnel_room(
        &mut self,
        session_title: &str,
        target: AsyncTarget,
        auto_lan_fallback: bool,
    ) -> Result<()> {
        // Ensure the session exists FIRST so `set_session` never races the background relay.
        let session_id = match self.session.clone() {
            Some(id) => id,
            None => self.session_for(session_title)?,
        };
        let initial_nodes = self.session_branch()?.unwrap_or_default();
        let snap_store = Arc::clone(&self.store);
        let snap_sid = session_id.clone();
        let snap_control = self.control.clone();
        let snapshot_fn =
            Arc::new(move || build_host_snapshot(&snap_store, &snap_sid, &snap_control));

        self.remote.pending_gen = self.remote.pending_gen.wrapping_add(1);
        let generation = self.remote.pending_gen;
        *lock(&self.remote.pending_result) = None;
        let result_slot = Arc::clone(&self.remote.pending_result);
        let tx = self.tx.clone();

        let task = self.handle.spawn(async move {
            let outcome = match if auto_lan_fallback {
                AutoRelay::start_cloudflared().await
            } else {
                AutoRelay::start().await
            } {
                Ok(auto) => {
                    match CollabHost::start_with_public_base(
                        auto.connect_url(),
                        auto.public_url(),
                        snapshot_fn,
                    )
                    .await
                    {
                        Ok((host, inbound_rx)) => {
                            AsyncRemoteOutcome::Ready(Box::new(AsyncRemoteReady {
                                auto,
                                host,
                                inbound_rx,
                                session_id,
                                initial_nodes,
                                target,
                            }))
                        }
                        Err(err) => AsyncRemoteOutcome::Failed(err),
                    }
                }
                Err(err) if auto_lan_fallback => {
                    let role = match target {
                        AsyncTarget::RemoteQr { control: true } => MobileRole::Control,
                        AsyncTarget::RemoteQr { control: false }
                        | AsyncTarget::CollabNotice { .. } => MobileRole::View,
                    };
                    AsyncRemoteOutcome::FallbackDirect {
                        role,
                        reason: err.to_string(),
                    }
                }
                Err(err) => AsyncRemoteOutcome::Failed(err),
            };
            *lock(&result_slot) = Some(PendingRemoteFinish {
                generation,
                outcome,
            });
            let _ = tx.send(UiEvent::RemoteReady);
        });
        self.remote.pending_task = Some(task);
        Ok(())
    }

    fn build_relay_view(&mut self, control: bool) -> Result<RemoteSessionView> {
        let Some(bridge) = self.collab_host.as_ref() else {
            return Err(HrnError::Config("collab relay room unavailable".to_owned()));
        };
        let url = if control {
            bridge.host.interactive_link().to_owned()
        } else {
            bridge.host.view_only_link().to_owned()
        };
        let qr_lines = QrMatrix::encode(&url)?.render_half_blocks_lines(2, false);
        let (transport_label, alt_urls) = match &self.auto_relay {
            Some(auto) => {
                let alts = auto
                    .alt_bases()
                    .iter()
                    .filter_map(|base| bridge.host.link_with_base(base, control).ok())
                    .collect();
                (auto.mode().label().to_owned(), alts)
            }
            None => ("E2E Relay".to_owned(), Vec::new()),
        };
        let view = RemoteSessionView {
            mode_label: if control {
                "control".to_owned()
            } else {
                "view-only".to_owned()
            },
            transport_label,
            url,
            qr_lines,
            connected: bridge.host.guest_count(),
            can_control: control,
            alt_urls,
        };
        self.remote.active_transport = ActiveTransport::Relay;
        self.remote.last_view = Some(view.clone());
        Ok(view)
    }

    fn remote_status_live(&mut self) -> RemoteCommandOutcome {
        if self.remote.is_starting() {
            return RemoteCommandOutcome::Notice(STARTING_REMOTE_NOTICE.to_owned());
        }
        match self.remote.active_transport {
            ActiveTransport::Direct => {
                if let (Some(web), Some(mut view)) =
                    (self.remote.web.as_ref(), self.remote.last_view.clone())
                {
                    view.connected = web.connected_count();
                    self.remote.last_view = Some(view.clone());
                    return RemoteCommandOutcome::Open(view);
                }
            }
            ActiveTransport::Relay => {
                if let (Some(bridge), Some(mut view)) =
                    (self.collab_host.as_ref(), self.remote.last_view.clone())
                    && !bridge.host.is_closed()
                {
                    view.connected = bridge.host.guest_count();
                    self.remote.last_view = Some(view.clone());
                    return RemoteCommandOutcome::Open(view);
                }
            }
            ActiveTransport::None => {}
        }
        RemoteCommandOutcome::Notice(
            "remote is inactive · run `/remote` (HTTPS tunnel) or `/remote lan` (LAN/Tailscale)"
                .to_owned(),
        )
    }
}
