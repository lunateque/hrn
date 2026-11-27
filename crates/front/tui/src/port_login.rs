//! Login, logout, setup wizard, and session settings types shared across the TUI [`crate::Backend`]
//! boundary.

use crate::port_info::LoginOutcome;
use hrn_kernel::{PermissionMode, ThinkingLevel};

/// What the user controls from the TUI for the current session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Model alias, as accepted by [`crate::Backend::resolve_model`].
    pub model: String,
    /// Thinking effort.
    pub thinking: ThinkingLevel,
    /// Tool permission mode.
    pub mode: PermissionMode,
    /// Passive post-turn advisor enabled.
    pub advisor: bool,
    /// Optional explicit model alias for the passive advisor.
    pub advisor_model: Option<String>,
    /// Hide thinking blocks in the transcript (the `--hide-thinking` flag; display only).
    pub hide_thinking: bool,
    /// The live goal-mode objective (`None`: goal mode off), shown in the status band.
    pub goal: Option<hrn_kernel::Goal>,
    /// Active session service tier (`/fast`, `/fast ultra`, `--service-tier`).
    pub service_tier: hrn_kernel::Tier,
}

/// Asynchronous events streamed from an in-TUI `/login` flow (OAuth PKCE, RFC 8628 device code,
/// or pre-save API-key validation probe).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginUiEvent {
    /// Display the authorization URL (and optional device `user_code`) in the `/login` modal.
    ShowUrl {
        /// Provider identifier (e.g. `"claude"`, `"chatgpt"`, `"kimi-code"`).
        provider: String,
        /// Human-readable provider label.
        label: String,
        /// Browser authorization or verification URL.
        url: String,
        /// Optional RFC 8628 end-user code to enter at `url`.
        user_code: Option<String>,
        /// Guidance message shown beneath the URL.
        message: String,
        /// Whether to render an inline paste box for a callback code or redirect URL.
        needs_code: bool,
    },
    /// Non-fatal progress or validation status update (never carries secrets).
    Progress(String),
    /// Pre-save API-key validation failed; keep the key-entry form open to re-prompt.
    ProbeFailed(String),
    /// Background login completed (succeeded or failed with a sanitized error).
    Finished(std::result::Result<LoginOutcome, String>),
}

/// One credential-bearing provider offered by `/logout`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogoutProviderChoice {
    /// Stable provider identifier (`claude`, `chatgpt`, `kimi-code`, `anthropic`, …).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Auth storage category (`subscription`, `api_key`, `oauth`).
    pub kind: String,
    /// Non-secret summary (account email or credential file location).
    pub detail: String,
}

/// Current onboarding state presented by the 3-step `/setup` wizard.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupWizardInfo {
    /// Providers currently holding working credentials.
    pub configured_providers: Vec<String>,
    /// Available model aliases (configured + discovered) for Step 2.
    pub model_aliases: Vec<String>,
    /// Current `[roles] default` value, when set.
    pub current_default: Option<String>,
    /// Available theme names for Step 3.
    pub themes: Vec<String>,
    /// Active theme name.
    pub current_theme: String,
}
