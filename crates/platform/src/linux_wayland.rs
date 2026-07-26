use std::ffi::OsString;

use crate::{EnvironmentStatus, HostPlatform, PlatformIssue, PlatformReport};

pub(crate) const fn native_capture_kind() -> crate::BackendKind {
    crate::BackendKind::LinuxWaylandInputCapture
}

pub(crate) const fn native_emulation_kind() -> crate::BackendKind {
    crate::BackendKind::LinuxWaylandRemoteDesktop
}

/// Checks the environment required before opening portal InputCapture and
/// RemoteDesktop sessions backed by EIS.
#[must_use]
pub fn probe_host() -> PlatformReport {
    probe_environment(
        std::env::var_os("XDG_SESSION_TYPE"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS"),
    )
}

fn probe_environment(
    session_type: Option<OsString>,
    wayland_display: Option<OsString>,
    session_bus: Option<OsString>,
) -> PlatformReport {
    let mut issues = Vec::new();

    let is_wayland = session_type
        .as_deref()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("wayland"));
    if !is_wayland {
        issues.push(PlatformIssue::NotWaylandSession);
    }
    if wayland_display.is_none() {
        issues.push(PlatformIssue::MissingWaylandDisplay);
    }
    if session_bus.is_none() {
        issues.push(PlatformIssue::MissingSessionBus);
    }

    PlatformReport {
        platform: HostPlatform::LinuxWayland,
        status: if issues.is_empty() {
            EnvironmentStatus::Available
        } else {
            EnvironmentStatus::Unavailable
        },
        issues,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::probe_environment;
    use crate::{EnvironmentStatus, PlatformIssue};

    #[test]
    fn accepts_a_complete_wayland_environment() {
        let report = probe_environment(
            Some(OsString::from("wayland")),
            Some(OsString::from("wayland-0")),
            Some(OsString::from("unix:path=/run/user/1000/bus")),
        );

        assert_eq!(report.status, EnvironmentStatus::Available);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn reports_each_missing_requirement() {
        let report = probe_environment(Some(OsString::from("x11")), None, None);

        assert_eq!(report.status, EnvironmentStatus::Unavailable);
        assert_eq!(
            report.issues,
            vec![
                PlatformIssue::NotWaylandSession,
                PlatformIssue::MissingWaylandDisplay,
                PlatformIssue::MissingSessionBus,
            ]
        );
    }
}
