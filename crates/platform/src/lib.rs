//! Native platform boundaries for input capture and injection.
//!
//! Native event loops belong on dedicated threads. Implementations expose only
//! the platform-neutral domain events defined by the `domain` crate.

#[cfg(target_os = "linux")]
mod linux_wayland;
#[cfg(target_os = "windows")]
mod windows;

pub use domain::HostPlatform;
use domain::InputEvent;
use serde::Serialize;
use thiserror::Error;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
compile_error!("Tevir supports only Windows and Linux Wayland");

#[cfg(target_os = "linux")]
pub use linux_wayland::probe_host;
#[cfg(target_os = "windows")]
pub use windows::probe_host;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentStatus {
    Available,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlatformReport {
    pub platform: HostPlatform,
    pub status: EnvironmentStatus,
    pub issues: Vec<PlatformIssue>,
}

impl PlatformReport {
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.status == EnvironmentStatus::Available
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformIssue {
    NotWaylandSession,
    MissingWaylandDisplay,
    MissingSessionBus,
}

impl std::fmt::Display for PlatformIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotWaylandSession => "XDG_SESSION_TYPE is not `wayland`",
            Self::MissingWaylandDisplay => "WAYLAND_DISPLAY is not set",
            Self::MissingSessionBus => "DBUS_SESSION_BUS_ADDRESS is not set",
        };
        formatter.write_str(message)
    }
}

/// Captures local events after the routing layer activates this node.
pub trait InputCapture: Send {
    fn set_enabled(&mut self, enabled: bool) -> Result<(), BackendError>;
    fn next_event(&mut self) -> Result<Option<InputEvent>, BackendError>;
}

/// Injects remote events and releases held state when focus changes or a peer disconnects.
pub trait InputInjection: Send {
    fn inject(&mut self, event: InputEvent) -> Result<(), BackendError>;
    fn release_all(&mut self) -> Result<(), BackendError>;
}

pub trait InputBackend: InputCapture + InputInjection {}

impl<T> InputBackend for T where T: InputCapture + InputInjection {}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("platform backend is unavailable: {reason}")]
    Unavailable { reason: String },
    #[error("platform permission was denied: {permission}")]
    PermissionDenied { permission: &'static str },
    #[error("platform operation `{operation}` failed: {reason}")]
    Operation {
        operation: &'static str,
        reason: String,
    },
}
