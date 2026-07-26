use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use domain::{Edge, InputEvent, Point};
use serde::Serialize;
use tokio::sync::mpsc as tokio_mpsc;

use crate::{
    BackendError,
    convert::{from_native, to_native},
    native_input::{NativeCapture, NativeCaptureEvent, NativeInjection},
    state::HeldInput,
};

pub use crate::native_input::DesktopGeometry;

const ENGINE_HANDLE: u64 = 1;
const ALL_CAPTURE_EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];
pub(crate) const COMMAND_QUEUE_CAPACITY: usize = 32;
pub const SERVICE_QUEUE_CAPACITY: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    LinuxWaylandClipboardPortal,
    LinuxWaylandInputCapture,
    LinuxWaylandRemoteDesktop,
    WindowsClipboard,
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
        edge_position: Option<f64>,
    },
    DesktopChanged(DesktopGeometry),
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
    DesktopChanged(DesktopGeometry),
    Applied {
        generation: u64,
        sequence: u64,
    },
    Released {
        generation: u64,
    },
    Failed {
        operation: &'static str,
        reason: String,
    },
    Stopped,
}

enum CaptureCommand {
    Configure(Vec<Edge>),
    Release,
}

enum InjectionCommand {
    BeginSession {
        generation: u64,
    },
    ApplyBatch {
        generation: u64,
        sequence: u64,
        events: Vec<InputEvent>,
    },
    WarpCursor {
        generation: u64,
        position: Point,
    },
    ReleaseAll {
        generation: u64,
    },
}

#[derive(Clone)]
pub struct CaptureService {
    commands: tokio_mpsc::Sender<CaptureCommand>,
    events: Arc<Mutex<Receiver<CaptureServiceEvent>>>,
    ready: Arc<AtomicBool>,
    _worker: Arc<ServiceWorker>,
}

impl std::fmt::Debug for CaptureService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureService")
            .field("ready", &self.is_ready())
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
        let ready = Arc::new(AtomicBool::new(false));
        let edges = edges.to_vec();
        let worker_ready = ready.clone();
        let worker = thread::Builder::new()
            .name(String::from("tevir-capture"))
            .spawn(move || {
                run_local_worker(run_capture(command_rx, event_tx, edges, worker_ready));
            })
            .map_err(|error| BackendError::Operation {
                operation: "start capture worker",
                reason: error.to_string(),
            })?;

        Ok(Self {
            commands,
            events: Arc::new(Mutex::new(events)),
            ready,
            _worker: Arc::new(ServiceWorker::new(worker)),
        })
    }

    pub fn configure(&self, edges: &[Edge]) -> Result<(), BackendError> {
        validate_edges(edges)?;
        self.send(CaptureCommand::Configure(edges.to_vec()))
    }

    pub fn release(&self) -> Result<(), BackendError> {
        self.send(CaptureCommand::Release)
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self._worker.is_finished()
    }

    #[must_use]
    pub const fn backend(&self) -> BackendKind {
        crate::native_capture_kind()
    }

    pub fn try_recv(&self) -> Result<CaptureServiceEvent, TryRecvError> {
        self.events
            .lock()
            .map_or(Err(TryRecvError::Disconnected), |events| events.try_recv())
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<CaptureServiceEvent, RecvTimeoutError> {
        self.events
            .lock()
            .map_or(Err(RecvTimeoutError::Disconnected), |events| {
                events.recv_timeout(timeout)
            })
    }

    fn send(&self, command: CaptureCommand) -> Result<(), BackendError> {
        self.commands.try_send(command).map_err(map_try_send_error)
    }
}

#[derive(Clone)]
pub struct InjectionService {
    commands: tokio_mpsc::Sender<InjectionCommand>,
    events: Arc<Mutex<Receiver<InjectionServiceEvent>>>,
    ready: Arc<AtomicBool>,
    ever_ready: Arc<AtomicBool>,
    next_generation: Arc<AtomicU64>,
    _worker: Arc<ServiceWorker>,
}

impl std::fmt::Debug for InjectionService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InjectionService")
            .field("ready", &self.is_ready())
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
        let ready = Arc::new(AtomicBool::new(false));
        let ever_ready = Arc::new(AtomicBool::new(false));
        let worker_ready = ready.clone();
        let worker_ever_ready = ever_ready.clone();
        let worker = thread::Builder::new()
            .name(String::from("tevir-injection"))
            .spawn(move || {
                run_local_worker(run_injection(
                    command_rx,
                    event_tx,
                    worker_ready,
                    worker_ever_ready,
                ));
            })
            .map_err(|error| BackendError::Operation {
                operation: "start injection worker",
                reason: error.to_string(),
            })?;

        Ok(Self {
            commands,
            events: Arc::new(Mutex::new(events)),
            ready,
            ever_ready,
            next_generation: Arc::new(AtomicU64::new(1)),
            _worker: Arc::new(ServiceWorker::new(worker)),
        })
    }

    pub fn begin_session(&self) -> Result<u64, BackendError> {
        let generation = take_injection_generation(&self.next_generation)?;
        self.send(InjectionCommand::BeginSession { generation })?;
        Ok(generation)
    }

    pub fn apply_batch(
        &self,
        generation: u64,
        sequence: u64,
        events: Vec<InputEvent>,
    ) -> Result<(), BackendError> {
        if events.is_empty() {
            return Err(BackendError::EmptyInputBatch);
        }
        self.send(InjectionCommand::ApplyBatch {
            generation,
            sequence,
            events,
        })
    }

    pub fn warp_cursor(&self, generation: u64, position: Point) -> Result<(), BackendError> {
        self.send(InjectionCommand::WarpCursor {
            generation,
            position,
        })
    }

    pub fn release_all(&self, generation: u64) -> Result<(), BackendError> {
        self.send(InjectionCommand::ReleaseAll { generation })
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn has_been_ready(&self) -> bool {
        self.ever_ready.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self._worker.is_finished()
    }

    #[must_use]
    pub const fn backend(&self) -> BackendKind {
        crate::native_emulation_kind()
    }

    pub fn try_recv(&self) -> Result<InjectionServiceEvent, TryRecvError> {
        self.events
            .lock()
            .map_or(Err(TryRecvError::Disconnected), |events| events.try_recv())
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<InjectionServiceEvent, RecvTimeoutError> {
        self.events
            .lock()
            .map_or(Err(RecvTimeoutError::Disconnected), |events| {
                events.recv_timeout(timeout)
            })
    }

    fn send(&self, command: InjectionCommand) -> Result<(), BackendError> {
        self.commands.try_send(command).map_err(map_try_send_error)
    }
}

struct ServiceWorker {
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl ServiceWorker {
    fn new(worker: JoinHandle<()>) -> Self {
        Self {
            worker: Mutex::new(Some(worker)),
        }
    }

    fn is_finished(&self) -> bool {
        self.worker.lock().map_or(true, |worker| {
            worker.as_ref().is_none_or(JoinHandle::is_finished)
        })
    }
}

impl Drop for ServiceWorker {
    fn drop(&mut self) {
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if worker.is_some_and(|worker| worker.join().is_err()) {
            tracing::error!("native input service worker panicked");
        }
    }
}

fn validate_edges(edges: &[Edge]) -> Result<(), BackendError> {
    let unique: HashSet<Edge> = edges.iter().copied().collect();
    if unique.len() != edges.len() {
        return Err(BackendError::Unavailable {
            reason: String::from("capture edges must be unique"),
        });
    }
    Ok(())
}

fn take_injection_generation(next: &AtomicU64) -> Result<u64, BackendError> {
    next.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |generation| {
        generation.checked_add(1)
    })
    .map_err(|_| BackendError::SessionGenerationExhausted)
}

pub(crate) fn map_try_send_error<T>(error: tokio_mpsc::error::TrySendError<T>) -> BackendError {
    match error {
        tokio_mpsc::error::TrySendError::Full(_) => BackendError::CommandQueueFull,
        tokio_mpsc::error::TrySendError::Closed(_) => BackendError::WorkerStopped,
    }
}

pub(crate) fn join_worker(worker: Option<JoinHandle<()>>) -> Result<(), BackendError> {
    if worker.is_some_and(|worker| worker.join().is_err()) {
        return Err(BackendError::WorkerPanicked);
    }
    Ok(())
}

pub(crate) fn run_local_worker(future: impl Future<Output = ()> + 'static) {
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
    ready: Arc<AtomicBool>,
) {
    let mut capture = match NativeCapture::new().await {
        Ok(capture) => capture,
        Err(error) => {
            send_capture_failure(&events, "open native capture", &error);
            return;
        }
    };

    for edge in ALL_CAPTURE_EDGES {
        if let Err(error) = capture.create(edge).await {
            send_capture_failure(&events, "create capture edge", &error);
            let _ = capture.terminate().await;
            return;
        }
    }

    if let Some(geometry) = capture.desktop_geometry()
        && events
            .send(CaptureServiceEvent::DesktopChanged(geometry))
            .is_err()
    {
        let _ = capture.terminate().await;
        return;
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
    ready.store(true, Ordering::Release);
    tracing::info!(backend = ?crate::native_capture_kind(), "input capture service ready");

    let mut enabled_edges: HashSet<Edge> = edges.into_iter().collect();
    let mut held = HeldInput::default();
    let mut accepting_input = false;
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(CaptureCommand::Configure(edges)) => {
                        let configured: HashSet<Edge> = edges.into_iter().collect();
                        if configured != enabled_edges {
                            accepting_input = false;
                            emit_capture_releases(&events, &mut held);
                            if let Err(error) = capture.release().await {
                                send_capture_failure(&events, "release capture", &error);
                            }
                            enabled_edges = configured;
                            tracing::info!(
                                edges = enabled_edges.len(),
                                "input capture edges configured"
                            );
                        }
                    }
                    Some(CaptureCommand::Release) => {
                        accepting_input = false;
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
                    Ok(NativeCaptureEvent::Activated {
                        edge,
                        edge_position,
                    }) => {
                        if enabled_edges.contains(&edge) {
                            accepting_input = true;
                            if events
                                .send(CaptureServiceEvent::Activated {
                                    edge,
                                    edge_position,
                                })
                                .is_err()
                            {
                                break;
                            }
                        } else {
                            accepting_input = false;
                            if let Err(error) = capture.release().await {
                                send_capture_failure(&events, "release unarmed capture", &error);
                            }
                        }
                    }
                    Ok(NativeCaptureEvent::Input(native)) => {
                        if !accepting_input {
                            continue;
                        }
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
                    #[cfg(target_os = "linux")]
                    Ok(NativeCaptureEvent::DesktopChanged(geometry)) => {
                        accepting_input = false;
                        emit_capture_releases(&events, &mut held);
                        if let Err(error) = capture.release().await {
                            send_capture_failure(&events, "release changed desktop", &error);
                        }
                        if events
                            .send(CaptureServiceEvent::DesktopChanged(geometry))
                            .is_err()
                        {
                            break;
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
    ready.store(false, Ordering::Release);
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
    ready: Arc<AtomicBool>,
    ever_ready: Arc<AtomicBool>,
) {
    let mut injection = match NativeInjection::new().await {
        Ok(injection) => injection,
        Err(error) => {
            send_injection_failure(&events, "open native injection", &error);
            return;
        }
    };
    if let Some(geometry) = injection.desktop_geometry()
        && events
            .send(InjectionServiceEvent::DesktopChanged(geometry))
            .is_err()
    {
        let _ = injection.terminate().await;
        return;
    }
    if events
        .send(InjectionServiceEvent::Ready {
            backend: crate::native_emulation_kind(),
        })
        .is_err()
    {
        let _ = injection.terminate().await;
        return;
    }
    ready.store(true, Ordering::Release);
    ever_ready.store(true, Ordering::Release);
    tracing::info!(
        backend = ?crate::native_emulation_kind(),
        "input injection service ready"
    );

    let mut held = HeldInput::default();
    let mut active_generation = 0;
    let mut display_tick = tokio::time::interval(Duration::from_millis(250));
    display_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    'commands: loop {
        tokio::select! {
        command = commands.recv() => {
        let Some(command) = command else {
            break;
        };
        match command {
            InjectionCommand::BeginSession { generation } => {
                release_injected_input(&mut injection, &events, &mut held, active_generation).await;
                active_generation = generation;
            }
            InjectionCommand::ApplyBatch {
                generation,
                sequence,
                events: batch,
            } => {
                if generation != active_generation {
                    tracing::debug!(
                        generation,
                        active_generation,
                        "discarded stale native input batch"
                    );
                    continue;
                }
                let mut applied = true;
                for event in batch {
                    match to_native(event) {
                        Ok(native) => {
                            if let Err(error) = injection.consume(native, ENGINE_HANDLE).await {
                                send_injection_failure(&events, "inject input", &error);
                                applied = false;
                            } else {
                                held.observe(event);
                            }
                        }
                        Err(error) => {
                            send_injection_failure(&events, "normalize remote input", &error);
                            applied = false;
                        }
                    }
                }
                if applied
                    && events
                        .send(InjectionServiceEvent::Applied {
                            generation,
                            sequence,
                        })
                        .is_err()
                {
                    break 'commands;
                }
            }
            InjectionCommand::WarpCursor {
                generation,
                position,
            } => {
                if generation == active_generation
                    && let Err(error) = injection.warp_cursor(position).await
                {
                    send_injection_failure(&events, "position remote pointer", &error);
                }
            }
            InjectionCommand::ReleaseAll { generation } => {
                if generation == active_generation {
                    release_injected_input(&mut injection, &events, &mut held, active_generation)
                        .await;
                }
            }
        }
        }
        _ = display_tick.tick() => {
            match injection.try_display_change() {
                Ok(Some(geometry)) => {
                    if events
                        .send(InjectionServiceEvent::DesktopChanged(geometry))
                        .is_err()
                    {
                        break 'commands;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    send_injection_failure(&events, "monitor native display", &error);
                    break 'commands;
                }
            }
        }
        }
    }

    release_injected_input(&mut injection, &events, &mut held, active_generation).await;
    let _ = injection.terminate().await;
    ready.store(false, Ordering::Release);
    let _ = events.send(InjectionServiceEvent::Stopped);
    tracing::info!("input injection service stopped");
}

async fn release_injected_input(
    injection: &mut NativeInjection,
    events: &SyncSender<InjectionServiceEvent>,
    held: &mut HeldInput,
    generation: u64,
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
    let _ = events.send(InjectionServiceEvent::Released { generation });
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use domain::Edge;

    use super::{take_injection_generation, validate_edges};
    use crate::BackendError;

    #[test]
    fn capture_edges_may_be_idle_but_must_be_unique() {
        assert!(validate_edges(&[]).is_ok());
        assert!(validate_edges(&[Edge::Left, Edge::Left]).is_err());
        assert!(validate_edges(&[Edge::Left, Edge::Right]).is_ok());
    }

    #[test]
    fn native_session_generations_do_not_wrap() {
        let next = AtomicU64::new(u64::MAX);

        assert!(matches!(
            take_injection_generation(&next),
            Err(BackendError::SessionGenerationExhausted)
        ));
    }
}
