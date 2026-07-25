use std::{
    collections::HashSet,
    sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError},
    thread::{self, JoinHandle},
    time::Duration,
};

use domain::{Edge, InputEvent};
use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::mpsc as tokio_mpsc;

use crate::{
    BackendError,
    convert::{from_native, to_native},
    state::HeldInput,
};

const ENGINE_HANDLE: u64 = 1;
const COMMAND_QUEUE_CAPACITY: usize = 32;
pub const SERVICE_QUEUE_CAPACITY: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    LinuxWaylandInputCapture,
    LinuxWaylandRemoteDesktop,
    WindowsHooks,
    WindowsSendInput,
}

#[derive(Debug)]
pub enum CaptureServiceEvent {
    Ready {
        backend: BackendKind,
    },
    Activated {
        edge: Edge,
    },
    Input(InputEvent),
    Released,
    Failed {
        operation: &'static str,
        reason: String,
    },
    Stopped,
}

#[derive(Debug)]
pub enum InjectionServiceEvent {
    Ready {
        backend: BackendKind,
    },
    Released,
    Failed {
        operation: &'static str,
        reason: String,
    },
    Stopped,
}

enum CaptureCommand {
    Release,
}

enum InjectionCommand {
    Inject(InputEvent),
    ReleaseAll,
}

pub struct CaptureService {
    commands: Option<tokio_mpsc::Sender<CaptureCommand>>,
    events: Receiver<CaptureServiceEvent>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for CaptureService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureService")
            .field("running", &self.commands.is_some())
            .finish_non_exhaustive()
    }
}

impl CaptureService {
    pub fn start(edges: &[Edge]) -> Result<Self, BackendError> {
        validate_edges(edges)?;
        tracing::info!(
            backend = ?crate::native_capture_kind(),
            edges = edges.len(),
            "starting input capture service"
        );
        let (commands, command_rx) = tokio_mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (event_tx, events) = mpsc::sync_channel(SERVICE_QUEUE_CAPACITY);
        let edges = edges.to_vec();
        let worker = thread::Builder::new()
            .name(String::from("tevir-capture"))
            .spawn(move || run_local_worker(run_capture(command_rx, event_tx, edges)))
            .map_err(|error| BackendError::Operation {
                operation: "start capture worker",
                reason: error.to_string(),
            })?;

        Ok(Self {
            commands: Some(commands),
            events,
            worker: Some(worker),
        })
    }

    pub fn release(&self) -> Result<(), BackendError> {
        self.send(CaptureCommand::Release)
    }

    pub fn try_recv(&self) -> Result<CaptureServiceEvent, TryRecvError> {
        self.events.try_recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<CaptureServiceEvent, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    pub fn shutdown(mut self) -> Result<(), BackendError> {
        self.commands.take();
        join_worker(self.worker.take())
    }

    fn send(&self, command: CaptureCommand) -> Result<(), BackendError> {
        self.commands
            .as_ref()
            .ok_or(BackendError::WorkerStopped)?
            .try_send(command)
            .map_err(map_try_send_error)
    }
}

impl Drop for CaptureService {
    fn drop(&mut self) {
        self.commands.take();
    }
}

pub struct InjectionService {
    commands: Option<tokio_mpsc::Sender<InjectionCommand>>,
    events: Receiver<InjectionServiceEvent>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for InjectionService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InjectionService")
            .field("running", &self.commands.is_some())
            .finish_non_exhaustive()
    }
}

impl InjectionService {
    pub fn start() -> Result<Self, BackendError> {
        tracing::info!(
            backend = ?crate::native_emulation_kind(),
            "starting input injection service"
        );
        let (commands, command_rx) = tokio_mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (event_tx, events) = mpsc::sync_channel(SERVICE_QUEUE_CAPACITY);
        let worker = thread::Builder::new()
            .name(String::from("tevir-injection"))
            .spawn(move || run_local_worker(run_injection(command_rx, event_tx)))
            .map_err(|error| BackendError::Operation {
                operation: "start injection worker",
                reason: error.to_string(),
            })?;

        Ok(Self {
            commands: Some(commands),
            events,
            worker: Some(worker),
        })
    }

    pub fn inject(&self, event: InputEvent) -> Result<(), BackendError> {
        self.send(InjectionCommand::Inject(event))
    }

    pub fn release_all(&self) -> Result<(), BackendError> {
        self.send(InjectionCommand::ReleaseAll)
    }

    pub fn try_recv(&self) -> Result<InjectionServiceEvent, TryRecvError> {
        self.events.try_recv()
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<InjectionServiceEvent, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    pub fn shutdown(mut self) -> Result<(), BackendError> {
        self.commands.take();
        join_worker(self.worker.take())
    }

    fn send(&self, command: InjectionCommand) -> Result<(), BackendError> {
        self.commands
            .as_ref()
            .ok_or(BackendError::WorkerStopped)?
            .try_send(command)
            .map_err(map_try_send_error)
    }
}

impl Drop for InjectionService {
    fn drop(&mut self) {
        self.commands.take();
    }
}

fn validate_edges(edges: &[Edge]) -> Result<(), BackendError> {
    if edges.is_empty() {
        return Err(BackendError::Unavailable {
            reason: String::from("at least one capture edge is required"),
        });
    }
    let unique: HashSet<Edge> = edges.iter().copied().collect();
    if unique.len() != edges.len() {
        return Err(BackendError::Unavailable {
            reason: String::from("capture edges must be unique"),
        });
    }
    Ok(())
}

fn map_try_send_error<T>(error: tokio_mpsc::error::TrySendError<T>) -> BackendError {
    match error {
        tokio_mpsc::error::TrySendError::Full(_) => BackendError::CommandQueueFull,
        tokio_mpsc::error::TrySendError::Closed(_) => BackendError::WorkerStopped,
    }
}

fn join_worker(worker: Option<JoinHandle<()>>) -> Result<(), BackendError> {
    if worker.is_some_and(|worker| worker.join().is_err()) {
        return Err(BackendError::WorkerPanicked);
    }
    Ok(())
}

fn run_local_worker(future: impl Future<Output = ()> + 'static) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return,
    };
    tokio::task::LocalSet::new().block_on(&runtime, future);
}

async fn run_capture(
    mut commands: tokio_mpsc::Receiver<CaptureCommand>,
    events: SyncSender<CaptureServiceEvent>,
    edges: Vec<Edge>,
) {
    let mut capture =
        match capture_engine::InputCapture::new(Some(crate::native_capture_backend())).await {
            Ok(capture) => capture,
            Err(error) => {
                send_capture_failure(&events, "open native capture", &error);
                return;
            }
        };

    for (index, edge) in edges.iter().copied().enumerate() {
        let handle = index as u64 + 1;
        if let Err(error) = capture
            .create(handle, capture_position_from_edge(edge))
            .await
        {
            send_capture_failure(&events, "create capture edge", &error);
            let _ = capture.terminate().await;
            return;
        }
    }

    if events
        .send(CaptureServiceEvent::Ready {
            backend: crate::native_capture_kind(),
        })
        .is_err()
    {
        let _ = capture.terminate().await;
        return;
    }
    tracing::info!(backend = ?crate::native_capture_kind(), "input capture service ready");

    let mut held = HeldInput::default();
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(CaptureCommand::Release) => {
                        emit_capture_releases(&events, &mut held);
                        if let Err(error) = capture.release().await {
                            send_capture_failure(&events, "release capture", &error);
                        } else if events.send(CaptureServiceEvent::Released).is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            native = capture.next() => {
                let Some(native) = native else {
                    let _ = events.send(CaptureServiceEvent::Failed {
                        operation: "capture input",
                        reason: String::from("native capture stream ended"),
                    });
                    break;
                };
                match native {
                    Ok((handle, capture_engine::CaptureEvent::Begin)) => {
                        if let Some(edge) = edges.get(handle.saturating_sub(1) as usize)
                            && events.send(CaptureServiceEvent::Activated { edge: *edge }).is_err()
                        {
                            break;
                        }
                    }
                    Ok((_, capture_engine::CaptureEvent::Input(native))) => {
                        match from_native(native) {
                            Ok(Some(event)) => {
                                held.observe(event);
                                if events.send(CaptureServiceEvent::Input(event)).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => {}
                            Err(error) => send_capture_failure(
                                &events,
                                "normalize captured input",
                                &error,
                            ),
                        }
                    }
                    Err(error) => {
                        send_capture_failure(&events, "capture input", &error);
                        break;
                    }
                }
            }
        }
    }

    emit_capture_releases(&events, &mut held);
    let _ = capture.release().await;
    let _ = capture.terminate().await;
    let _ = events.send(CaptureServiceEvent::Stopped);
    tracing::info!("input capture service stopped");
}

fn emit_capture_releases(events: &SyncSender<CaptureServiceEvent>, held: &mut HeldInput) {
    for release in held.release_all(0) {
        if events.send(CaptureServiceEvent::Input(release)).is_err() {
            break;
        }
    }
}

async fn run_injection(
    mut commands: tokio_mpsc::Receiver<InjectionCommand>,
    events: SyncSender<InjectionServiceEvent>,
) {
    let mut injection = match emulation_engine::InputEmulation::new(Some(
        crate::native_emulation_backend(),
    ))
    .await
    {
        Ok(injection) => injection,
        Err(error) => {
            send_injection_failure(&events, "open native injection", &error);
            return;
        }
    };
    let _created = injection.create(ENGINE_HANDLE).await;
    if events
        .send(InjectionServiceEvent::Ready {
            backend: crate::native_emulation_kind(),
        })
        .is_err()
    {
        injection.terminate().await;
        return;
    }
    tracing::info!(
        backend = ?crate::native_emulation_kind(),
        "input injection service ready"
    );

    let mut held = HeldInput::default();
    while let Some(command) = commands.recv().await {
        match command {
            InjectionCommand::Inject(event) => match to_native(event) {
                Ok(native) => {
                    if let Err(error) = injection.consume(native, ENGINE_HANDLE).await {
                        send_injection_failure(&events, "inject input", &error);
                    } else {
                        held.observe(event);
                    }
                }
                Err(error) => send_injection_failure(&events, "normalize remote input", &error),
            },
            InjectionCommand::ReleaseAll => {
                release_injected_input(&mut injection, &events, &mut held).await;
            }
        }
    }

    release_injected_input(&mut injection, &events, &mut held).await;
    injection.destroy(ENGINE_HANDLE).await;
    injection.terminate().await;
    let _ = events.send(InjectionServiceEvent::Stopped);
    tracing::info!("input injection service stopped");
}

async fn release_injected_input(
    injection: &mut emulation_engine::InputEmulation,
    events: &SyncSender<InjectionServiceEvent>,
    held: &mut HeldInput,
) {
    for release in held.release_all(0) {
        match to_native(release) {
            Ok(native) => {
                if let Err(error) = injection.consume(native, ENGINE_HANDLE).await {
                    send_injection_failure(events, "release held input", &error);
                }
            }
            Err(error) => send_injection_failure(events, "normalize held input", &error),
        }
    }
    if let Err(error) = injection.release_keys(ENGINE_HANDLE).await {
        send_injection_failure(events, "release held keys", &error);
    }
    let _ = events.send(InjectionServiceEvent::Released);
}

fn send_capture_failure(
    events: &SyncSender<CaptureServiceEvent>,
    operation: &'static str,
    error: &impl std::fmt::Display,
) {
    tracing::error!(operation, error = %error, "input capture service failed");
    let _ = events.send(CaptureServiceEvent::Failed {
        operation,
        reason: error.to_string(),
    });
}

fn send_injection_failure(
    events: &SyncSender<InjectionServiceEvent>,
    operation: &'static str,
    error: &impl std::fmt::Display,
) {
    tracing::error!(operation, error = %error, "input injection service failed");
    let _ = events.send(InjectionServiceEvent::Failed {
        operation,
        reason: error.to_string(),
    });
}

const fn capture_position_from_edge(edge: Edge) -> capture_engine::Position {
    match edge {
        Edge::Left => capture_engine::Position::Left,
        Edge::Right => capture_engine::Position::Right,
        Edge::Top => capture_engine::Position::Top,
        Edge::Bottom => capture_engine::Position::Bottom,
    }
}

#[cfg(test)]
mod tests {
    use domain::Edge;

    use super::validate_edges;

    #[test]
    fn capture_edges_must_be_nonempty_and_unique() {
        assert!(validate_edges(&[]).is_err());
        assert!(validate_edges(&[Edge::Left, Edge::Left]).is_err());
        assert!(validate_edges(&[Edge::Left, Edge::Right]).is_ok());
    }
}
