//! The approval-tiered `desktop` tool, `[desktop]` configuration, and persistent manager.

use crate::actions::run_action;
use crate::session::DesktopSession;
use crate::win_bridge::DesktopBridge;
use hrn_edge_rt::tokio;
use hrn_kernel::serde::Deserialize;
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{
    BoxFuture, HrnError, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::sync::Arc;
use tokio::sync::{Mutex, MutexGuard};

/// Embedded `[desktop]` config defaults.
const DEFAULTS_JSON: &str = include_str!("../data/defaults.json");
/// Concise `desktop` tool description.
const DESCRIPTION: &str = include_str!("../prompts/desktop.md");
/// Full `help://desktop` reference page.
pub(crate) const HELP: &str = include_str!("../prompts/help-desktop.md");

/// Fallback behavior when background (`PostMessage`) input cannot reach a target window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(crate = "hrn_kernel::serde", rename_all = "snake_case")]
pub enum BackgroundFallback {
    /// Briefly focus the target window, deliver input, wait for drain, and restore focus.
    Takeover,
    /// Refuse the operation and surface `BackgroundUnavailable`.
    Refuse,
}

#[derive(Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
struct RawDefaults {
    display: String,
    max_width: u32,
    max_height: u32,
    background_fallback: BackgroundFallback,
    eval_prelude: bool,
}

/// `[desktop]` module configuration parsed from `data/defaults.json` plus user overrides.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields, default)]
pub struct DesktopConfig {
    /// Default display selector (`"active"`, `"primary"`, `"all"`, index, or name).
    pub display: String,
    /// Maximum captured image width in pixels.
    pub max_width: u32,
    /// Maximum captured image height in pixels.
    pub max_height: u32,
    /// Behavior when background `PostMessage` input is unavailable (`"takeover"` or `"refuse"`).
    pub background_fallback: BackgroundFallback,
    /// Expose the `computer.*` facade inside `eval` cells (wrapping `tool.desktop`).
    pub eval_prelude: bool,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        if let Ok(d) = serde_json::from_str::<RawDefaults>(DEFAULTS_JSON) {
            Self {
                display: d.display,
                max_width: d.max_width,
                max_height: d.max_height,
                background_fallback: d.background_fallback,
                eval_prelude: d.eval_prelude,
            }
        } else {
            Self {
                display: "active".to_owned(),
                max_width: 3840,
                max_height: 2400,
                background_fallback: BackgroundFallback::Takeover,
                eval_prelude: true,
            }
        }
    }
}

impl DesktopConfig {
    /// Validate configured dimensions after deserializing `[desktop]`.
    pub fn validate(&self) -> Result {
        if self.max_width == 0 || self.max_height == 0 {
            return Err(HrnError::Config(
                "[desktop] `max_width` and `max_height` must be at least 1".to_owned(),
            ));
        }
        Ok(())
    }

    /// Maximum single image dimension passed to the Rust image attachment decoder.
    pub(crate) fn max_image_dim(&self) -> u32 {
        self.max_width.max(self.max_height).max(1)
    }
}

pub(crate) struct DesktopState {
    pub(crate) bridge: Option<DesktopBridge>,
    pub(crate) session: DesktopSession,
}

impl DesktopState {
    pub(crate) fn split_mut(&mut self) -> Result<(&mut DesktopBridge, &mut DesktopSession)> {
        let bridge = self
            .bridge
            .as_mut()
            .ok_or_else(|| HrnError::Tool("desktop bridge unavailable".to_owned()))?;
        Ok((bridge, &mut self.session))
    }
}

/// Manages a lazily-spawned persistent desktop automation bridge and stateful session.
pub struct DesktopManager {
    config: DesktopConfig,
    state: Mutex<DesktopState>,
}

impl DesktopManager {
    /// Construct a new manager with default `[desktop]` configuration.
    pub fn new() -> Self {
        Self::with_config(DesktopConfig::default())
    }

    /// Construct a new manager with explicit [`DesktopConfig`].
    pub fn with_config(config: DesktopConfig) -> Self {
        Self {
            config,
            state: Mutex::new(DesktopState {
                bridge: None,
                session: DesktopSession::new(),
            }),
        }
    }

    /// Active `[desktop]` configuration.
    pub fn config(&self) -> &DesktopConfig {
        &self.config
    }

    pub(crate) async fn lock_state(&self) -> MutexGuard<'_, DesktopState> {
        self.state.lock().await
    }

    #[cfg(test)]
    pub(crate) async fn inject_test_bridge(&self, bridge: DesktopBridge) {
        self.state.lock().await.bridge = Some(bridge);
    }
}

impl Default for DesktopManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Windows UI Automation and computer-use tool.
///
/// Inspection actions declare [`ToolApproval::read`] while input/mutation actions declare
/// [`ToolApproval::exec`].
pub struct DesktopTool {
    manager: Arc<DesktopManager>,
}

impl DesktopTool {
    /// Wrap a shared [`DesktopManager`].
    pub fn new(manager: Arc<DesktopManager>) -> Self {
        Self { manager }
    }
}

impl Tool for DesktopTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "desktop".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: desktop_parameters(),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, args: &Value) -> ToolApproval {
        if is_read_action(args.get("action").and_then(Value::as_str)) {
            ToolApproval::read()
        } else {
            ToolApproval::exec()
        }
    }

    fn summarize_call(&self, args: &Value) -> String {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("?");
        match action {
            "click" | "double_click" | "right_click" | "move" => {
                let x = args.get("x").and_then(Value::as_i64).unwrap_or(0);
                let y = args.get("y").and_then(Value::as_i64).unwrap_or(0);
                format!("{action} ({x}, {y})")
            }
            "hotkey" => {
                let keys = args
                    .get("keys")
                    .map_or_else(|| "?".to_owned(), Value::to_string);
                format!("hotkey {keys}")
            }
            "type_text" | "el_type" | "clipboard_write" => {
                let len = args.get("text").and_then(Value::as_str).map_or(0, str::len);
                format!("{action} ({len} chars)")
            }
            "set_value" => {
                let r = args.get("ref").and_then(Value::as_str).unwrap_or("?");
                let len = args
                    .get("value")
                    .and_then(Value::as_str)
                    .map_or(0, str::len);
                format!("set_value {r} ({len} chars)")
            }
            "invoke" | "press" | "focus" | "select_text" | "el_read" => {
                let r = args.get("ref").and_then(Value::as_str).unwrap_or("?");
                format!("{action} {r}")
            }
            "screenshot" | "window_capture" | "ax" | "find" => summarize_target(action, args),
            "open_app" => {
                let t = args.get("target").and_then(Value::as_str).unwrap_or("?");
                format!("open_app {t}")
            }
            other => other.to_owned(),
        }
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let mgr = Arc::clone(&self.manager);
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(HrnError::Cancelled);
            }
            run_action(&mgr, &args).await
        })
    }
}

fn is_read_action(action: Option<&str>) -> bool {
    matches!(
        action,
        Some(
            "list_windows"
                | "ax"
                | "find"
                | "el_read"
                | "displays"
                | "capabilities"
                | "screenshot"
                | "window_capture"
                | "zoom"
                | "clipboard_read"
                | "apps_list"
        )
    )
}

fn summarize_target(action: &str, args: &Value) -> String {
    if let Some(id) = args
        .get("window_id")
        .or_else(|| args.get("id"))
        .and_then(Value::as_u64)
    {
        return format!("{action} id={id}");
    }
    if let Some(w) = args.get("window").and_then(Value::as_str) {
        return format!("{action} \"{w}\"");
    }
    action.to_owned()
}

fn desktop_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": [
                    "screenshot", "window_capture", "zoom", "list_windows", "displays",
                    "capabilities", "apps_list", "open_app", "ax", "find", "el_read",
                    "set_value", "select_text", "invoke", "focus", "el_type",
                    "clipboard_read", "clipboard_write", "click", "double_click",
                    "right_click", "move", "scroll", "type_text", "hotkey", "close"
                ],
                "description": "Desktop action to perform."
            },
            "window_id": { "type": "integer", "description": "Stable numeric window handle from `list_windows`." },
            "id": { "type": "integer", "description": "Alias for `window_id`." },
            "window": { "type": "string", "description": "Window title substring or numeric window handle string." },
            "display": { "type": "string", "description": "Display selector (`active`, `primary`, `all`, index, or name)." },
            "all_monitors": { "type": "boolean", "description": "Capture entire virtual desktop across monitors (`screenshot`)." },
            "x": { "type": "integer", "description": "X coordinate in active capture frame space." },
            "y": { "type": "integer", "description": "Y coordinate in active capture frame space." },
            "w": { "type": "integer", "description": "Region width in active capture frame space (`zoom`)." },
            "h": { "type": "integer", "description": "Region height in active capture frame space (`zoom`)." },
            "delta_y": { "type": "integer", "description": "Wheel notches to scroll (positive=up, negative=down)." },
            "text": { "type": "string", "description": "Unicode text (`type_text`, `el_type`, `clipboard_write`)." },
            "keys": { "description": "Hotkey combo string (\"ctrl+l\") or array ([\"ctrl\", \"shift\", \"p\"])." },
            "ref": { "type": "string", "description": "UIA element reference from `ax` or `find` (e.g. \"e3\")." },
            "field": {
                "type": "string",
                "enum": ["value", "bounds", "actions", "parent", "children"],
                "description": "Field to inspect on `ref` (`el_read`)."
            },
            "value": { "type": "string", "description": "Text value to assign (`set_value`) or substring filter (`find`)." },
            "role": { "type": "string", "description": "Case/alias-insensitive role filter (`find`)." },
            "title": { "type": "string", "description": "Case-insensitive element name substring filter (`find`)." },
            "limit": { "type": "integer", "description": "Maximum matches returned by `find` (<= 5000)." },
            "max_depth": { "type": "integer", "description": "Maximum tree depth for `ax` (default 24)." },
            "max_nodes": { "type": "integer", "description": "Maximum nodes for `ax` (default/max 800)." },
            "start": { "type": "integer", "description": "Selection start character offset (`select_text`)." },
            "end": { "type": "integer", "description": "Selection end character offset (`select_text`)." },
            "target": { "type": "string", "description": "Application executable or target to launch (`open_app`)." },
            "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments for `open_app`." },
            "background": { "type": "boolean", "description": "Send input via background `PostMessage` without stealing focus." },
            "takeover": { "type": "boolean", "description": "Force temporary focus takeover and restore for window input." }
        },
        "required": ["action"]
    })
}
