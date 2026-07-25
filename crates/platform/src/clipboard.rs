use std::{
    num::NonZeroUsize,
    sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError},
    thread::{self, JoinHandle},
    time::Duration,
};

use serde::Serialize;
use tokio::sync::mpsc as tokio_mpsc;

use crate::{
    BackendError,
    service::{
        BackendKind, COMMAND_QUEUE_CAPACITY, SERVICE_QUEUE_CAPACITY, join_worker,
        map_try_send_error, run_local_worker,
    },
};

#[cfg(target_os = "linux")]
const CLIPBOARD_IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardServiceEvent {
    Ready {
        backend: BackendKind,
    },
    Changed {
        text: String,
    },
    Applied,
    Failed {
        operation: &'static str,
        reason: String,
    },
    Stopped,
}

enum ClipboardCommand {
    Apply(String),
}

pub struct ClipboardService {
    commands: Option<tokio_mpsc::Sender<ClipboardCommand>>,
    events: Receiver<ClipboardServiceEvent>,
    worker: Option<JoinHandle<()>>,
    maximum_text_bytes: usize,
}

impl std::fmt::Debug for ClipboardService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardService")
            .field("running", &self.commands.is_some())
            .field("maximum_text_bytes", &self.maximum_text_bytes)
            .finish_non_exhaustive()
    }
}

impl ClipboardService {
    pub fn start(maximum_text_bytes: NonZeroUsize) -> Result<Self, BackendError> {
        tracing::info!(
            backend = ?native_clipboard_kind(),
            maximum_text_bytes = maximum_text_bytes.get(),
            "starting clipboard service"
        );
        let (commands, command_rx) = tokio_mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (event_tx, events) = mpsc::sync_channel(SERVICE_QUEUE_CAPACITY);
        let maximum_text_bytes = maximum_text_bytes.get();
        let worker = thread::Builder::new()
            .name(String::from("tevir-clipboard"))
            .spawn(move || {
                run_local_worker(run_clipboard(command_rx, event_tx, maximum_text_bytes));
            })
            .map_err(|error| BackendError::Operation {
                operation: "start clipboard worker",
                reason: error.to_string(),
            })?;

        Ok(Self {
            commands: Some(commands),
            events,
            worker: Some(worker),
            maximum_text_bytes,
        })
    }

    pub fn apply(&self, text: impl Into<String>) -> Result<(), BackendError> {
        let text = text.into();
        validate_text_length(&text, self.maximum_text_bytes)?;
        self.commands
            .as_ref()
            .ok_or(BackendError::WorkerStopped)?
            .try_send(ClipboardCommand::Apply(text))
            .map_err(map_try_send_error)
    }

    pub fn try_recv(&self) -> Result<ClipboardServiceEvent, TryRecvError> {
        self.events.try_recv()
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ClipboardServiceEvent, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    pub fn shutdown(mut self) -> Result<(), BackendError> {
        self.commands.take();
        join_worker(self.worker.take())
    }
}

impl Drop for ClipboardService {
    fn drop(&mut self) {
        self.commands.take();
    }
}

const fn native_clipboard_kind() -> BackendKind {
    #[cfg(target_os = "linux")]
    {
        BackendKind::LinuxWaylandClipboardPortal
    }
    #[cfg(target_os = "windows")]
    {
        BackendKind::WindowsClipboard
    }
}

fn validate_text_length(text: &str, maximum: usize) -> Result<(), BackendError> {
    if text.len() > maximum {
        return Err(BackendError::ClipboardTextTooLarge {
            actual: text.len(),
            maximum,
        });
    }
    Ok(())
}

fn send_failure(
    events: &SyncSender<ClipboardServiceEvent>,
    operation: &'static str,
    error: &impl std::fmt::Display,
) {
    tracing::error!(operation, error = %error, "clipboard service failed");
    let _ = events.send(ClipboardServiceEvent::Failed {
        operation,
        reason: error.to_string(),
    });
}

#[cfg(target_os = "linux")]
async fn run_clipboard(
    commands: tokio_mpsc::Receiver<ClipboardCommand>,
    events: SyncSender<ClipboardServiceEvent>,
    maximum_text_bytes: usize,
) {
    if let Err(error) = run_wayland_clipboard(commands, &events, maximum_text_bytes).await {
        send_failure(&events, "run Wayland clipboard portal", &error);
    }
    let _ = events.send(ClipboardServiceEvent::Stopped);
    tracing::info!("clipboard service stopped");
}

#[cfg(target_os = "linux")]
async fn run_wayland_clipboard(
    mut commands: tokio_mpsc::Receiver<ClipboardCommand>,
    events: &SyncSender<ClipboardServiceEvent>,
    maximum_text_bytes: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    use ashpd::desktop::{
        CreateSessionOptions,
        clipboard::{Clipboard, RequestClipboardOptions, SetSelectionOptions},
        remote_desktop::{RemoteDesktop, StartOptions},
    };
    use futures_util::StreamExt;

    const TEXT_MIME_TYPES: [&str; 2] = ["text/plain;charset=utf-8", "text/plain"];

    let remote_desktop = RemoteDesktop::new().await?;
    let clipboard = Clipboard::new().await?;
    let session = remote_desktop
        .create_session(CreateSessionOptions::default())
        .await?;
    let session_key = serde_json::to_string(&session)?;

    clipboard
        .request(&session, RequestClipboardOptions::default())
        .await?;
    let mut owner_changes = Box::pin(
        clipboard
            .receive_selection_owner_changed::<RemoteDesktop>()
            .await?,
    );
    let mut transfers = Box::pin(
        clipboard
            .receive_selection_transfer::<RemoteDesktop>()
            .await?,
    );
    let mut closed = Box::pin(session.receive_closed().await?);

    let response = remote_desktop
        .start(&session, None, StartOptions::default())
        .await?
        .response()?;
    if !response.is_clipboard_enabled() {
        return Err("the desktop portal did not grant clipboard access".into());
    }

    if events
        .send(ClipboardServiceEvent::Ready {
            backend: native_clipboard_kind(),
        })
        .is_err()
    {
        let _ = session.close().await;
        return Ok(());
    }
    tracing::info!("Wayland clipboard portal ready");

    let mut owned_text: Option<String> = None;
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(ClipboardCommand::Apply(text)) = command else {
                    break;
                };
                let options = SetSelectionOptions::default().set_mime_types(&TEXT_MIME_TYPES);
                match clipboard.set_selection(&session, options).await {
                    Ok(()) => {
                        owned_text = Some(text);
                        if events.send(ClipboardServiceEvent::Applied).is_err() {
                            break;
                        }
                    }
                    Err(error) => send_failure(events, "set Wayland clipboard", &error),
                }
            }
            owner_change = owner_changes.next() => {
                let Some((event_session, change)) = owner_change else {
                    return Err("clipboard owner signal stream ended".into());
                };
                if session_matches(&session_key, &event_session)
                    && change.session_is_owner() != Some(true)
                {
                    handle_owner_change(
                        &clipboard,
                        &session,
                        events,
                        maximum_text_bytes,
                        &change,
                    )
                    .await;
                }
            }
            transfer = transfers.next() => {
                let Some((event_session, mime_type, serial)) = transfer else {
                    return Err("clipboard transfer signal stream ended".into());
                };
                if session_matches(&session_key, &event_session) {
                    handle_transfer(
                        &clipboard,
                        &session,
                        owned_text.as_deref(),
                        &mime_type,
                        serial,
                    )
                    .await;
                }
            }
            closed_event = closed.next() => {
                if closed_event.is_some() {
                    return Err("clipboard portal session was closed".into());
                }
                return Err("clipboard portal close signal stream ended".into());
            }
        }
    }

    let _ = clipboard
        .set_selection(&session, SetSelectionOptions::default())
        .await;
    let _ = session.close().await;
    Ok(())
}

#[cfg(target_os = "linux")]
fn session_matches<T>(expected: &str, session: &ashpd::desktop::Session<T>) -> bool
where
    T: ashpd::desktop::SessionPortal,
{
    serde_json::to_string(session).is_ok_and(|actual| actual == expected)
}

#[cfg(target_os = "linux")]
fn text_mime_type(mime_types: &[String]) -> Option<&str> {
    mime_types
        .iter()
        .find(|mime| {
            mime.to_ascii_lowercase()
                .replace(' ', "")
                .starts_with("text/plain;charset=utf-8")
        })
        .or_else(|| {
            mime_types
                .iter()
                .find(|mime| mime.eq_ignore_ascii_case("text/plain"))
        })
        .map(String::as_str)
}

#[cfg(target_os = "linux")]
async fn handle_owner_change(
    clipboard: &ashpd::desktop::clipboard::Clipboard,
    session: &ashpd::desktop::Session<ashpd::desktop::remote_desktop::RemoteDesktop>,
    events: &SyncSender<ClipboardServiceEvent>,
    maximum_text_bytes: usize,
    change: &ashpd::desktop::clipboard::SelectionOwnerChanged,
) {
    let Some(mime_type) = text_mime_type(change.mime_types()) else {
        return;
    };
    match read_selection(clipboard, session, mime_type, maximum_text_bytes).await {
        Ok(text) => {
            tracing::debug!(text_bytes = text.len(), "local clipboard changed");
            let _ = events.send(ClipboardServiceEvent::Changed { text });
        }
        Err(error) => send_failure(events, "read Wayland clipboard", &error),
    }
}

#[cfg(target_os = "linux")]
async fn read_selection(
    clipboard: &ashpd::desktop::clipboard::Clipboard,
    session: &ashpd::desktop::Session<ashpd::desktop::remote_desktop::RemoteDesktop>,
    mime_type: &str,
    maximum_text_bytes: usize,
) -> Result<String, Box<dyn std::error::Error>> {
    use tokio::io::AsyncReadExt;

    let fd: std::os::fd::OwnedFd = clipboard.selection_read(session, mime_type).await?.into();
    let file = std::fs::File::from(fd);
    let mut reader = tokio::fs::File::from_std(file).take(maximum_text_bytes as u64 + 1);
    let mut bytes = Vec::new();
    tokio::time::timeout(CLIPBOARD_IO_TIMEOUT, reader.read_to_end(&mut bytes))
        .await
        .map_err(|_| "timed out reading the Wayland clipboard")??;
    if bytes.len() > maximum_text_bytes {
        return Err(format!(
            "clipboard text is {} bytes; the maximum is {maximum_text_bytes}",
            bytes.len()
        )
        .into());
    }
    Ok(String::from_utf8(bytes)?)
}

#[cfg(target_os = "linux")]
async fn handle_transfer(
    clipboard: &ashpd::desktop::clipboard::Clipboard,
    session: &ashpd::desktop::Session<ashpd::desktop::remote_desktop::RemoteDesktop>,
    owned_text: Option<&str>,
    mime_type: &str,
    serial: u32,
) {
    let success = match owned_text.filter(|_| is_supported_text_mime(mime_type)) {
        Some(text) => match write_selection(clipboard, session, serial, text).await {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(
                    serial,
                    error = %error,
                    "could not write Wayland clipboard transfer"
                );
                false
            }
        },
        None => false,
    };
    if let Err(error) = clipboard
        .selection_write_done(session, serial, success)
        .await
    {
        tracing::error!(serial, error = %error, "could not finish Wayland clipboard transfer");
    }
}

#[cfg(target_os = "linux")]
const fn is_supported_text_mime(mime_type: &str) -> bool {
    matches!(
        mime_type.as_bytes(),
        b"text/plain" | b"text/plain;charset=utf-8"
    )
}

#[cfg(target_os = "linux")]
async fn write_selection(
    clipboard: &ashpd::desktop::clipboard::Clipboard,
    session: &ashpd::desktop::Session<ashpd::desktop::remote_desktop::RemoteDesktop>,
    serial: u32,
    text: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::AsyncWriteExt;

    let fd: std::os::fd::OwnedFd = clipboard.selection_write(session, serial).await?.into();
    let file = std::fs::File::from(fd);
    let mut writer = tokio::fs::File::from_std(file);
    tokio::time::timeout(CLIPBOARD_IO_TIMEOUT, async {
        writer.write_all(text.as_bytes()).await?;
        writer.shutdown().await
    })
    .await
    .map_err(|_| "timed out writing the Wayland clipboard")??;
    Ok(())
}

#[cfg(target_os = "windows")]
async fn run_clipboard(
    mut commands: tokio_mpsc::Receiver<ClipboardCommand>,
    events: SyncSender<ClipboardServiceEvent>,
    maximum_text_bytes: usize,
) {
    const POLL_INTERVAL: Duration = Duration::from_millis(250);

    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            send_failure(&events, "open Windows clipboard", &error);
            return;
        }
    };
    let mut last_seen = windows_text(&mut clipboard).unwrap_or_default();
    if events
        .send(ClipboardServiceEvent::Ready {
            backend: native_clipboard_kind(),
        })
        .is_err()
    {
        return;
    }
    tracing::info!("Windows clipboard service ready");

    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_read_error: Option<String> = None;
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(ClipboardCommand::Apply(text)) = command else {
                    break;
                };
                match clipboard.set_text(text.clone()) {
                    Ok(()) => {
                        last_seen = Some(text);
                        if events.send(ClipboardServiceEvent::Applied).is_err() {
                            break;
                        }
                    }
                    Err(error) => send_failure(&events, "set Windows clipboard", &error),
                }
            }
            _ = interval.tick() => {
                match windows_text(&mut clipboard) {
                    Ok(text) => {
                        last_read_error = None;
                        if text != last_seen {
                            last_seen.clone_from(&text);
                            if let Some(text) = text {
                                if let Err(error) = validate_text_length(&text, maximum_text_bytes) {
                                    send_failure(&events, "read Windows clipboard", &error);
                                } else if events.send(ClipboardServiceEvent::Changed { text }).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        if last_read_error.as_deref() != Some(reason.as_str()) {
                            send_failure(&events, "read Windows clipboard", &error);
                            last_read_error = Some(reason);
                        }
                    }
                }
            }
        }
    }
    let _ = events.send(ClipboardServiceEvent::Stopped);
    tracing::info!("clipboard service stopped");
}

#[cfg(target_os = "windows")]
fn windows_text(clipboard: &mut arboard::Clipboard) -> Result<Option<String>, arboard::Error> {
    match clipboard.get_text() {
        Ok(text) => Ok(Some(text)),
        Err(arboard::Error::ContentNotAvailable) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::validate_text_length;

    #[test]
    fn clipboard_text_limit_is_measured_in_utf8_bytes() {
        assert!(validate_text_length("four", 4).is_ok());
        assert!(validate_text_length("\u{00e9}", 2).is_ok());
        assert!(validate_text_length("\u{00e9}\u{00e9}", 3).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn text_mime_preference_is_deterministic() {
        let mime_types = vec![
            String::from("text/plain"),
            String::from("text/plain;charset=UTF-8"),
        ];
        assert_eq!(
            super::text_mime_type(&mime_types),
            Some("text/plain;charset=UTF-8")
        );
        assert_eq!(super::text_mime_type(&[String::from("image/png")]), None);
    }
}
