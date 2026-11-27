//! Chromium / Edge / Brave discovery, `[browser]` configuration, isolated headless launch,
//! and CDP page target lookup.

use hrn_edge_http::loopback_http_get;
use hrn_edge_proc::{ChildWrapper, build_wrapped_command, resolve_on_path};
use hrn_edge_rt::tokio;
use hrn_kernel::serde::Deserialize;
use hrn_kernel::serde_json::{self, Value};
use hrn_kernel::{HrnError, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static PROFILE_SEQ: AtomicU64 = AtomicU64::new(1);

/// Embedded `[browser]` module defaults (`data/defaults.json`).
const DEFAULTS_JSON: &str = include_str!("../data/defaults.json");

/// Running browser handle (optional owned child process + temp user-data-dir cleaned on drop).
pub(crate) struct LaunchedBrowser {
    pub(crate) page_ws_url: String,
    child: Option<Box<dyn ChildWrapper>>,
    profile_dir: Option<PathBuf>,
}

impl Drop for LaunchedBrowser {
    fn drop(&mut self) {
        // Drop child first so Windows releases file handles inside the temp profile dir.
        self.child.take();
        if let Some(dir) = self.profile_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[derive(Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
struct RawBrowserDefaults {
    relay: bool,
    cdp_url: Option<String>,
    binary: Option<PathBuf>,
    screenshot_dir: Option<PathBuf>,
    headless: bool,
}

/// `[browser]` module configuration loaded from `data/defaults.json`, user config, and env vars.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields, default)]
pub struct BrowserConfig {
    /// Default to the user's Chrome via the native browser relay (skipped for subagents).
    pub relay: bool,
    /// Explicit CDP endpoint (`http://127.0.0.1:9222` or `ws://127.0.0.1:9222/devtools/page/...`).
    pub cdp_url: Option<String>,
    /// Explicit browser executable override.
    pub binary: Option<PathBuf>,
    /// Optional directory where captured browser screenshots are also saved as PNG files.
    pub screenshot_dir: Option<PathBuf>,
    /// Launch managed browsers headlessly (default `true`).
    pub headless: bool,
    /// Per-user data directory where `browser-relay.json` lives (injected by host registration).
    #[serde(skip)]
    pub data_dir: PathBuf,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        let parsed: RawBrowserDefaults =
            serde_json::from_str(DEFAULTS_JSON).unwrap_or(RawBrowserDefaults {
                relay: false,
                cdp_url: None,
                binary: None,
                screenshot_dir: None,
                headless: true,
            });
        Self {
            relay: parsed.relay,
            cdp_url: parsed.cdp_url,
            binary: parsed.binary,
            screenshot_dir: parsed.screenshot_dir,
            headless: parsed.headless,
            data_dir: std::env::temp_dir(),
        }
    }
}

impl BrowserConfig {
    /// Build configuration from defaults merged with `HRN_BROWSER_CDP_URL` and `HRN_BROWSER_BIN`.
    pub fn from_env() -> Self {
        Self::default().with_env_fallbacks()
    }

    /// Fill unset `cdp_url` / `binary` fields from `HRN_BROWSER_CDP_URL` and `HRN_BROWSER_BIN`.
    #[must_use]
    pub fn with_env_fallbacks(mut self) -> Self {
        if self.cdp_url.is_none() {
            self.cdp_url = std::env::var("HRN_BROWSER_CDP_URL")
                .ok()
                .filter(|s| !s.trim().is_empty());
        }
        if self.binary.is_none() {
            self.binary = std::env::var("HRN_BROWSER_BIN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from);
        }
        self
    }

    /// Attach the host's per-user data directory for relay discovery.
    #[must_use]
    pub fn with_data_dir(mut self, data_dir: PathBuf) -> Self {
        self.data_dir = data_dir;
        self
    }
}

/// Attach to an existing CDP endpoint (optionally matching a specific target id/URL/title).
pub(crate) async fn attach_cdp(url: &str, target: Option<&str>) -> Result<LaunchedBrowser> {
    let ws = resolve_cdp_page_ws(url, target).await?;
    Ok(LaunchedBrowser {
        page_ws_url: ws,
        child: None,
        profile_dir: None,
    })
}

/// Launch an isolated managed Chromium/Edge process using an optional explicit executable path.
pub(crate) async fn launch_managed(
    binary_override: Option<&Path>,
    headless: bool,
) -> Result<LaunchedBrowser> {
    let bin = discover_browser_binary(binary_override).ok_or_else(|| {
        HrnError::Tool(
            "no Chromium/Chrome/Edge/Brave browser found. Install Edge/Chrome or set \
             `HRN_BROWSER_BIN` / `HRN_BROWSER_CDP_URL`."
                .to_owned(),
        )
    })?;

    let profile_dir = std::env::temp_dir().join(format!(
        "hrn-browser-{}-{}",
        std::process::id(),
        PROFILE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&profile_dir).map_err(|e| {
        HrnError::Tool(format!(
            "failed to create temp browser profile `{}`: {e}",
            profile_dir.display()
        ))
    })?;

    let mut args = vec![
        "--remote-debugging-port=0".to_owned(),
        "--remote-allow-origins=*".to_owned(),
        format!("--user-data-dir={}", profile_dir.display()),
        "--no-first-run".to_owned(),
        "--no-default-browser-check".to_owned(),
        "--disable-extensions".to_owned(),
        "--disable-background-networking".to_owned(),
        "--disable-sync".to_owned(),
        "--disable-gpu".to_owned(),
        "--window-size=1440,900".to_owned(),
    ];
    if headless {
        args.push("--headless=new".to_owned());
    }
    args.push("about:blank".to_owned());

    let program = bin.to_string_lossy().into_owned();
    let mut cmd = build_wrapped_command(&program, &args, &profile_dir, &BTreeMap::new());
    let child = cmd
        .spawn()
        .map_err(|e| HrnError::Tool(format!("failed to launch `{program}`: {e}")))?;

    let port = wait_devtools_port(&profile_dir, Duration::from_secs(12)).await?;
    let page_ws_url = discover_page_ws(port).await?;

    Ok(LaunchedBrowser {
        page_ws_url,
        child: Some(child),
        profile_dir: Some(profile_dir),
    })
}

/// Discover an installed Chromium-based browser on Windows or Unix.
pub fn discover_browser_binary(override_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = override_path
        && p.is_file()
    {
        return Some(p.to_path_buf());
    }

    #[cfg(windows)]
    {
        let mut candidates = Vec::new();
        for env_key in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Ok(base) = std::env::var(env_key) {
                let root = PathBuf::from(base);
                candidates.push(root.join(r"Google\Chrome\Application\chrome.exe"));
                candidates.push(root.join(r"Microsoft\Edge\Application\msedge.exe"));
                candidates.push(root.join(r"BraveSoftware\Brave-Browser\Application\brave.exe"));
                candidates.push(root.join(r"Chromium\Application\chrome.exe"));
            }
        }
        for path in candidates {
            if path.is_file() {
                return Some(path);
            }
        }
        for name in ["chrome", "msedge", "brave", "chromium"] {
            if let Some(found) = resolve_on_path(name) {
                return Some(found);
            }
        }
        None
    }

    #[cfg(not(windows))]
    {
        for name in [
            "google-chrome",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "brave-browser",
        ] {
            if let Some(found) = resolve_on_path(name) {
                return Some(found);
            }
        }
        None
    }
}

async fn wait_devtools_port(profile_dir: &Path, timeout: Duration) -> Result<u16> {
    let port_file = profile_dir.join("DevToolsActivePort");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(text) = tokio::fs::read_to_string(&port_file).await
            && let Some(first_line) = text.lines().next()
            && let Ok(port) = first_line.trim().parse::<u16>()
            && port > 0
        {
            return Ok(port);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(HrnError::Tool(
                "timed out waiting for browser DevToolsActivePort".to_owned(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

async fn resolve_cdp_page_ws(url: &str, target_filter: Option<&str>) -> Result<String> {
    if url.starts_with("ws://") {
        return Ok(url.to_owned());
    }
    let base = url.trim_end_matches('/');
    let list_url = format!("{base}/json/list");
    let body = loopback_http_get(&list_url, Duration::from_secs(5)).await?;
    extract_page_ws(&body, target_filter).ok_or_else(|| match target_filter {
        Some(needle) => HrnError::Tool(format!(
            "no CDP target matching `{needle}` found at {list_url}"
        )),
        None => HrnError::Tool(format!(
            "no page target with webSocketDebuggerUrl found at {list_url}"
        )),
    })
}

async fn discover_page_ws(port: u16) -> Result<String> {
    let list_url = format!("http://127.0.0.1:{port}/json/list");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(body) = loopback_http_get(&list_url, Duration::from_secs(2)).await
            && let Some(ws) = extract_page_ws(&body, None)
        {
            return Ok(ws);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(HrnError::Tool(format!(
                "timed out discovering CDP page target at {list_url}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn extract_page_ws(json_text: &str, target_filter: Option<&str>) -> Option<String> {
    let targets: Value = serde_json::from_str(json_text).ok()?;
    let arr = targets.as_array()?;
    if let Some(needle) = target_filter {
        let low = needle.to_ascii_lowercase();
        let matched = arr.iter().find(|t| {
            let id = t.get("id").and_then(Value::as_str).unwrap_or("");
            let url = t.get("url").and_then(Value::as_str).unwrap_or("");
            let title = t.get("title").and_then(Value::as_str).unwrap_or("");
            id.eq_ignore_ascii_case(needle)
                || url.to_ascii_lowercase().contains(&low)
                || title.to_ascii_lowercase().contains(&low)
        })?;
        return matched
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }

    let is_page = |t: &&Value| t.get("type").and_then(Value::as_str) == Some("page");
    let is_internal_ui = |t: &&Value| {
        t.get("url").and_then(Value::as_str).is_some_and(|u| {
            u.starts_with("edge://")
                || u.starts_with("chrome://")
                || u.starts_with("chrome-extension://")
        })
    };
    let page = arr
        .iter()
        .find(|t| is_page(t) && !is_internal_ui(t))
        .or_else(|| arr.iter().find(|t| is_page(t)))
        .or_else(|| arr.first())?;
    page.get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_owned)
}
