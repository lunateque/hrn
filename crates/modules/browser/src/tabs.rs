//! Named browser tab management, backend selection (`relay` / `cdp_url` / managed), and
//! owned vs. borrowed tab lifecycle semantics.

use crate::cdp::CdpClient;
use crate::launcher::{BrowserConfig, LaunchedBrowser, attach_cdp, launch_managed};
use crate::relay::{RelayState, read_relay_state};
use hrn_edge_rt::tokio;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{HrnError, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Mutex;

const RELAY_TIMEOUT: Duration = Duration::from_secs(15);

/// Resolved backend target for opening or borrowing a browser tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BackendTarget {
    /// Connect through the native Chrome extension relay (`<data>/browser-relay.json`).
    Relay,
    /// Connect directly to a loopback CDP HTTP/WebSocket URL.
    CdpUrl(String),
    /// Launch a managed headless/headed browser executable.
    Managed(Option<PathBuf>),
}

/// Resolve backend priority:
/// - Explicit `app = "relay"` -> [`BackendTarget::Relay`].
/// - Explicit `app = "ws://..." | "http://..."` -> [`BackendTarget::CdpUrl`].
/// - Explicit `app = <exe-path>` (other than `"auto"`) -> [`BackendTarget::Managed`].
/// - Omitted or `"auto"`: `[browser] relay` (skipped when `is_subagent == true`) ->
///   `[browser] cdp_url` -> managed browser.
pub(crate) fn resolve_backend(
    app: Option<&str>,
    cfg: &BrowserConfig,
    is_subagent: bool,
) -> BackendTarget {
    match app.map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) if a.eq_ignore_ascii_case("relay") => BackendTarget::Relay,
        Some(a) if a.eq_ignore_ascii_case("auto") => default_backend(cfg, is_subagent),
        Some(a) if a.starts_with("ws://") || a.starts_with("http://") => {
            BackendTarget::CdpUrl(a.to_owned())
        }
        Some(path) => BackendTarget::Managed(Some(PathBuf::from(path))),
        None => default_backend(cfg, is_subagent),
    }
}

fn default_backend(cfg: &BrowserConfig, is_subagent: bool) -> BackendTarget {
    if cfg.relay && !is_subagent {
        return BackendTarget::Relay;
    }
    if let Some(url) = &cfg.cdp_url {
        return BackendTarget::CdpUrl(url.clone());
    }
    BackendTarget::Managed(cfg.binary.clone())
}

/// Whether `hrn` created the tab (safe to close) or borrowed an existing user tab (detach only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabOwnership {
    Owned,
    Borrowed,
}

/// Relay metadata attached to tabs driven through `hrn browser-relay`.
pub(crate) struct RelayTabMeta {
    pub(crate) tab_id: i64,
    pub(crate) control_ws_url: String,
}

/// One live named browser tab session.
pub(crate) struct TabSession {
    pub(crate) cdp: CdpClient,
    pub(crate) ownership: TabOwnership,
    pub(crate) relay: Option<RelayTabMeta>,
    pub(crate) _launched: Option<LaunchedBrowser>,
    pub(crate) refs: BTreeMap<String, i64>,
    pub(crate) initial_nav_done: bool,
}

/// Manages lazily-created named browser tabs across Chrome relay, direct CDP, and managed Chrome.
pub struct BrowserManager {
    config: BrowserConfig,
    tabs: Mutex<BTreeMap<String, TabSession>>,
}

impl BrowserManager {
    /// Construct a manager (nothing connects or launches until the first `browser` tool call).
    pub fn new(config: BrowserConfig) -> Self {
        Self {
            config,
            tabs: Mutex::new(BTreeMap::new()),
        }
    }

    /// Borrow the active `[browser]` configuration.
    pub(crate) fn config(&self) -> &BrowserConfig {
        &self.config
    }

    /// Lock the named tab table.
    pub(crate) async fn lock_tabs(
        &self,
    ) -> tokio::sync::MutexGuard<'_, BTreeMap<String, TabSession>> {
        self.tabs.lock().await
    }

    /// Ensure named tab `name` is connected and alive, opening or borrowing it if absent.
    pub(crate) async fn ensure_tab<'a>(
        &self,
        tabs: &'a mut BTreeMap<String, TabSession>,
        name: &str,
        app: Option<&str>,
        target: Option<&str>,
        initial_url: Option<&str>,
        is_subagent: bool,
    ) -> Result<&'a mut TabSession> {
        let needs_connect = match tabs.get(name) {
            Some(existing) if !existing.cdp.is_closed() => {
                // Re-attach if the caller explicitly requests a different borrowed target.
                target.is_some()
            }
            _ => true,
        };

        if needs_connect {
            if let Some(old) = tabs.remove(name) {
                teardown_tab(old).await;
            }
            let backend = resolve_backend(app, &self.config, is_subagent);
            let session = match backend {
                BackendTarget::Relay => open_relay_tab(&self.config, target, initial_url).await?,
                BackendTarget::CdpUrl(url) => open_cdp_tab(&url, target).await?,
                BackendTarget::Managed(bin) => {
                    open_managed_tab(bin.as_deref(), self.config.headless).await?
                }
            };
            tabs.insert(name.to_owned(), session);
        }

        tabs.get_mut(name)
            .ok_or_else(|| HrnError::Tool(format!("browser tab `{name}` unavailable")))
    }

    /// Close one named tab (or all tabs when `name` is `None`), closing only `hrn`-owned tabs
    /// and detaching borrowed tabs without closing them.
    pub(crate) async fn close_tab(&self, name: Option<&str>) -> String {
        let mut tabs = self.tabs.lock().await;
        if let Some(tab_name) = name {
            if let Some(sess) = tabs.remove(tab_name) {
                let ownership = sess.ownership;
                teardown_tab(sess).await;
                return match ownership {
                    TabOwnership::Owned => format!("Closed browser tab `{tab_name}`."),
                    TabOwnership::Borrowed => {
                        format!("Detached borrowed browser tab `{tab_name}` (tab left open).")
                    }
                };
            }
            return format!("No active browser tab named `{tab_name}`.");
        }

        if tabs.is_empty() {
            return "No active browser session.".to_owned();
        }
        let mut closed = 0usize;
        let mut detached = 0usize;
        let drained: Vec<TabSession> = std::mem::take(&mut *tabs).into_values().collect();
        for sess in drained {
            match sess.ownership {
                TabOwnership::Owned => closed += 1,
                TabOwnership::Borrowed => detached += 1,
            }
            teardown_tab(sess).await;
        }
        match (closed, detached) {
            (1, 0) => "Browser session closed.".to_owned(),
            (c, 0) => format!("Closed {c} owned browser tabs."),
            (0, d) => format!("Detached {d} borrowed browser tab(s) (tabs left open)."),
            (c, d) => format!("Closed {c} owned tab(s) and detached {d} borrowed tab(s)."),
        }
    }

    /// List open tabs tracked by this manager (plus Chrome relay tabs when relay is connected).
    pub(crate) async fn list_tabs_summary(
        &self,
        app: Option<&str>,
        is_subagent: bool,
    ) -> Result<String> {
        let mut lines = Vec::new();
        {
            let tabs = self.tabs.lock().await;
            for (name, sess) in tabs.iter() {
                let kind = match sess.ownership {
                    TabOwnership::Owned => "owned",
                    TabOwnership::Borrowed => "borrowed",
                };
                let via = match &sess.relay {
                    Some(meta) => format!("relay tabId={}", meta.tab_id),
                    None => "cdp".to_owned(),
                };
                lines.push(format!("- {name} ({kind}, {via})"));
            }
        }

        if matches!(
            resolve_backend(app, &self.config, is_subagent),
            BackendTarget::Relay
        ) && let Ok(state) = read_relay_state(&self.config.data_dir)
            && let Ok(chrome_tabs) = relay_list_tabs(&state).await
        {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push("Chrome relay tabs:".to_owned());
            for tab in chrome_tabs {
                let id = tab.get("tabId").and_then(Value::as_i64).unwrap_or(0);
                let title = tab.get("title").and_then(Value::as_str).unwrap_or("");
                let url = tab.get("url").and_then(Value::as_str).unwrap_or("");
                lines.push(format!("  [{id}] {title} — {url}"));
            }
        }

        if lines.is_empty() {
            Ok("No active browser tabs.".to_owned())
        } else {
            Ok(lines.join("\n"))
        }
    }
}

async fn teardown_tab(sess: TabSession) {
    sess.cdp.close().await;
    if let Some(meta) = sess.relay
        && let Ok(ctrl) = CdpClient::connect(&meta.control_ws_url).await
    {
        let method = match sess.ownership {
            TabOwnership::Owned => "Relay.closeTab",
            TabOwnership::Borrowed => "Relay.detachTab",
        };
        let _ = ctrl
            .call(method, json!({ "tabId": meta.tab_id }), RELAY_TIMEOUT)
            .await;
        ctrl.close().await;
    }
}

async fn open_relay_tab(
    cfg: &BrowserConfig,
    target: Option<&str>,
    initial_url: Option<&str>,
) -> Result<TabSession> {
    let state = read_relay_state(&cfg.data_dir)?;
    let control_url = state.control_ws_url();
    let ctrl = CdpClient::connect(&control_url).await.map_err(|e| {
        HrnError::Tool(format!(
            "failed to connect to browser relay ({e}); run `hrn browser-relay install` and \
             ensure Chrome has the relay extension loaded"
        ))
    })?;

    let (tab_id, ownership, initial_nav_done) = if let Some(needle) = target {
        let tabs = relay_list_tabs_via(&ctrl).await?;
        let found = find_matching_relay_tab(&tabs, needle)
            .ok_or_else(|| HrnError::Tool(format!("no Chrome tab matched target `{needle}`")))?;
        let _ = ctrl
            .call("Relay.attachTab", json!({ "tabId": found }), RELAY_TIMEOUT)
            .await?;
        (found, TabOwnership::Borrowed, false)
    } else {
        let url = initial_url.unwrap_or("about:blank");
        let opened = ctrl
            .call("Relay.openTab", json!({ "url": url }), RELAY_TIMEOUT)
            .await?;
        let id = opened.get("tabId").and_then(Value::as_i64).ok_or_else(|| {
            HrnError::Tool("browser relay `open_tab` returned no `tabId`".to_owned())
        })?;
        (id, TabOwnership::Owned, initial_url.is_some())
    };
    ctrl.close().await;

    let cdp = CdpClient::connect(&state.tab_ws_url(tab_id)).await?;
    cdp.enable_domains().await?;
    Ok(TabSession {
        cdp,
        ownership,
        relay: Some(RelayTabMeta {
            tab_id,
            control_ws_url: control_url,
        }),
        _launched: None,
        refs: BTreeMap::new(),
        initial_nav_done,
    })
}

async fn relay_list_tabs(state: &RelayState) -> Result<Vec<Value>> {
    let ctrl = CdpClient::connect(&state.control_ws_url()).await?;
    let tabs = relay_list_tabs_via(&ctrl).await;
    ctrl.close().await;
    tabs
}

async fn relay_list_tabs_via(ctrl: &CdpClient) -> Result<Vec<Value>> {
    let res = ctrl
        .call("Relay.listTabs", json!({}), RELAY_TIMEOUT)
        .await?;
    Ok(res
        .get("tabs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn find_matching_relay_tab(tabs: &[Value], needle: &str) -> Option<i64> {
    let trimmed = needle.trim();
    if let Ok(wanted_id) = trimmed.parse::<i64>()
        && let Some(hit) = tabs
            .iter()
            .find(|t| t.get("tabId").and_then(Value::as_i64) == Some(wanted_id))
    {
        return hit.get("tabId").and_then(Value::as_i64);
    }
    let low = trimmed.to_ascii_lowercase();
    tabs.iter()
        .find(|t| {
            let url = t.get("url").and_then(Value::as_str).unwrap_or("");
            let title = t.get("title").and_then(Value::as_str).unwrap_or("");
            url.to_ascii_lowercase().contains(&low) || title.to_ascii_lowercase().contains(&low)
        })
        .and_then(|t| t.get("tabId").and_then(Value::as_i64))
}

async fn open_cdp_tab(url: &str, target: Option<&str>) -> Result<TabSession> {
    let launched = attach_cdp(url, target).await?;
    let cdp = CdpClient::connect(&launched.page_ws_url).await?;
    cdp.enable_domains().await?;
    Ok(TabSession {
        cdp,
        ownership: if target.is_some() {
            TabOwnership::Borrowed
        } else {
            TabOwnership::Owned
        },
        relay: None,
        _launched: Some(launched),
        refs: BTreeMap::new(),
        initial_nav_done: false,
    })
}

async fn open_managed_tab(binary: Option<&std::path::Path>, headless: bool) -> Result<TabSession> {
    let launched = launch_managed(binary, headless).await?;
    let cdp = CdpClient::connect(&launched.page_ws_url).await?;
    cdp.enable_domains().await?;
    Ok(TabSession {
        cdp,
        ownership: TabOwnership::Owned,
        relay: None,
        _launched: Some(launched),
        refs: BTreeMap::new(),
        initial_nav_done: false,
    })
}
