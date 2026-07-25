use crate::{EnvironmentStatus, HostPlatform, PlatformReport};

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
