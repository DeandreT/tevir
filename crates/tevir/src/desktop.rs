use std::{io, path::Path};

#[cfg(target_os = "linux")]
use directories::BaseDirs;
#[cfg(target_os = "windows")]
use std::process::Command;
#[cfg(target_os = "linux")]
use std::{fs, path::PathBuf};
use thiserror::Error;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

#[cfg(target_os = "linux")]
const AUTOSTART_NAME: &str = "tevir.desktop";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    Show,
    ReturnControl,
    Quit,
}

pub struct DesktopIntegration {
    tray: TrayIcon,
    show: MenuId,
    return_control: MenuItem,
    quit: MenuId,
}

impl DesktopIntegration {
    pub fn start() -> Result<Self, DesktopError> {
        initialize_native_menu_loop()?;

        let show = MenuItem::new("Show Tevir", true, None);
        let return_control = MenuItem::new("Return control", false, None);
        let separator = PredefinedMenuItem::separator();
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::with_items(&[&show, &return_control, &separator, &quit])
            .map_err(|error| DesktopError::Tray(error.to_string()))?;
        let tray = TrayIconBuilder::new()
            .with_tooltip("Tevir")
            .with_icon(tray_icon()?)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|error| DesktopError::Tray(error.to_string()))?;

        Ok(Self {
            tray,
            show: show.id().clone(),
            return_control,
            quit: quit.id().clone(),
        })
    }

    pub fn set_return_control_enabled(&self, enabled: bool) {
        if self.return_control.is_enabled() != enabled {
            self.return_control.set_enabled(enabled);
        }
    }

    pub fn poll(&self) -> Vec<TrayAction> {
        pump_native_menu_loop();
        let mut actions = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.show {
                actions.push(TrayAction::Show);
            } else if event.id == *self.return_control.id() {
                actions.push(TrayAction::ReturnControl);
            } else if event.id == self.quit {
                actions.push(TrayAction::Quit);
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(
                event,
                TrayIconEvent::Click {
                    ref id,
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } if id == self.tray.id()
            ) {
                actions.push(TrayAction::Show);
            }
        }
        actions
    }
}

pub fn set_autostart(enabled: bool) -> Result<(), DesktopError> {
    let executable = std::env::current_exe().map_err(DesktopError::CurrentExecutable)?;
    set_autostart_for(enabled, &executable)
}

#[cfg(target_os = "linux")]
fn set_autostart_for(enabled: bool, executable: &Path) -> Result<(), DesktopError> {
    let config = BaseDirs::new()
        .map(|directories| directories.config_dir().to_path_buf())
        .ok_or(DesktopError::ConfigDirectoryUnavailable)?;
    let directory = config.join("autostart");
    let path = directory.join(AUTOSTART_NAME);
    if enabled {
        fs::create_dir_all(&directory).map_err(|source| {
            DesktopError::CreateAutostartDirectory {
                path: directory,
                source,
            }
        })?;
        fs::write(&path, linux_autostart_entry(executable))
            .map_err(|source| DesktopError::WriteAutostart { path, source })
    } else {
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(DesktopError::RemoveAutostart { path, source }),
        }
    }
}

#[cfg(target_os = "windows")]
fn set_autostart_for(enabled: bool, executable: &Path) -> Result<(), DesktopError> {
    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let mut command = Command::new("reg.exe");
    if enabled {
        command.args([
            "add",
            RUN_KEY,
            "/v",
            "Tevir",
            "/t",
            "REG_SZ",
            "/d",
            &format!("\"{}\"", executable.display()),
            "/f",
        ]);
    } else {
        command.args(["delete", RUN_KEY, "/v", "Tevir", "/f"]);
    }
    let output = command
        .output()
        .map_err(|source| DesktopError::RunAutostartCommand { source })?;
    if output.status.success()
        || (!enabled && String::from_utf8_lossy(&output.stderr).contains("unable to find"))
    {
        Ok(())
    } else {
        Err(DesktopError::AutostartCommandFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

#[cfg(target_os = "linux")]
fn linux_autostart_entry(executable: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Tevir\nComment=Network keyboard and pointer sharing\nExec=\"{}\"\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        escape_desktop_exec(executable)
    )
}

#[cfg(target_os = "linux")]
fn escape_desktop_exec(executable: &Path) -> String {
    executable
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
}

fn tray_icon() -> Result<Icon, DesktopError> {
    const SIDE: usize = 32;
    let mut rgba = vec![0_u8; SIDE * SIDE * 4];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let dx = x.abs_diff(SIDE / 2);
            let dy = y.abs_diff(SIDE / 2);
            let inside = dx * dx + dy * dy <= 15 * 15;
            if !inside {
                continue;
            }
            let offset = (y * SIDE + x) * 4;
            let accent = dx * dx + dy * dy >= 13 * 13;
            let mark = (7..=24).contains(&x) && (8..=12).contains(&y)
                || (14..=18).contains(&x) && (8..=24).contains(&y);
            let color = if mark {
                [231, 234, 232, 255]
            } else if accent {
                [50, 185, 164, 255]
            } else {
                [28, 31, 32, 255]
            };
            rgba[offset..offset + 4].copy_from_slice(&color);
        }
    }
    Icon::from_rgba(rgba, SIDE as u32, SIDE as u32)
        .map_err(|error| DesktopError::Tray(error.to_string()))
}

#[cfg(target_os = "linux")]
fn initialize_native_menu_loop() -> Result<(), DesktopError> {
    gtk::init().map_err(|error| DesktopError::Tray(error.to_string()))
}

#[cfg(target_os = "windows")]
const fn initialize_native_menu_loop() -> Result<(), DesktopError> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn pump_native_menu_loop() {
    let context = gtk::glib::MainContext::default();
    while context.pending() {
        let _ = context.iteration(false);
    }
}

#[cfg(target_os = "windows")]
const fn pump_native_menu_loop() {}

#[derive(Debug, Error)]
pub enum DesktopError {
    #[error("could not determine the current executable: {0}")]
    CurrentExecutable(io::Error),
    #[cfg(target_os = "linux")]
    #[error("the platform did not provide a configuration directory")]
    ConfigDirectoryUnavailable,
    #[cfg(target_os = "linux")]
    #[error("could not create autostart directory `{}`: {source}", path.display())]
    CreateAutostartDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[cfg(target_os = "linux")]
    #[error("could not write autostart entry `{}`: {source}", path.display())]
    WriteAutostart {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[cfg(target_os = "linux")]
    #[error("could not remove autostart entry `{}`: {source}", path.display())]
    RemoveAutostart {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[cfg(target_os = "windows")]
    #[error("could not run the Windows autostart command: {source}")]
    RunAutostartCommand {
        #[source]
        source: io::Error,
    },
    #[cfg(target_os = "windows")]
    #[error("the Windows autostart command failed: {0}")]
    AutostartCommandFailed(String),
    #[error("system tray unavailable: {0}")]
    Tray(String),
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::path::Path;

    #[cfg(target_os = "linux")]
    use super::linux_autostart_entry;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_autostart_quotes_the_executable() {
        let entry = linux_autostart_entry(Path::new("/opt/Tevir $Build/tevir"));

        assert!(entry.contains("Exec=\"/opt/Tevir \\$Build/tevir\""));
        assert!(entry.contains("Terminal=false"));
    }
}
