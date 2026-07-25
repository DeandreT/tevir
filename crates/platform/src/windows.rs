use crate::{EnvironmentStatus, HostPlatform, PlatformReport};

pub(crate) const fn native_capture_backend() -> capture_engine::Backend {
    capture_engine::Backend::Windows
}

pub(crate) const fn native_emulation_backend() -> emulation_engine::Backend {
    emulation_engine::Backend::Windows
}

pub(crate) const fn native_capture_kind() -> crate::BackendKind {
    crate::BackendKind::WindowsHooks
}

pub(crate) const fn native_emulation_kind() -> crate::BackendKind {
    crate::BackendKind::WindowsSendInput
}

/// Windows input hooks and injection do not require a desktop-session
/// environment variable. Fine-grained permission checks occur when opening the
/// native backend.
#[must_use]
pub fn probe_host() -> PlatformReport {
    PlatformReport {
        platform: HostPlatform::Windows,
        status: EnvironmentStatus::Available,
        issues: Vec::new(),
    }
}
