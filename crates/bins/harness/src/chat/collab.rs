//! `/share`, `/collab`, `/join`, and `/leave` live handlers on [`ChatBackend`].

use super::ChatBackend;
use super::remote::RemoteLiveHub;
use crate::collab::{CollabHostBridge, default_guest_name};
use hrn_edge_http::seal::CollabLink;
use hrn_edge_ipc::collab::{
    CollabGuest, CollabHost, GuestEvent, HostInbound, HostSnapshot, ReplicaConfig,
};
use hrn_edge_ipc::relay::AutoRelay;
use hrn_kernel::{AgentEvent, HrnError, Result};
use hrn_tui::{TaskEnd, TurnOutcome, UiEvent};
use std::sync::Arc;

impl Drop for ChatBackend {
    fn drop(&mut self) {
        self.shutdown_remote();
        self.close_collab_host("host closed session");
        if let Some(guest) = self.collab_guest.take() {
            guest.leave();
        }
    }
}

impl ChatBackend {
    /// `/share [--no-redact]`: build sealed share link asynchronously and report via `UiEvent::Report`.
    pub(super) fn share_session_live(&self, no_redact: bool) -> Result<String> {
        let Some(session_id) = self.active_session_id() else {
            return Err(HrnError::Store("no active session to share yet".to_owned()));
        };
        let store = Arc::clone(&self.store);
        let share_cfg = self.cfg.share.clone();
        let system_prompt = Some(self.runtime.system_prompt_for(&self.cwd, &self.control));
        let cwd = self.cwd.clone();
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            match crate::share::share_session(
                store.as_ref(),
                &share_cfg,
                &session_id,
                system_prompt,
                no_redact,
                &cwd,
            )
            .await
            {
                Ok((url, report)) => {
                    let note = if report.stripped_images
                        || report.string_cap.is_some()
                        || report.dropped_nodes > 0
                    {
                        format!(
                            " (trimmed: images={}, str_cap={:?}, dropped={})",
                            report.stripped_images, report.string_cap, report.dropped_nodes
                        )
                    } else {
                        String::new()
                    };
                    let _ = tx.send(UiEvent::Report(format!("share link: {url}{note}")));
                }
                Err(err) => {
                    let _ = tx.send(UiEvent::Report(format!("/share failed: {err}")));
                }
            }
        });
        Ok("sealing and uploading session snapshot…".to_owned())
    }

    /// `/collab [view|status|stop]`: start, inspect, or close a host-authoritative collab room.
    pub(super) fn collab_command_live(&mut self, arg: Option<&str>) -> Result<String> {
        let sub = arg.map(str::trim).unwrap_or_default();
        match sub {
            "status" => Ok(self.collab_status_report()),
            "stop" => {
                let cancelled = self.remote.cancel_pending();
                if self.collab_host.is_some() || self.auto_relay.is_some() || cancelled {
                    self.close_collab_host("host stopped collaboration");
                    Ok("collab room closed (goodbye sent)".to_owned())
                } else {
                    Ok("no active collab host room".to_owned())
                }
            }
            "" | "view" => self.start_or_show_collab_host(sub == "view"),
            other => Err(HrnError::Config(format!(
                "unknown /collab subcommand `{other}` · usage: /collab [view|status|stop]"
            ))),
        }
    }

    pub(super) fn start_or_show_collab_host(&mut self, view_only: bool) -> Result<String> {
        if let Some(bridge) = &self.collab_host {
            if !bridge.host.is_closed() {
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
                return Ok(format!(
                    "collab room `{}` ({kind}, {} guest(s)): {link}",
                    bridge.host.room(),
                    bridge.host.guest_count()
                ));
            }
            self.collab_host = None;
        }
        if self.collab_guest.is_some() {
            return Err(HrnError::Config(
                "already joined a collab room as a guest · run `/leave` first".to_owned(),
            ));
        }
        if self.needs_async_tunnel_start() {
            return self.start_async_collab_host(view_only);
        }
        self.start_collab_host_sync(view_only, true)
    }

    pub(super) fn start_collab_host_sync(
        &mut self,
        view_only: bool,
        allow_tunnel: bool,
    ) -> Result<String> {
        // Always ensure a session exists BEFORE starting `AutoRelay`: `session_for` calls
        // `set_session`, which closes any prior collab host + auto relay on session change.
        let session_id = match self.session.clone() {
            Some(id) => id,
            None => self.session_for("collab session")?,
        };
        let (connect_base, public_base, transport_note) =
            self.resolve_collab_relay_endpoints(allow_tunnel)?;
        let initial_nodes = self.session_branch()?.unwrap_or_default();
        let snap_store = Arc::clone(&self.store);
        let snap_sid = session_id.clone();
        let snap_control = self.control.clone();
        let snapshot_fn =
            Arc::new(move || build_host_snapshot(&snap_store, &snap_sid, &snap_control));
        let (host, inbound_rx) = self.handle.block_on(CollabHost::start_with_public_base(
            &connect_base,
            &public_base,
            snapshot_fn,
        ))?;
        spawn_host_inbound_pump(
            &self.handle,
            inbound_rx,
            self.tx.clone(),
            Arc::clone(&self.remote.hub),
        );
        let link = if view_only {
            host.view_only_link().to_owned()
        } else {
            host.interactive_link().to_owned()
        };
        let room = host.room().to_owned();
        let bridge = CollabHostBridge::new(host, session_id, &initial_nodes);
        self.remote.hub.set_collab_host(Some(bridge.clone()));
        self.collab_host = Some(bridge);
        let kind = if view_only {
            "view-only"
        } else {
            "interactive"
        };
        let suffix = transport_note
            .map(|label| format!(" · {label}"))
            .unwrap_or_default();
        Ok(format!(
            "collab room `{room}` open ({kind}{suffix}): {link}"
        ))
    }

    fn resolve_collab_relay_endpoints(
        &mut self,
        allow_tunnel: bool,
    ) -> Result<(String, String, Option<&'static str>)> {
        let configured = self.cfg.collab.relay.trim();
        if !configured.is_empty() {
            self.auto_relay = None;
            return Ok((configured.to_owned(), configured.to_owned(), None));
        }
        if let Some(auto) = &self.auto_relay {
            return Ok((
                auto.connect_url().to_owned(),
                auto.public_url().to_owned(),
                Some(auto.mode().short_label()),
            ));
        }
        let auto = if allow_tunnel {
            self.handle.block_on(AutoRelay::start())?
        } else {
            self.handle.block_on(AutoRelay::start_local_only())?
        };
        let connect = auto.connect_url().to_owned();
        let public = auto.public_url().to_owned();
        let label = auto.mode().short_label();
        self.auto_relay = Some(auto);
        Ok((connect, public, Some(label)))
    }

    /// `/join <link>`: connect as a guest and sync back-transcript + live events into a local
    /// `collab-of=<room>` replica session.
    pub(super) fn join_collab_live(&mut self, link_raw: &str) -> Result<String> {
        let link = CollabLink::parse(link_raw)?;
        self.close_collab_host("host joined another room");
        if let Some(old) = self.collab_guest.take() {
            old.leave();
        }
        let default_relay =
            (!self.cfg.collab.relay.is_empty()).then_some(self.cfg.collab.relay.as_str());
        let guest_name = default_guest_name();
        let replica = ReplicaConfig {
            store: Arc::clone(&self.store),
            cwd: self.cwd.to_string_lossy().into_owned(),
        };
        let (guest, mut ev_rx) = self.handle.block_on(CollabGuest::connect(
            link,
            default_relay,
            &guest_name,
            Some(replica),
        ))?;
        let room = guest.room().to_owned();
        let can_write = guest.can_write();
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                match ev {
                    GuestEvent::SnapshotReady {
                        meta,
                        nodes,
                        replica_session_id,
                    } => {
                        let mode = if can_write {
                            "interactive"
                        } else {
                            "view-only"
                        };
                        let _ = tx.send(UiEvent::SessionLoaded {
                            meta: Box::new(meta),
                            nodes,
                            notice: Some(format!(
                                "[collab] synced replica `{replica_session_id}` ({mode})"
                            )),
                        });
                    }
                    GuestEvent::Agent(agent_ev) => {
                        let turn_end = match &agent_ev {
                            AgentEvent::TurnDone { .. } => Some(TurnOutcome::Done),
                            AgentEvent::TurnError { code, message, .. } => {
                                if code == "cancelled" {
                                    Some(TurnOutcome::Interrupted)
                                } else {
                                    Some(TurnOutcome::Failed(message.clone()))
                                }
                            }
                            _ => None,
                        };
                        let _ = tx.send(UiEvent::Agent(agent_ev));
                        if let Some(outcome) = turn_end {
                            let _ = tx.send(UiEvent::Finished(TaskEnd::Turn {
                                outcome,
                                unsent_steers: Vec::new(),
                            }));
                        }
                    }
                    GuestEvent::Error { message } => {
                        let _ = tx.send(UiEvent::Report(format!("[collab error] {message}")));
                    }
                    GuestEvent::Goodbye { reason } => {
                        let _ = tx.send(UiEvent::Report(format!(
                            "[collab] host closed room: {reason}"
                        )));
                        break;
                    }
                    GuestEvent::Disconnected { reason } => {
                        let _ =
                            tx.send(UiEvent::Report(format!("[collab] disconnected: {reason}")));
                        break;
                    }
                    GuestEvent::Welcome { .. }
                    | GuestEvent::SnapshotChunk { .. }
                    | GuestEvent::Entry { .. }
                    | GuestEvent::State { .. } => {}
                }
            }
        });
        self.collab_guest = Some(guest);
        let mode = if can_write {
            "interactive"
        } else {
            "view-only"
        };
        Ok(format!("joined collab room `{room}` ({mode})"))
    }

    /// `/leave`: disconnect from the active guest collaboration room.
    pub(super) fn leave_collab_live(&mut self) -> String {
        if let Some(guest) = self.collab_guest.take() {
            let room = guest.room().to_owned();
            guest.leave();
            return format!("left collab room `{room}`");
        }
        if self.collab_host.is_some() {
            self.close_collab_host("host left collaboration");
            return "stopped hosting collab room (goodbye sent)".to_owned();
        }
        "not in a collab room".to_owned()
    }

    /// Close any active collab host room cleanly (schedules `HostFrame::Goodbye` without blocking)
    /// and stop any zero-config embedded relay + HTTPS tunnel or in-flight startup task.
    pub(super) fn close_collab_host(&mut self, reason: &str) {
        let _ = self.remote.cancel_pending();
        self.close_collab_host_keep_relay(reason);
        self.auto_relay = None;
    }

    /// Close only the active collab room while preserving a running [`AutoRelay`] tunnel for fast
    /// key/room rotation.
    pub(super) fn close_collab_host_keep_relay(&mut self, reason: &str) {
        self.remote.hub.set_collab_host(None);
        if let Some(bridge) = self.collab_host.take() {
            bridge.host.close(reason.to_owned());
        }
    }

    /// Forward a guest TUI prompt to the collab host when joined as a guest.
    pub(super) fn submit_collab_guest(&mut self, prompt: &str) -> Result<bool> {
        let Some(guest) = &self.collab_guest else {
            return Ok(false);
        };
        if guest.is_closed() {
            self.collab_guest = None;
            return Ok(false);
        }
        guest.send_prompt(prompt)?;
        Ok(true)
    }

    /// Forward an interrupt (`Esc`) to the collab host when joined as an interactive guest.
    pub(super) fn interrupt_collab_guest(&mut self) -> bool {
        let Some(guest) = &self.collab_guest else {
            return false;
        };
        if guest.is_closed() {
            self.collab_guest = None;
            return false;
        }
        let _ = guest.send_abort();
        true
    }

    /// Whether the backend is currently joined to a view-only collaboration room.
    pub(super) fn collab_guest_read_only_live(&self) -> bool {
        self.collab_guest
            .as_ref()
            .is_some_and(|g| !g.is_closed() && !g.can_write())
    }

    fn collab_status_report(&self) -> String {
        if let Some(bridge) = &self.collab_host
            && !bridge.host.is_closed()
        {
            return format!(
                "hosting collab room `{}` · {} guest(s)\ninteractive: {}\nview-only:   {}",
                bridge.host.room(),
                bridge.host.guest_count(),
                bridge.host.interactive_link(),
                bridge.host.view_only_link(),
            );
        }
        if let Some(guest) = &self.collab_guest
            && !guest.is_closed()
        {
            let mode = if guest.can_write() {
                "interactive"
            } else {
                "view-only"
            };
            return format!(
                "joined collab room `{}` ({mode}) · replica `{}`",
                guest.room(),
                guest.replica_session_id()
            );
        }
        "collab is inactive · `/collab` hosts a room or `/join <link>` joins one".to_owned()
    }
}

pub(super) fn spawn_host_inbound_pump(
    handle: &tokio::runtime::Handle,
    mut inbound_rx: tokio::sync::mpsc::Receiver<HostInbound>,
    tx: hrn_tui::UiSender,
    remote_hub: Arc<RemoteLiveHub>,
) {
    handle.spawn(async move {
        while let Some(ev) = inbound_rx.recv().await {
            match ev {
                HostInbound::GuestJoined {
                    name, can_write, ..
                } => {
                    let mode = if can_write {
                        "interactive"
                    } else {
                        "view-only"
                    };
                    let _ = tx.send(UiEvent::Report(format!(
                        "[collab] guest `{name}` joined ({mode})"
                    )));
                }
                HostInbound::GuestLeft { name, .. } => {
                    let _ = tx.send(UiEvent::Report(format!("[collab] guest `{name}` left")));
                }
                HostInbound::Prompt { badged_text, .. } => {
                    let _ = tx.send(UiEvent::CollabPrompt { badged_text });
                }
                HostInbound::Steer { text, .. } => {
                    let _ = tx.send(UiEvent::RemoteSteer { text });
                }
                HostInbound::Approve {
                    call_id,
                    approved,
                    always,
                    ..
                } => {
                    if let Err(err) =
                        remote_hub.resolve_approval_from_remote(&call_id, approved, always)
                    {
                        let _ = tx.send(UiEvent::Report(format!("[remote approval] {err}")));
                    }
                }
                HostInbound::AskAnswer { id, answers, .. } => {
                    if let Err(err) = remote_hub.resolve_ask_from_remote(&id, &answers) {
                        let _ = tx.send(UiEvent::Report(format!("[remote ask] {err}")));
                    }
                }
                HostInbound::Abort { guest_name, .. } => {
                    let _ = tx.send(UiEvent::CollabAbort { guest_name });
                }
                HostInbound::Disconnected { reason } => {
                    let _ = tx.send(UiEvent::Report(format!("[collab] room closed: {reason}")));
                    break;
                }
            }
        }
    });
}

pub(super) fn build_host_snapshot(
    store: &Arc<dyn hrn_kernel::SessionStore>,
    session_id: &str,
    control: &hrn_harness::SessionControl,
) -> HostSnapshot {
    let Some(meta) = store.get_session(session_id).ok().flatten() else {
        return HostSnapshot {
            session_id: session_id.to_owned(),
            ..HostSnapshot::default()
        };
    };
    let nodes = match meta.head_node_id.as_deref() {
        Some(head) => store.load_branch(&meta.id, head).unwrap_or_default(),
        None => Vec::new(),
    };
    HostSnapshot {
        session_id: meta.id,
        title: meta.title,
        model: meta.model,
        head_node_id: meta.head_node_id,
        busy: control.is_running(),
        nodes,
    }
}
