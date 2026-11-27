//! Structured error taxonomy for Windows desktop automation matching omp error codes.

use hrn_kernel::HrnError;
use std::fmt;

/// Typed errors produced by the `desktop` module with stable omp error codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopError {
    /// Target window id, handle, or title substring could not be resolved.
    WindowNotFound(String),
    /// Background input (`PostMessage`) could not be delivered to the target window.
    BackgroundUnavailable(String),
    /// Target window input queue is busy or unresponsive.
    InputBusy(String),
    /// UIA element ref is absent from both current and previous snapshots or its window closed.
    StaleRef(String),
    /// Active coordinate frame no longer matches the target window's rect or identity.
    InvalidCoordinateFrame(String),
    /// UIA mutation could not be verified on read-back (unsupported or value unchanged).
    AxUnconfirmed(String),
    /// General UI Automation or bridge execution failure.
    AxFailed(String),
    /// Screen or window pixel capture failed.
    CaptureFailed(String),
    /// Desktop session is locked, disconnected, or on a UAC secure desktop.
    ScreenLocked(String),
}

impl DesktopError {
    /// Stable omp error code string for this error variant.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::WindowNotFound(_) => "WindowNotFound",
            Self::BackgroundUnavailable(_) => "BackgroundUnavailable",
            Self::InputBusy(_) => "InputBusy",
            Self::StaleRef(_) => "StaleRef",
            Self::InvalidCoordinateFrame(_) => "InvalidCoordinateFrame",
            Self::AxUnconfirmed(_) => "AxUnconfirmed",
            Self::AxFailed(_) => "AxFailed",
            Self::CaptureFailed(_) => "CaptureFailed",
            Self::ScreenLocked(_) => "ScreenLocked",
        }
    }

    /// Human-readable error message without the `[<code>]` prefix.
    pub fn message(&self) -> &str {
        match self {
            Self::WindowNotFound(m)
            | Self::BackgroundUnavailable(m)
            | Self::InputBusy(m)
            | Self::StaleRef(m)
            | Self::InvalidCoordinateFrame(m)
            | Self::AxUnconfirmed(m)
            | Self::AxFailed(m)
            | Self::CaptureFailed(m)
            | Self::ScreenLocked(m) => m,
        }
    }

    /// Parse a structured bridge error code into a [`DesktopError`].
    pub fn from_code(code: &str, message: impl Into<String>) -> Option<Self> {
        let msg = message.into();
        match code {
            "WindowNotFound" => Some(Self::WindowNotFound(msg)),
            "BackgroundUnavailable" => Some(Self::BackgroundUnavailable(msg)),
            "InputBusy" => Some(Self::InputBusy(msg)),
            "StaleRef" => Some(Self::StaleRef(msg)),
            "InvalidCoordinateFrame" => Some(Self::InvalidCoordinateFrame(msg)),
            "AxUnconfirmed" => Some(Self::AxUnconfirmed(msg)),
            "AxFailed" => Some(Self::AxFailed(msg)),
            "CaptureFailed" => Some(Self::CaptureFailed(msg)),
            "ScreenLocked" => Some(Self::ScreenLocked(msg)),
            _ => None,
        }
    }
}

impl fmt::Display for DesktopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code(), self.message())
    }
}

impl std::error::Error for DesktopError {}

impl From<DesktopError> for HrnError {
    fn from(err: DesktopError) -> Self {
        Self::Tool(err.to_string())
    }
}
