//! Native platform boundaries for input and clipboard access.
//!
//! Native event loops belong on dedicated threads. Implementations expose only
//! the platform-neutral domain events defined by the `domain` crate.

mod clipboard;
mod convert;
#[cfg(target_os = "linux")]
mod linux_wayland;
mod native_input;
mod service;
mod state;
#[cfg(target_os = "windows")]
mod windows;

pub use clipboard::{ClipboardService, ClipboardServiceEvent};
pub use domain::HostPlatform;
use serde::Serialize;
pub use service::{
    BackendKind, CaptureService, CaptureServiceEvent, DesktopGeometry, InjectionService,
    InjectionServiceEvent, SERVICE_QUEUE_CAPACITY,
};
use thiserror::Error;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
compile_error!("Tevir supports only Windows and Linux Wayland");

#[cfg(target_os = "linux")]
pub use linux_wayland::probe_host;
#[cfg(target_os = "linux")]
use linux_wayland::{native_capture_kind, native_emulation_kind};
#[cfg(target_os = "windows")]
pub use windows::probe_host;
#[cfg(target_os = "windows")]
use windows::{native_capture_kind, native_emulation_kind};

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
    #[error("backend command queue is full")]
    CommandQueueFull,
    #[error("native input batch cannot be empty")]
    EmptyInputBatch,
    #[error("native input session generation is exhausted")]
    SessionGenerationExhausted,
    #[error("backend worker has stopped")]
    WorkerStopped,
    #[error("backend worker thread panicked")]
    WorkerPanicked,
    #[error("clipboard text is {actual} bytes; the maximum is {maximum}")]
    ClipboardTextTooLarge { actual: usize, maximum: usize },
}
