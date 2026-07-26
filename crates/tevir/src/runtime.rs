use std::{
    collections::{BTreeMap, VecDeque},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    num::NonZeroUsize,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use domain::{
    DesktopLayout, Edge, InputEvent, InputKind, KeyAction, NodeId, PhysicalKey, Point, Rect,
    ScreenPlacement, Topology,
};
use identity::{LocalIdentity, TrustStore};
use platform::{
    BackendKind, CaptureService, CaptureServiceEvent, ClipboardService, ClipboardServiceEvent,
    DesktopGeometry, InjectionService, InjectionServiceEvent,
};
use protocol::{
    Capabilities, ClipboardGeneration, ClipboardText, MAX_CLIPBOARD_TEXT_BYTES, Session,
};
use session::{
    AgentAction, AgentSession, ClipboardAction, ClipboardSession, ControllerAction,
    ControllerSession,
};
use thiserror::Error;
use tokio::sync::mpsc as tokio_mpsc;
use transport::{
    ClipboardEndpoint, ControlReceiver, ControlSender, PeerConnection, ReconnectPolicy,
    SecureClient, SecureServer, SessionLimits, SessionProfile, TransportError,
};

use crate::config::{Config, EdgeBehavior, Role};

const COMMAND_CAPACITY: usize = 4;
const EVENT_CAPACITY: usize = 256;
const ACCEPT_CAPACITY: usize = 8;
const NETWORK_EVENT_CAPACITY: usize = 256;
const OUTBOUND_CAPACITY: usize = 128;
const CLIPBOARD_OUTBOUND_CAPACITY: usize = 8;
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(2);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

pub struct SessionRuntime {
    commands: tokio_mpsc::Sender<RuntimeCommand>,
    events: Receiver<RuntimeEvent>,
    worker: Option<JoinHandle<()>>,
}

impl SessionRuntime {
    pub fn start(
        config: Config,
        identity: LocalIdentity,
        trust: TrustStore,
        native_input: NativeInputHost,
    ) -> Result<Self, RuntimeStartError> {
        validate_trust(&config, &trust)?;
        if native_input.role() != runtime_role(&config.role) {
            return Err(RuntimeStartError::NativeRoleMismatch);
        }
        let (commands, command_rx) = tokio_mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, events) = mpsc::sync_channel(EVENT_CAPACITY);
        let worker = thread::Builder::new()
            .name(String::from("tevir-session"))
            .spawn(move || {
                run_worker(config, identity, trust, native_input, command_rx, event_tx);
            })
            .map_err(RuntimeStartError::Spawn)?;
        Ok(Self {
            commands,
            events,
            worker: Some(worker),
        })
    }

    pub fn try_recv(&self) -> Result<RuntimeEvent, TryRecvError> {
        self.events.try_recv()
    }

    pub fn reconfigure_controller(
        &self,
        topology: Topology,
        edge_behavior: EdgeBehavior,
    ) -> Result<(), RuntimeCommandError> {
        self.commands
            .try_send(RuntimeCommand::ReconfigureController {
                topology,
                edge_behavior,
            })
            .map_err(|_| RuntimeCommandError)
    }

    pub fn return_control(&self) -> Result<(), RuntimeCommandError> {
        self.commands
            .try_send(RuntimeCommand::ReturnLocal)
            .map_err(|_| RuntimeCommandError)
    }

    pub fn stop(mut self) {
        let _ = self.commands.try_send(RuntimeCommand::Stop);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("session worker panicked while stopping");
        }
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        let _ = self.commands.try_send(RuntimeCommand::Stop);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeRole {
    Controller,
    Agent,
}

#[derive(Clone, Debug)]
pub enum NativeInputHost {
    Controller(CaptureService),
    Agent(InjectionService),
}

impl NativeInputHost {
    pub fn start(role: RuntimeRole) -> Result<Self, platform::BackendError> {
        match role {
            RuntimeRole::Controller => Ok(Self::Controller(CaptureService::start(&[])?)),
            RuntimeRole::Agent => Ok(Self::Agent(InjectionService::start()?)),
        }
    }

    #[must_use]
    pub const fn role(&self) -> RuntimeRole {
        match self {
            Self::Controller(_) => RuntimeRole::Controller,
            Self::Agent(_) => RuntimeRole::Agent,
        }
    }

    #[must_use]
    pub fn should_restart_after_close(&self) -> bool {
        match self {
            // Capture construction completes before the portal reports whether
            // permission was granted, so automatic retries could reprompt after
            // a denial.
            Self::Controller(_) => false,
            Self::Agent(injection) => injection.has_been_ready(),
        }
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        match self {
            Self::Controller(capture) => capture.is_finished(),
            Self::Agent(injection) => injection.is_finished(),
        }
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        match self {
            Self::Controller(capture) => capture.is_ready(),
            Self::Agent(injection) => injection.is_ready(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    Starting {
        role: RuntimeRole,
    },
    Listening {
        address: SocketAddr,
    },
    Connecting {
        peer: NodeId,
        address: SocketAddr,
        attempt: u32,
    },
    Connected {
        peer: NodeId,
        session_id: u128,
    },
    Disconnected {
        peer: NodeId,
        reason: String,
    },
    NativeReady {
        backend: BackendKind,
    },
    FocusChanged {
        node: NodeId,
    },
    AgentControl {
        controller: NodeId,
        active: bool,
    },
    LocalDesktopChanged {
        geometry: DesktopGeometry,
    },
    DisplayChanged {
        screen: ScreenPlacement,
    },
    ClipboardReady {
        backend: BackendKind,
    },
    ClipboardSynchronized {
        peer: NodeId,
        received: bool,
    },
    ConfigurationApplied,
    Error {
        message: String,
    },
    Stopped,
}

enum RuntimeCommand {
    Stop,
    ReturnLocal,
    ReconfigureController {
        topology: Topology,
        edge_behavior: EdgeBehavior,
    },
}

#[derive(Clone, Copy, Debug, Error)]
#[error("session command queue is unavailable")]
pub struct RuntimeCommandError;

fn validate_trust(config: &Config, trust: &TrustStore) -> Result<(), RuntimeStartError> {
    match &config.role {
        Role::Controller { topology, .. } => {
            let mut has_peer = false;
            for screen in topology.screens() {
                if screen.node == config.node {
                    continue;
                }
                has_peer = true;
                if trust.get(&screen.node).is_none() {
                    return Err(RuntimeStartError::UntrustedPeer(screen.node.clone()));
                }
            }
            if !has_peer {
                return Err(RuntimeStartError::NoRemoteScreens);
            }
        }
        Role::Agent {
            controller_node, ..
        } => {
            if trust.get(controller_node).is_none() {
                return Err(RuntimeStartError::UntrustedPeer(controller_node.clone()));
            }
        }
    }
    Ok(())
}

fn run_worker(
    config: Config,
    identity: LocalIdentity,
    trust: TrustStore,
    native_input: NativeInputHost,
    commands: tokio_mpsc::Receiver<RuntimeCommand>,
    events: SyncSender<RuntimeEvent>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            send_event(
                &events,
                RuntimeEvent::Error {
                    message: format!("Could not start session runtime: {error}"),
                },
            );
            send_event(&events, RuntimeEvent::Stopped);
            return;
        }
    };
    let clipboard = match ClipboardService::start(
        NonZeroUsize::new(MAX_CLIPBOARD_TEXT_BYTES).unwrap_or(NonZeroUsize::MIN),
    ) {
        Ok(clipboard) => Some(clipboard),
        Err(error) => {
            tracing::warn!(%error, "clipboard synchronization is unavailable");
            send_event(
                &events,
                RuntimeEvent::Error {
                    message: format!("Clipboard synchronization is unavailable: {error}"),
                },
            );
            None
        }
    };
    let result = match (config.role.clone(), native_input) {
        (
            Role::Controller {
                listen,
                topology,
                edge_behavior,
            },
            NativeInputHost::Controller(capture),
        ) => {
            let result = runtime.block_on(run_controller(
                config.node,
                listen,
                topology,
                edge_behavior,
                identity,
                trust,
                capture.clone(),
                clipboard,
                commands,
                events.clone(),
            ));
            let _ = capture.release();
            let _ = capture.configure(&[]);
            result
        }
        (
            Role::Agent {
                controller_node,
                controller,
                display_layout,
            },
            NativeInputHost::Agent(injection),
        ) => runtime.block_on(run_agent(
            config.node,
            controller_node,
            controller,
            display_layout,
            identity,
            trust,
            injection.clone(),
            clipboard,
            commands,
            events.clone(),
        )),
        (Role::Controller { .. }, NativeInputHost::Agent(_))
        | (Role::Agent { .. }, NativeInputHost::Controller(_)) => Err(RuntimeError::Native(
            String::from("prepared native input role does not match the configuration"),
        )),
    };
    if let Err(error) = result {
        tracing::error!(error = %error, "session runtime failed");
        send_event(
            &events,
            RuntimeEvent::Error {
                message: error.to_string(),
            },
        );
    }
    send_event(&events, RuntimeEvent::Stopped);
}

#[allow(clippy::too_many_arguments)]
async fn run_controller(
    local_node: NodeId,
    listen: SocketAddr,
    mut topology: Topology,
    edge_behavior: EdgeBehavior,
    identity: LocalIdentity,
    trust: TrustStore,
    capture: CaptureService,
    clipboard: Option<ClipboardService>,
    mut commands: tokio_mpsc::Receiver<RuntimeCommand>,
    events: SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    send_event(
        &events,
        RuntimeEvent::Starting {
            role: RuntimeRole::Controller,
        },
    );
    let profile = session_profile(clipboard.is_some());
    let server = SecureServer::bind(listen, identity, &trust, profile, SessionLimits::default())
        .map_err(|error| RuntimeError::Transport(error.to_string()))?;
    let address = server
        .local_addr()
        .map_err(|error| RuntimeError::Transport(error.to_string()))?;
    send_event(&events, RuntimeEvent::Listening { address });
    let (mut accepted, accept_worker) = spawn_accept_worker(server);

    let edges = capture_edges(&topology, &local_node, edge_behavior);
    capture
        .configure(&edges)
        .map_err(|error| RuntimeError::Native(error.to_string()))?;
    if capture.is_ready() {
        send_event(
            &events,
            RuntimeEvent::NativeReady {
                backend: capture.backend(),
            },
        );
    }
    let mut controller = ControllerSession::new(topology.clone(), local_node.clone())
        .map_err(|error| RuntimeError::Session(error.to_string()))?;
    send_event(
        &events,
        RuntimeEvent::FocusChanged {
            node: local_node.clone(),
        },
    );

    let (network_tx, mut network_rx) = tokio_mpsc::channel(NETWORK_EVENT_CAPACITY);
    let mut peers = BTreeMap::<NodeId, ActivePeer>::new();
    let mut input_tick = tokio::time::interval(INPUT_POLL_INTERVAL);
    input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat_tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat_nonce = 0u64;
    let mut clipboard_state = ClipboardRuntimeState::new(clipboard);
    let mut edge_state = EdgeState {
        behavior: edge_behavior,
        pending_activation: None,
        emergency_shortcut: EmergencyShortcut::default(),
    };

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    None | Some(RuntimeCommand::Stop) => break,
                    Some(RuntimeCommand::ReturnLocal) => {
                        edge_state.pending_activation = None;
                        let actions = controller
                            .return_to_local()
                            .map_err(|error| RuntimeError::Session(error.to_string()))?;
                        apply_controller_actions(actions, &capture, &peers, &events)?;
                        send_event(&events, RuntimeEvent::FocusChanged {
                            node: controller.focus().clone(),
                        });
                        tracing::info!("control returned to the controller");
                    }
                    Some(RuntimeCommand::ReconfigureController {
                        topology: updated,
                        edge_behavior: updated_behavior,
                    }) => {
                        edge_state.pending_activation = None;
                        let actions = controller
                            .reconcile_topology(updated.clone())
                            .map_err(|error| RuntimeError::Session(error.to_string()))?;
                        apply_controller_actions(actions, &capture, &peers, &events)?;
                        topology = updated;
                        edge_state.behavior = updated_behavior;
                        let removed = peers
                            .keys()
                            .filter(|peer| topology.screen(peer).is_none())
                            .cloned()
                            .collect::<Vec<_>>();
                        for peer in removed {
                            if let Some(connection) = peers.remove(&peer) {
                                let _ = connection.outbound.try_send(Session::Disconnect);
                            }
                            send_event(&events, RuntimeEvent::Disconnected {
                                peer,
                                reason: String::from("Removed from the topology"),
                            });
                        }
                        capture
                            .configure(&capture_edges(
                                &topology,
                                &local_node,
                                edge_state.behavior,
                            ))
                            .map_err(|error| RuntimeError::Native(error.to_string()))?;
                        send_event(&events, RuntimeEvent::FocusChanged {
                            node: controller.focus().clone(),
                        });
                        send_event(&events, RuntimeEvent::ConfigurationApplied);
                        tracing::info!("controller topology and edge behavior applied live");
                    }
                }
            }
            result = accepted.recv() => {
                let Some(result) = result else {
                    return Err(RuntimeError::Transport(String::from(
                        "Secure connection listener stopped",
                    )));
                };
                match result {
                    Ok(connection) => {
                        accept_controller_peer(
                            connection,
                            &local_node,
                            &topology,
                            &mut controller,
                            &mut peers,
                            &network_tx,
                            &events,
                        )?;
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "incoming secure connection rejected");
                        send_event(&events, RuntimeEvent::Error {
                            message: format!("Incoming connection rejected: {error}"),
                        });
                    }
                }
            }
            network = network_rx.recv() => {
                if let Some(network) = network {
                    handle_controller_network(
                        network,
                        &mut topology,
                        &mut controller,
                        &capture,
                        &mut peers,
                        &mut clipboard_state,
                        &events,
                    )?;
                }
            }
            _ = input_tick.tick() => {
                drain_capture_events(
                    &capture,
                    &mut topology,
                    &local_node,
                    &mut controller,
                    &peers,
                    &mut edge_state,
                    &events,
                )?;
                complete_pending_activation(
                    &capture,
                    &mut controller,
                    &peers,
                    &mut edge_state.pending_activation,
                    &events,
                )?;
                let actions = controller
                    .flush()
                    .map_err(|error| RuntimeError::Session(error.to_string()))?;
                apply_controller_actions(actions, &capture, &peers, &events)?;
                drain_controller_clipboard(&local_node, &mut peers, &mut clipboard_state, &events)?;
            }
            _ = heartbeat_tick.tick() => {
                heartbeat_nonce = heartbeat_nonce.wrapping_add(1);
                for peer in peers.values() {
                    let _ = peer.outbound.try_send(Session::Heartbeat {
                        nonce: heartbeat_nonce,
                    });
                }
            }
        }
    }

    for peer in peers.values() {
        let _ = peer.outbound.try_send(Session::Disconnect);
    }
    accept_worker.abort();
    let _ = capture.release();
    Ok(())
}

fn spawn_accept_worker(
    server: SecureServer,
) -> (
    tokio_mpsc::Receiver<Result<PeerConnection, TransportError>>,
    tokio::task::JoinHandle<()>,
) {
    let (accepted_tx, accepted) = tokio_mpsc::channel(ACCEPT_CAPACITY);
    let worker = tokio::spawn(async move {
        loop {
            let result = server.accept().await;
            if accepted_tx.send(result).await.is_err() {
                break;
            }
        }
    });
    (accepted, worker)
}

fn accept_controller_peer(
    connection: PeerConnection,
    local_node: &NodeId,
    topology: &Topology,
    controller: &mut ControllerSession,
    peers: &mut BTreeMap<NodeId, ActivePeer>,
    network_tx: &tokio_mpsc::Sender<NetworkEvent>,
    events: &SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    let info = connection.info().clone();
    if topology.screen(&info.peer).is_none() {
        connection.close();
        send_event(
            events,
            RuntimeEvent::Error {
                message: format!(
                    "Trusted node `{}` is not present in the controller topology",
                    info.peer
                ),
            },
        );
        return Ok(());
    }
    controller
        .reset_peer(&info.peer)
        .map_err(|error| RuntimeError::Session(error.to_string()))?;
    let clipboard = info.negotiated_capabilities.clipboard_text.then(|| {
        let outbound = spawn_clipboard_worker(
            connection.clipboard_endpoint(),
            info.peer.clone(),
            info.session_id,
            network_tx.clone(),
        );
        ActiveClipboard {
            session: ClipboardSession::new(local_node.clone(), info.peer.clone()),
            outbound,
        }
    });
    let outbound = spawn_connection_worker(connection, network_tx.clone());
    peers.insert(
        info.peer.clone(),
        ActivePeer {
            session_id: info.session_id,
            outbound,
            clipboard,
        },
    );
    tracing::info!(peer = %info.peer, session_id = info.session_id, "controller peer connected");
    send_event(
        events,
        RuntimeEvent::Connected {
            peer: info.peer,
            session_id: info.session_id,
        },
    );
    Ok(())
}

fn drain_capture_events(
    capture: &CaptureService,
    topology: &mut Topology,
    local_node: &NodeId,
    controller: &mut ControllerSession,
    peers: &BTreeMap<NodeId, ActivePeer>,
    edge_state: &mut EdgeState,
    events: &SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    while let Ok(event) = capture.try_recv() {
        match event {
            CaptureServiceEvent::Ready { backend } => {
                send_event(events, RuntimeEvent::NativeReady { backend });
            }
            CaptureServiceEvent::Activated {
                edge,
                edge_position,
            } => {
                let position = edge_position.unwrap_or(0.5).clamp(0.0, 1.0);
                tracing::info!(?edge, position, "controller capture edge activated");
                if !edge_state.behavior.allows(edge, position) {
                    tracing::info!(
                        ?edge,
                        position,
                        "capture released because the edge policy excludes this position"
                    );
                    capture
                        .release()
                        .map_err(|error| RuntimeError::Native(error.to_string()))?;
                    continue;
                }
                let Some((target, offset)) =
                    activation_target(topology, local_node, edge, edge_position)
                else {
                    tracing::info!(
                        ?edge,
                        position,
                        "capture released because no desktop is reachable here"
                    );
                    capture
                        .release()
                        .map_err(|error| RuntimeError::Native(error.to_string()))?;
                    continue;
                };
                if !peers.contains_key(&target) {
                    tracing::info!(
                        ?edge,
                        position,
                        %target,
                        "capture released because the target is disconnected"
                    );
                    capture
                        .release()
                        .map_err(|error| RuntimeError::Native(error.to_string()))?;
                    send_event(
                        events,
                        RuntimeEvent::Error {
                            message: format!("`{target}` is not connected"),
                        },
                    );
                    continue;
                }
                if edge_state.behavior.switch_delay_ms == 0 {
                    tracing::info!(?edge, offset, %target, "transferring control to peer");
                    activate_controller(edge, offset, controller, capture, peers, events)?;
                } else {
                    edge_state.pending_activation = Some(PendingActivation {
                        edge,
                        offset,
                        target,
                        ready_at: Instant::now()
                            + Duration::from_millis(u64::from(edge_state.behavior.switch_delay_ms)),
                    });
                }
            }
            CaptureServiceEvent::DesktopChanged(geometry) => {
                if edge_state.pending_activation.take().is_some() {
                    capture
                        .release()
                        .map_err(|error| RuntimeError::Native(error.to_string()))?;
                }
                let updated = resize_topology_screen(topology, local_node, &geometry.layout)
                    .map_err(RuntimeError::Session)?;
                if updated != *topology {
                    let actions = controller
                        .reconcile_topology(updated.clone())
                        .map_err(|error| RuntimeError::Session(error.to_string()))?;
                    apply_controller_actions(actions, capture, peers, events)?;
                    *topology = updated;
                    send_event(
                        events,
                        RuntimeEvent::FocusChanged {
                            node: controller.focus().clone(),
                        },
                    );
                }
                send_event(events, RuntimeEvent::LocalDesktopChanged { geometry });
            }
            CaptureServiceEvent::Input(event) => {
                if edge_state.emergency_shortcut.observe(event) {
                    edge_state.pending_activation = None;
                    let actions = controller
                        .return_to_local()
                        .map_err(|error| RuntimeError::Session(error.to_string()))?;
                    apply_controller_actions(actions, capture, peers, events)?;
                    send_event(
                        events,
                        RuntimeEvent::FocusChanged {
                            node: controller.focus().clone(),
                        },
                    );
                    tracing::info!("emergency shortcut returned control to the controller");
                    continue;
                }
                if edge_state.pending_activation.is_some() {
                    continue;
                }
                if controller.focus() == local_node {
                    continue;
                }
                let previous_focus = controller.focus().clone();
                let actions = controller
                    .route_input(event)
                    .map_err(|error| RuntimeError::Session(error.to_string()))?;
                apply_controller_actions(actions, capture, peers, events)?;
                if controller.focus() != &previous_focus {
                    send_event(
                        events,
                        RuntimeEvent::FocusChanged {
                            node: controller.focus().clone(),
                        },
                    );
                }
            }
            CaptureServiceEvent::Released => {
                edge_state.pending_activation = None;
            }
            CaptureServiceEvent::Failed { operation, reason } => {
                return Err(RuntimeError::Native(format!("{operation}: {reason}")));
            }
            CaptureServiceEvent::Stopped => {
                return Err(RuntimeError::Native(String::from(
                    "Input capture service stopped",
                )));
            }
        }
    }
    Ok(())
}

fn complete_pending_activation(
    capture: &CaptureService,
    controller: &mut ControllerSession,
    peers: &BTreeMap<NodeId, ActivePeer>,
    pending: &mut Option<PendingActivation>,
    events: &SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    if pending
        .as_ref()
        .is_none_or(|pending| pending.ready_at > Instant::now())
    {
        return Ok(());
    }
    let Some(pending) = pending.take() else {
        return Ok(());
    };
    if !peers.contains_key(&pending.target) {
        capture
            .release()
            .map_err(|error| RuntimeError::Native(error.to_string()))?;
        return Ok(());
    }
    activate_controller(
        pending.edge,
        pending.offset,
        controller,
        capture,
        peers,
        events,
    )
}

fn activate_controller(
    edge: Edge,
    offset: u32,
    controller: &mut ControllerSession,
    capture: &CaptureService,
    peers: &BTreeMap<NodeId, ActivePeer>,
    events: &SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    let actions = controller
        .activate(edge, offset)
        .map_err(|error| RuntimeError::Session(error.to_string()))?;
    apply_controller_actions(actions, capture, peers, events)?;
    send_event(
        events,
        RuntimeEvent::FocusChanged {
            node: controller.focus().clone(),
        },
    );
    Ok(())
}

fn apply_controller_actions(
    actions: Vec<ControllerAction>,
    capture: &CaptureService,
    peers: &BTreeMap<NodeId, ActivePeer>,
    events: &SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    for action in actions {
        match action {
            ControllerAction::Send { peer, message } => {
                let Some(connection) = peers.get(&peer) else {
                    send_event(
                        events,
                        RuntimeEvent::Error {
                            message: format!("`{peer}` is not connected"),
                        },
                    );
                    continue;
                };
                connection
                    .outbound
                    .try_send(message)
                    .map_err(|_| RuntimeError::OutboundQueue(peer))?;
            }
            ControllerAction::ReleaseCapture => capture
                .release()
                .map_err(|error| RuntimeError::Native(error.to_string()))?,
        }
    }
    Ok(())
}

fn drain_controller_clipboard(
    local_node: &NodeId,
    peers: &mut BTreeMap<NodeId, ActivePeer>,
    state: &mut ClipboardRuntimeState,
    events: &SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    let native_events = state.service.as_ref().map_or_else(Vec::new, |service| {
        std::iter::from_fn(|| service.try_recv().ok()).collect()
    });
    for event in native_events {
        match event {
            ClipboardServiceEvent::Ready { backend } => {
                send_event(events, RuntimeEvent::ClipboardReady { backend });
            }
            ClipboardServiceEvent::Changed { text } => {
                let peer_ids = peers
                    .iter()
                    .filter(|(_, active)| active.clipboard.is_some())
                    .map(|(peer, _)| peer.clone())
                    .collect::<Vec<_>>();
                for peer in peer_ids {
                    let actions = peers
                        .get_mut(&peer)
                        .and_then(|active| active.clipboard.as_mut())
                        .ok_or_else(|| RuntimeError::Session(String::from("clipboard peer lost")))?
                        .session
                        .local_changed(text.clone())
                        .map_err(|error| RuntimeError::Session(error.to_string()))?;
                    apply_controller_clipboard_actions(&peer, actions, peers, state)?;
                }
                tracing::debug!(node = %local_node, bytes = text.len(), "local clipboard offered");
            }
            ClipboardServiceEvent::Applied => {
                let Some((peer, generation)) = state.applications.pending.take() else {
                    return Err(RuntimeError::Session(String::from(
                        "native clipboard applied without a pending generation",
                    )));
                };
                let Some(clipboard) = peers
                    .get_mut(&peer)
                    .and_then(|active| active.clipboard.as_mut())
                else {
                    tracing::debug!(%peer, "clipboard peer disconnected during application");
                    continue;
                };
                let actions = clipboard
                    .session
                    .confirm_applied(&generation)
                    .map_err(|error| RuntimeError::Session(error.to_string()))?;
                apply_controller_clipboard_actions(&peer, actions, peers, state)?;
                send_event(
                    events,
                    RuntimeEvent::ClipboardSynchronized {
                        peer,
                        received: true,
                    },
                );
            }
            ClipboardServiceEvent::Failed { operation, reason } => {
                tracing::warn!(operation, %reason, "clipboard service failed");
                send_event(
                    events,
                    RuntimeEvent::Error {
                        message: format!("Clipboard synchronization stopped: {reason}"),
                    },
                );
                state.service.take();
                state.applications = ClipboardApplications::new();
            }
            ClipboardServiceEvent::Stopped => {
                state.service.take();
                state.applications = ClipboardApplications::new();
                tracing::warn!("native clipboard service stopped");
            }
        }
    }
    start_next_clipboard_application(state.service.as_ref(), &mut state.applications)
}

fn apply_controller_clipboard_actions(
    peer: &NodeId,
    actions: Vec<ClipboardAction>,
    peers: &BTreeMap<NodeId, ActivePeer>,
    state: &mut ClipboardRuntimeState,
) -> Result<(), RuntimeError> {
    for action in actions {
        match action {
            ClipboardAction::SendControl(control) => peers
                .get(peer)
                .ok_or_else(|| RuntimeError::Session(format!("clipboard peer `{peer}` is gone")))?
                .outbound
                .try_send(Session::Clipboard(control))
                .map_err(|_| RuntimeError::OutboundQueue(peer.clone()))?,
            ClipboardAction::SendTransfer(transfer) => peers
                .get(peer)
                .and_then(|active| active.clipboard.as_ref())
                .ok_or_else(|| {
                    RuntimeError::Session(format!("clipboard peer `{peer}` is unavailable"))
                })?
                .outbound
                .try_send(transfer)
                .map_err(|_| {
                    RuntimeError::Session(format!("clipboard transfer queue for `{peer}` is full"))
                })?,
            ClipboardAction::ApplyRemote(transfer) => {
                state
                    .applications
                    .queued
                    .push_back((peer.clone(), transfer));
            }
        }
    }
    start_next_clipboard_application(state.service.as_ref(), &mut state.applications)
}

fn start_next_clipboard_application(
    service: Option<&ClipboardService>,
    applications: &mut ClipboardApplications,
) -> Result<(), RuntimeError> {
    if applications.pending.is_some() {
        return Ok(());
    }
    let Some((peer, transfer)) = applications.queued.pop_front() else {
        return Ok(());
    };
    let service = service.ok_or_else(|| {
        RuntimeError::Native(String::from("native clipboard service is unavailable"))
    })?;
    service
        .apply(transfer.text())
        .map_err(|error| RuntimeError::Native(error.to_string()))?;
    applications.pending = Some((peer, transfer.generation().clone()));
    Ok(())
}

fn handle_controller_network(
    event: NetworkEvent,
    topology: &mut Topology,
    controller: &mut ControllerSession,
    capture: &CaptureService,
    peers: &mut BTreeMap<NodeId, ActivePeer>,
    clipboard_state: &mut ClipboardRuntimeState,
    events: &SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    match event {
        NetworkEvent::Message {
            peer,
            session_id,
            message,
        } if active_session(peers, &peer, session_id) => match message {
            Session::DisplayChanged { layout } => {
                let updated = match resize_topology_screen(topology, &peer, &layout) {
                    Ok(updated) => updated,
                    Err(error) => {
                        tracing::warn!(%peer, %error, "agent display update rejected");
                        send_event(
                            events,
                            RuntimeEvent::Error {
                                message: format!(
                                    "Could not apply the display reported by `{peer}`: {error}"
                                ),
                            },
                        );
                        return Ok(());
                    }
                };
                let screen = updated
                    .screen(&peer)
                    .cloned()
                    .ok_or_else(|| RuntimeError::Session(format!("unknown peer `{peer}`")))?;
                if updated != *topology {
                    let actions = controller
                        .reconcile_topology(updated.clone())
                        .map_err(|error| RuntimeError::Session(error.to_string()))?;
                    apply_controller_actions(actions, capture, peers, events)?;
                    *topology = updated;
                    send_event(
                        events,
                        RuntimeEvent::FocusChanged {
                            node: controller.focus().clone(),
                        },
                    );
                } else if let Some(connection) = peers.get(&peer) {
                    connection
                        .outbound
                        .try_send(Session::FocusChanged {
                            focus_epoch: controller.focus_epoch(),
                            target: controller.focus().clone(),
                            entry_position: controller.focus_position(),
                        })
                        .map_err(|_| RuntimeError::OutboundQueue(peer.clone()))?;
                }
                tracing::info!(
                    peer = %peer,
                    width = layout.size().width.get(),
                    height = layout.size().height.get(),
                    monitors = layout.monitor_count(),
                    "agent display synchronized"
                );
                send_event(events, RuntimeEvent::DisplayChanged { screen });
            }
            Session::InputAcknowledged { through_sequence } => {
                controller
                    .acknowledge(&peer, through_sequence)
                    .map_err(|error| RuntimeError::Session(error.to_string()))?;
            }
            Session::Heartbeat { nonce } => {
                if let Some(connection) = peers.get(&peer) {
                    let _ = connection
                        .outbound
                        .try_send(Session::HeartbeatAcknowledged { nonce });
                }
            }
            Session::HeartbeatAcknowledged { .. } => {}
            Session::Clipboard(control) => {
                let acknowledged = matches!(&control, protocol::ClipboardControl::Applied { .. });
                let actions = peers
                    .get_mut(&peer)
                    .and_then(|active| active.clipboard.as_mut())
                    .ok_or_else(|| {
                        RuntimeError::Session(format!(
                            "peer `{peer}` sent clipboard control without negotiating it"
                        ))
                    })?
                    .session
                    .receive_control(control)
                    .map_err(|error| RuntimeError::Session(error.to_string()))?;
                apply_controller_clipboard_actions(&peer, actions, peers, clipboard_state)?;
                if acknowledged {
                    send_event(
                        events,
                        RuntimeEvent::ClipboardSynchronized {
                            peer,
                            received: false,
                        },
                    );
                }
            }
            Session::Disconnect => {
                disconnect_controller_peer(
                    peer,
                    session_id,
                    String::from("Peer closed the session"),
                    controller,
                    capture,
                    peers,
                    events,
                )?;
            }
            Session::FocusChanged { .. } | Session::Input(_) => {
                send_event(
                    events,
                    RuntimeEvent::Error {
                        message: format!("Unexpected message from `{peer}`"),
                    },
                );
            }
        },
        NetworkEvent::Disconnected {
            peer,
            session_id,
            reason,
        } => disconnect_controller_peer(
            peer, session_id, reason, controller, capture, peers, events,
        )?,
        NetworkEvent::ClipboardTransfer {
            peer,
            session_id,
            transfer,
        } if active_session(peers, &peer, session_id) => {
            let actions = peers
                .get_mut(&peer)
                .and_then(|active| active.clipboard.as_mut())
                .ok_or_else(|| {
                    RuntimeError::Session(format!(
                        "peer `{peer}` sent clipboard data without negotiating it"
                    ))
                })?
                .session
                .receive_transfer(transfer)
                .map_err(|error| RuntimeError::Session(error.to_string()))?;
            apply_controller_clipboard_actions(&peer, actions, peers, clipboard_state)?;
        }
        NetworkEvent::ClipboardFailed {
            peer,
            session_id,
            reason,
        } if active_session(peers, &peer, session_id) => {
            if let Some(active) = peers.get_mut(&peer) {
                active.clipboard = None;
            }
            clipboard_state
                .applications
                .queued
                .retain(|(queued_peer, _)| queued_peer != &peer);
            tracing::warn!(%peer, %reason, "clipboard transfer worker stopped");
            send_event(
                events,
                RuntimeEvent::Error {
                    message: format!("Clipboard synchronization with `{peer}` stopped: {reason}"),
                },
            );
        }
        NetworkEvent::Message { .. } => {}
        NetworkEvent::ClipboardTransfer { .. } | NetworkEvent::ClipboardFailed { .. } => {}
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn disconnect_controller_peer(
    peer: NodeId,
    session_id: u128,
    reason: String,
    controller: &mut ControllerSession,
    capture: &CaptureService,
    peers: &mut BTreeMap<NodeId, ActivePeer>,
    events: &SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    if !active_session(peers, &peer, session_id) {
        return Ok(());
    }
    peers.remove(&peer);
    send_event(
        events,
        RuntimeEvent::Disconnected {
            peer: peer.clone(),
            reason,
        },
    );
    if controller.focus() == &peer {
        let actions = controller
            .return_to_local()
            .map_err(|error| RuntimeError::Session(error.to_string()))?;
        apply_controller_actions(actions, capture, peers, events)?;
        send_event(
            events,
            RuntimeEvent::FocusChanged {
                node: controller.focus().clone(),
            },
        );
    }
    Ok(())
}

fn active_session(peers: &BTreeMap<NodeId, ActivePeer>, peer: &NodeId, session_id: u128) -> bool {
    peers
        .get(peer)
        .is_some_and(|active| active.session_id == session_id)
}

#[allow(clippy::too_many_arguments)]
async fn run_agent(
    local_node: NodeId,
    controller_node: NodeId,
    controller_address: SocketAddr,
    display_layout: DesktopLayout,
    identity: LocalIdentity,
    trust: TrustStore,
    injection: InjectionService,
    clipboard: Option<ClipboardService>,
    mut commands: tokio_mpsc::Receiver<RuntimeCommand>,
    events: SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    send_event(
        &events,
        RuntimeEvent::Starting {
            role: RuntimeRole::Agent,
        },
    );
    let bind_address = match controller_address.ip() {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let client = SecureClient::bind(
        bind_address,
        identity,
        &trust,
        session_profile(clipboard.is_some()),
        SessionLimits::default(),
    )
    .map_err(|error| RuntimeError::Transport(error.to_string()))?;
    if injection.is_ready() {
        send_event(
            &events,
            RuntimeEvent::NativeReady {
                backend: injection.backend(),
            },
        );
    }
    let reconnect = ReconnectPolicy::default();
    let mut attempt = 0u32;
    let mut display_geometry = DesktopGeometry {
        origin: domain::Point { x: 0, y: 0 },
        layout: display_layout,
    };

    loop {
        send_event(
            &events,
            RuntimeEvent::Connecting {
                peer: controller_node.clone(),
                address: controller_address,
                attempt: attempt.saturating_add(1),
            },
        );
        let connection = tokio::select! {
            command = commands.recv() => {
                if command.is_none() || matches!(command, Some(RuntimeCommand::Stop)) {
                    break;
                }
                continue;
            }
            connected = client.connect(&controller_node, controller_address) => connected,
        };
        match connection {
            Ok(connection) => {
                attempt = 0;
                let info = connection.info().clone();
                send_event(
                    &events,
                    RuntimeEvent::Connected {
                        peer: info.peer.clone(),
                        session_id: info.session_id,
                    },
                );
                let outcome = run_agent_connection(
                    &local_node,
                    &controller_node,
                    &mut display_geometry,
                    AgentNativeServices {
                        injection: &injection,
                        clipboard: clipboard.as_ref(),
                    },
                    connection,
                    &mut commands,
                    &events,
                )
                .await?;
                if outcome == AgentConnectionOutcome::Stopped {
                    break;
                }
                send_event(
                    &events,
                    RuntimeEvent::Disconnected {
                        peer: controller_node.clone(),
                        reason: String::from("Connection lost"),
                    },
                );
            }
            Err(error) => {
                send_event(
                    &events,
                    RuntimeEvent::Disconnected {
                        peer: controller_node.clone(),
                        reason: error.to_string(),
                    },
                );
            }
        }

        let Some(delay) = reconnect.delay_before(attempt) else {
            return Err(RuntimeError::ReconnectExhausted(controller_node));
        };
        attempt = attempt.saturating_add(1);
        tokio::select! {
            command = commands.recv() => {
                if command.is_none() || matches!(command, Some(RuntimeCommand::Stop)) {
                    break;
                }
            }
            () = tokio::time::sleep(delay) => {}
        }
    }

    Ok(())
}

async fn run_agent_connection(
    local_node: &NodeId,
    controller_node: &NodeId,
    display_geometry: &mut DesktopGeometry,
    native: AgentNativeServices<'_>,
    connection: PeerConnection,
    commands: &mut tokio_mpsc::Receiver<RuntimeCommand>,
    events: &SyncSender<RuntimeEvent>,
) -> Result<AgentConnectionOutcome, RuntimeError> {
    let AgentNativeServices {
        injection,
        clipboard,
    } = native;
    while let Ok(event) = injection.try_recv() {
        match event {
            InjectionServiceEvent::Ready { backend } => {
                send_event(events, RuntimeEvent::NativeReady { backend });
            }
            InjectionServiceEvent::DesktopChanged(geometry) => {
                *display_geometry = geometry;
            }
            InjectionServiceEvent::Failed { operation, reason } => {
                return Err(RuntimeError::Native(format!("{operation}: {reason}")));
            }
            InjectionServiceEvent::Stopped => {
                return Err(RuntimeError::Native(String::from(
                    "Input injection service stopped",
                )));
            }
            InjectionServiceEvent::Applied { .. } | InjectionServiceEvent::Released { .. } => {}
        }
    }
    let injection_generation = injection
        .begin_session()
        .map_err(|error| RuntimeError::Native(error.to_string()))?;
    let connection_info = connection.info().clone();
    let (clipboard_event_tx, mut clipboard_events) = tokio_mpsc::channel(NETWORK_EVENT_CAPACITY);
    let mut active_clipboard = (clipboard.is_some()
        && connection_info.negotiated_capabilities.clipboard_text)
        .then(|| AgentClipboard {
            session: ClipboardSession::new(local_node.clone(), controller_node.clone()),
            outbound: spawn_clipboard_worker(
                connection.clipboard_endpoint(),
                controller_node.clone(),
                connection_info.session_id,
                clipboard_event_tx,
            ),
        });
    let mut clipboard_applications = ClipboardApplications::new();
    let (mut sender, receiver) = connection.split_control();
    let (mut inbound, reader_worker) = spawn_control_reader(receiver);
    let mut agent = AgentSession::new(local_node.clone(), display_geometry.size());
    let mut input_tick = tokio::time::interval(INPUT_POLL_INTERVAL);
    input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut controlled = false;
    let mut application_pending = false;

    let outcome = async {
        sender
            .send(Session::DisplayChanged {
                layout: display_geometry.layout.clone(),
            })
            .await
            .map_err(|error| RuntimeError::Transport(error.to_string()))?;
        tracing::info!(
            width = display_geometry.size().width.get(),
            height = display_geometry.size().height.get(),
            monitors = display_geometry.monitor_count(),
            "agent display reported"
        );
        send_event(
            events,
            RuntimeEvent::LocalDesktopChanged {
                geometry: display_geometry.clone(),
            },
        );
        'session: loop {
            tokio::select! {
            command = commands.recv() => {
                if command.is_none() || matches!(command, Some(RuntimeCommand::Stop)) {
                    let _ = sender.send(Session::Disconnect).await;
                    let _ = injection.release_all(injection_generation);
                    break Ok(AgentConnectionOutcome::Stopped);
                }
            }
            inbound_event = inbound.recv(), if !application_pending => {
                let message = match inbound_event {
                    Some(ControlReadEvent::Message(message)) => message,
                    Some(ControlReadEvent::Disconnected(reason)) => {
                        tracing::warn!(%reason, "agent control connection ended");
                        let _ = injection.release_all(injection_generation);
                        break Ok(AgentConnectionOutcome::Disconnected);
                    }
                    None => {
                        let _ = injection.release_all(injection_generation);
                        break Ok(AgentConnectionOutcome::Disconnected);
                    }
                };
                if let Session::Clipboard(control) = message {
                    let acknowledged =
                        matches!(&control, protocol::ClipboardControl::Applied { .. });
                    let actions = active_clipboard
                        .as_mut()
                        .ok_or_else(|| RuntimeError::Session(String::from(
                            "controller sent clipboard control without negotiating it",
                        )))?
                        .session
                        .receive_control(control)
                        .map_err(|error| RuntimeError::Session(error.to_string()))?;
                    apply_agent_clipboard_actions(
                        actions,
                        active_clipboard.as_ref(),
                        clipboard,
                        &mut clipboard_applications,
                        &mut sender,
                    )
                    .await?;
                    if acknowledged {
                        send_event(events, RuntimeEvent::ClipboardSynchronized {
                            peer: controller_node.clone(),
                            received: false,
                        });
                    }
                    continue 'session;
                }
                let actions = agent
                    .handle(message)
                    .map_err(|error| RuntimeError::Session(error.to_string()))?;
                application_pending = actions
                    .iter()
                    .any(|action| matches!(action, AgentAction::ApplyInput(_)));
                apply_agent_actions(
                    actions,
                    injection,
                    injection_generation,
                    &mut sender,
                )
                .await?;
                let next_controlled = agent.active_focus_epoch().is_some();
                if next_controlled != controlled {
                    controlled = next_controlled;
                    send_event(events, RuntimeEvent::AgentControl {
                        controller: controller_node.clone(),
                        active: controlled,
                    });
                }
            }
            _ = input_tick.tick() => {
                drain_agent_clipboard(
                    controller_node,
                    clipboard,
                    &mut active_clipboard,
                    &mut clipboard_applications,
                    &mut sender,
                    events,
                )
                .await?;
                while let Ok(event) = clipboard_events.try_recv() {
                    match event {
                        NetworkEvent::ClipboardTransfer {
                            peer,
                            session_id,
                            transfer,
                        } if peer == *controller_node
                            && session_id == connection_info.session_id =>
                        {
                            let actions = active_clipboard
                                .as_mut()
                                .ok_or_else(|| RuntimeError::Session(String::from(
                                    "controller sent clipboard data without negotiating it",
                                )))?
                                .session
                                .receive_transfer(transfer)
                                .map_err(|error| RuntimeError::Session(error.to_string()))?;
                            apply_agent_clipboard_actions(
                                actions,
                                active_clipboard.as_ref(),
                                clipboard,
                                &mut clipboard_applications,
                                &mut sender,
                            )
                            .await?;
                        }
                        NetworkEvent::ClipboardFailed {
                            peer,
                            session_id,
                            reason,
                        } if peer == *controller_node
                            && session_id == connection_info.session_id =>
                        {
                            active_clipboard = None;
                            send_event(events, RuntimeEvent::Error {
                                message: format!(
                                    "Clipboard synchronization with `{peer}` stopped: {reason}"
                                ),
                            });
                        }
                        NetworkEvent::Message { .. }
                        | NetworkEvent::Disconnected { .. }
                        | NetworkEvent::ClipboardTransfer { .. }
                        | NetworkEvent::ClipboardFailed { .. } => {}
                    }
                }
                while let Ok(event) = injection.try_recv() {
                    match event {
                        InjectionServiceEvent::Ready { backend } => {
                            send_event(events, RuntimeEvent::NativeReady { backend });
                        }
                        InjectionServiceEvent::DesktopChanged(geometry) => {
                            if geometry != *display_geometry {
                                *display_geometry = geometry.clone();
                                application_pending = false;
                                let actions = agent.reconcile_display(geometry.size());
                                apply_agent_actions(
                                    actions,
                                    injection,
                                    injection_generation,
                                    &mut sender,
                                )
                                .await?;
                                sender
                                    .send(Session::DisplayChanged {
                                        layout: geometry.layout.clone(),
                                    })
                                    .await
                                    .map_err(|error| {
                                        RuntimeError::Transport(error.to_string())
                                    })?;
                                if controlled {
                                    controlled = false;
                                    send_event(events, RuntimeEvent::AgentControl {
                                        controller: controller_node.clone(),
                                        active: false,
                                    });
                                }
                                send_event(
                                    events,
                                    RuntimeEvent::LocalDesktopChanged {
                                        geometry: geometry.clone(),
                                    },
                                );
                                tracing::info!(
                                    width = geometry.size().width.get(),
                                    height = geometry.size().height.get(),
                                    monitors = geometry.monitor_count(),
                                    "agent display change reported"
                                );
                            }
                        }
                        InjectionServiceEvent::Applied {
                            generation,
                            sequence,
                        } => {
                            if generation == injection_generation {
                                application_pending = false;
                                let actions = agent
                                    .confirm_applied(sequence)
                                    .map_err(|error| RuntimeError::Session(error.to_string()))?;
                                apply_agent_actions(
                                    actions,
                                    injection,
                                    injection_generation,
                                    &mut sender,
                                )
                                .await?;
                            } else {
                                tracing::debug!(
                                    generation,
                                    active_generation = injection_generation,
                                    "ignored stale native input event"
                                );
                            }
                        }
                        InjectionServiceEvent::Released { generation } => {
                            if generation != injection_generation {
                                tracing::debug!(
                                    generation,
                                    active_generation = injection_generation,
                                    "ignored stale native input event"
                                );
                            }
                        }
                        InjectionServiceEvent::Failed { operation, reason } => {
                            break 'session Err(RuntimeError::Native(format!(
                                "{operation}: {reason}"
                            )));
                        }
                        InjectionServiceEvent::Stopped => {
                            break 'session Err(RuntimeError::Native(String::from(
                                "Input injection service stopped",
                            )));
                        }
                    }
                }
            }
            }
        }
    }
    .await;
    sender.close();
    reader_worker.abort();
    outcome
}

async fn apply_agent_actions(
    actions: Vec<AgentAction>,
    injection: &InjectionService,
    injection_generation: u64,
    sender: &mut ControlSender,
) -> Result<(), RuntimeError> {
    for action in actions {
        match action {
            AgentAction::FocusEntered { position } => injection
                .warp_cursor(injection_generation, position)
                .map_err(|error| RuntimeError::Native(error.to_string()))?,
            AgentAction::ApplyInput(batch) => injection
                .apply_batch(injection_generation, batch.sequence, batch.events)
                .map_err(|error| RuntimeError::Native(error.to_string()))?,
            AgentAction::ReleaseAllInput => injection
                .release_all(injection_generation)
                .map_err(|error| RuntimeError::Native(error.to_string()))?,
            AgentAction::Send(message) => {
                sender
                    .send(message)
                    .await
                    .map_err(|error| RuntimeError::Transport(error.to_string()))?;
            }
            AgentAction::Clipboard(_) => {}
            AgentAction::CloseConnection => sender.close(),
        }
    }
    Ok(())
}

async fn drain_agent_clipboard(
    controller_node: &NodeId,
    service: Option<&ClipboardService>,
    clipboard: &mut Option<AgentClipboard>,
    applications: &mut ClipboardApplications,
    sender: &mut ControlSender,
    events: &SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    let native_events = service.map_or_else(Vec::new, |service| {
        std::iter::from_fn(|| service.try_recv().ok()).collect()
    });
    for event in native_events {
        match event {
            ClipboardServiceEvent::Ready { backend } => {
                send_event(events, RuntimeEvent::ClipboardReady { backend });
            }
            ClipboardServiceEvent::Changed { text } => {
                let Some(active) = clipboard.as_mut() else {
                    continue;
                };
                let actions = active
                    .session
                    .local_changed(text)
                    .map_err(|error| RuntimeError::Session(error.to_string()))?;
                apply_agent_clipboard_actions(
                    actions,
                    clipboard.as_ref(),
                    service,
                    applications,
                    sender,
                )
                .await?;
            }
            ClipboardServiceEvent::Applied => {
                let Some((peer, generation)) = applications.pending.take() else {
                    return Err(RuntimeError::Session(String::from(
                        "native clipboard applied without a pending generation",
                    )));
                };
                if peer != *controller_node {
                    return Err(RuntimeError::Session(format!(
                        "native clipboard application belongs to unexpected peer `{peer}`"
                    )));
                }
                let Some(active) = clipboard.as_mut() else {
                    continue;
                };
                let actions = active
                    .session
                    .confirm_applied(&generation)
                    .map_err(|error| RuntimeError::Session(error.to_string()))?;
                apply_agent_clipboard_actions(
                    actions,
                    clipboard.as_ref(),
                    service,
                    applications,
                    sender,
                )
                .await?;
                send_event(
                    events,
                    RuntimeEvent::ClipboardSynchronized {
                        peer,
                        received: true,
                    },
                );
            }
            ClipboardServiceEvent::Failed { operation, reason } => {
                tracing::warn!(operation, %reason, "clipboard service failed");
                send_event(
                    events,
                    RuntimeEvent::Error {
                        message: format!("Clipboard synchronization stopped: {reason}"),
                    },
                );
                *clipboard = None;
                *applications = ClipboardApplications::new();
            }
            ClipboardServiceEvent::Stopped => {
                *clipboard = None;
                *applications = ClipboardApplications::new();
                tracing::warn!("native clipboard service stopped");
            }
        }
    }
    start_next_clipboard_application(service, applications)
}

async fn apply_agent_clipboard_actions(
    actions: Vec<ClipboardAction>,
    clipboard: Option<&AgentClipboard>,
    service: Option<&ClipboardService>,
    applications: &mut ClipboardApplications,
    sender: &mut ControlSender,
) -> Result<(), RuntimeError> {
    for action in actions {
        match action {
            ClipboardAction::SendControl(control) => sender
                .send(Session::Clipboard(control))
                .await
                .map_err(|error| RuntimeError::Transport(error.to_string()))?,
            ClipboardAction::SendTransfer(transfer) => clipboard
                .ok_or_else(|| {
                    RuntimeError::Session(String::from("clipboard transfer channel is unavailable"))
                })?
                .outbound
                .try_send(transfer)
                .map_err(|_| {
                    RuntimeError::Session(String::from("clipboard transfer queue is full"))
                })?,
            ClipboardAction::ApplyRemote(transfer) => {
                let peer = transfer.generation().owner.clone();
                applications.queued.push_back((peer, transfer));
            }
        }
    }
    start_next_clipboard_application(service, applications)
}

fn spawn_connection_worker(
    connection: PeerConnection,
    events: tokio_mpsc::Sender<NetworkEvent>,
) -> tokio_mpsc::Sender<Session> {
    let peer = connection.info().peer.clone();
    let session_id = connection.info().session_id;
    let (mut sender, receiver) = connection.split_control();
    let (mut inbound, reader_worker) = spawn_control_reader(receiver);
    let (outbound, mut outbound_rx) = tokio_mpsc::channel(OUTBOUND_CAPACITY);
    tokio::spawn(async move {
        let reason = loop {
            tokio::select! {
                message = outbound_rx.recv() => {
                    let Some(message) = message else {
                        break String::from("Connection replaced");
                    };
                    if let Err(error) = sender.send(message).await {
                        break error.to_string();
                    }
                }
                inbound_event = inbound.recv() => {
                    match inbound_event {
                        Some(ControlReadEvent::Message(message)) => {
                            if events.send(NetworkEvent::Message {
                                peer: peer.clone(),
                                session_id,
                                message,
                            }).await.is_err() {
                                break String::from("Session runtime stopped");
                            }
                        }
                        Some(ControlReadEvent::Disconnected(reason)) => break reason,
                        None => break String::from("Control reader stopped"),
                    }
                }
            }
        };
        sender.close();
        reader_worker.abort();
        let _ = events
            .send(NetworkEvent::Disconnected {
                peer,
                session_id,
                reason,
            })
            .await;
    });
    outbound
}

fn spawn_clipboard_worker(
    endpoint: ClipboardEndpoint,
    peer: NodeId,
    session_id: u128,
    events: tokio_mpsc::Sender<NetworkEvent>,
) -> tokio_mpsc::Sender<ClipboardText> {
    let (outbound, mut outbound_rx) = tokio_mpsc::channel(CLIPBOARD_OUTBOUND_CAPACITY);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                transfer = outbound_rx.recv() => {
                    let Some(transfer) = transfer else {
                        break;
                    };
                    let result = async {
                        let mut stream = endpoint.open().await?;
                        stream.send(&transfer).await?;
                        stream.finish()
                    }
                    .await;
                    if let Err(error) = result {
                        let _ = events.send(NetworkEvent::ClipboardFailed {
                            peer: peer.clone(),
                            session_id,
                            reason: error.to_string(),
                        }).await;
                        break;
                    }
                }
                stream = endpoint.accept() => {
                    let result = match stream {
                        Ok(mut stream) => stream.receive().await,
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(transfer) => {
                            if events.send(NetworkEvent::ClipboardTransfer {
                                peer: peer.clone(),
                                session_id,
                                transfer,
                            }).await.is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = events.send(NetworkEvent::ClipboardFailed {
                                peer: peer.clone(),
                                session_id,
                                reason: error.to_string(),
                            }).await;
                            break;
                        }
                    }
                }
            }
        }
    });
    outbound
}

fn spawn_control_reader(
    mut receiver: ControlReceiver,
) -> (
    tokio_mpsc::Receiver<ControlReadEvent>,
    tokio::task::JoinHandle<()>,
) {
    let (events_tx, events) = tokio_mpsc::channel(NETWORK_EVENT_CAPACITY);
    let worker = tokio::spawn(async move {
        loop {
            match receiver.receive().await {
                Ok(message) => {
                    if events_tx
                        .send(ControlReadEvent::Message(message))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = events_tx
                        .send(ControlReadEvent::Disconnected(error.to_string()))
                        .await;
                    break;
                }
            }
        }
    });
    (events, worker)
}

fn capture_edges(
    topology: &Topology,
    local_node: &NodeId,
    edge_behavior: EdgeBehavior,
) -> Vec<Edge> {
    [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
        .into_iter()
        .filter(|edge| {
            edge_behavior.active_interval(*edge).is_some()
                && activation_target(topology, local_node, *edge, None).is_some()
        })
        .collect()
}

fn resize_topology_screen(
    topology: &Topology,
    node: &NodeId,
    layout: &DesktopLayout,
) -> Result<Topology, String> {
    let current = topology
        .screen(node)
        .ok_or_else(|| format!("node `{node}` is not present in the topology"))?;
    if current.layout == *layout {
        return Ok(topology.clone());
    }
    let size = layout.size();

    let old_width = i64::from(current.bounds.size.width.get());
    let old_height = i64::from(current.bounds.size.height.get());
    let new_width = i64::from(size.width.get());
    let new_height = i64::from(size.height.get());
    let mut x = current.bounds.left() + (old_width - new_width) / 2;
    let mut y = current.bounds.top() + (old_height - new_height) / 2;

    for neighbor in topology
        .screens()
        .iter()
        .filter(|screen| &screen.node != node)
    {
        if current.bounds.left() == neighbor.bounds.right() {
            x = current.bounds.left();
        } else if current.bounds.right() == neighbor.bounds.left() {
            x = neighbor.bounds.left() - new_width;
        }
        if current.bounds.top() == neighbor.bounds.bottom() {
            y = current.bounds.top();
        } else if current.bounds.bottom() == neighbor.bounds.top() {
            y = neighbor.bounds.top() - new_height;
        }
    }

    let origin = Point {
        x: i32::try_from(x)
            .map_err(|_| format!("resized desktop for `{node}` exceeds the coordinate range"))?,
        y: i32::try_from(y)
            .map_err(|_| format!("resized desktop for `{node}` exceeds the coordinate range"))?,
    };
    let screens = topology
        .screens()
        .iter()
        .cloned()
        .map(|mut screen| {
            if &screen.node == node {
                screen.bounds = Rect::new(origin, size);
                screen.layout = layout.clone();
            }
            screen
        })
        .collect();
    Topology::new(screens).map_err(|error| error.to_string())
}

fn activation_target(
    topology: &Topology,
    local_node: &NodeId,
    edge: Edge,
    edge_position: Option<f64>,
) -> Option<(NodeId, u32)> {
    let source = topology.screen(local_node)?;
    if let Some(edge_position) = edge_position {
        let length = match edge {
            Edge::Left | Edge::Right => source.bounds.size.height.get(),
            Edge::Top | Edge::Bottom => source.bounds.size.width.get(),
        };
        let maximum = length.saturating_sub(1);
        let offset = (edge_position.clamp(0.0, 1.0) * f64::from(maximum)).round() as u32;
        let transition = topology.transition(local_node, edge, offset)?;
        return Some((transition.target.clone(), offset));
    }
    topology.screens().iter().find_map(|candidate| {
        if candidate.node == source.node {
            return None;
        }
        shared_edge_offset(source, candidate, edge).map(|offset| (candidate.node.clone(), offset))
    })
}

fn shared_edge_offset(
    source: &ScreenPlacement,
    candidate: &ScreenPlacement,
    edge: Edge,
) -> Option<u32> {
    let (touches, opposite, source_start, candidate_start) = match edge {
        Edge::Left => (
            candidate.bounds.right() == source.bounds.left(),
            Edge::Right,
            source.bounds.top(),
            candidate.bounds.top(),
        ),
        Edge::Right => (
            candidate.bounds.left() == source.bounds.right(),
            Edge::Left,
            source.bounds.top(),
            candidate.bounds.top(),
        ),
        Edge::Top => (
            candidate.bounds.bottom() == source.bounds.top(),
            Edge::Bottom,
            source.bounds.left(),
            candidate.bounds.left(),
        ),
        Edge::Bottom => (
            candidate.bounds.top() == source.bounds.bottom(),
            Edge::Top,
            source.bounds.left(),
            candidate.bounds.left(),
        ),
    };
    if !touches {
        return None;
    }
    source.layout.edge_segments(edge).into_iter().find_map(
        |(source_segment_start, source_segment_end)| {
            let source_segment_start = source_start + i64::from(source_segment_start);
            let source_segment_end = source_start + i64::from(source_segment_end);
            candidate
                .layout
                .edge_segments(opposite)
                .into_iter()
                .find_map(|(candidate_segment_start, candidate_segment_end)| {
                    let overlap_start = source_segment_start
                        .max(candidate_start + i64::from(candidate_segment_start));
                    let overlap_end =
                        source_segment_end.min(candidate_start + i64::from(candidate_segment_end));
                    if overlap_start >= overlap_end {
                        return None;
                    }
                    let midpoint = overlap_start + (overlap_end - overlap_start - 1) / 2;
                    u32::try_from(midpoint - source_start).ok()
                })
        },
    )
}

fn session_profile(clipboard_text: bool) -> SessionProfile {
    SessionProfile {
        platform: platform::probe_host().platform,
        capabilities: Capabilities {
            keyboard: true,
            relative_pointer: true,
            absolute_pointer: false,
            clipboard_text,
        },
    }
}

fn send_event(events: &SyncSender<RuntimeEvent>, event: RuntimeEvent) {
    let _ = events.send(event);
}

struct ActivePeer {
    session_id: u128,
    outbound: tokio_mpsc::Sender<Session>,
    clipboard: Option<ActiveClipboard>,
}

struct ActiveClipboard {
    session: ClipboardSession,
    outbound: tokio_mpsc::Sender<ClipboardText>,
}

struct AgentClipboard {
    session: ClipboardSession,
    outbound: tokio_mpsc::Sender<ClipboardText>,
}

struct AgentNativeServices<'a> {
    injection: &'a InjectionService,
    clipboard: Option<&'a ClipboardService>,
}

struct ClipboardRuntimeState {
    service: Option<ClipboardService>,
    applications: ClipboardApplications,
}

impl ClipboardRuntimeState {
    const fn new(service: Option<ClipboardService>) -> Self {
        Self {
            service,
            applications: ClipboardApplications::new(),
        }
    }
}

struct ClipboardApplications {
    pending: Option<(NodeId, ClipboardGeneration)>,
    queued: VecDeque<(NodeId, ClipboardText)>,
}

impl ClipboardApplications {
    const fn new() -> Self {
        Self {
            pending: None,
            queued: VecDeque::new(),
        }
    }
}

struct PendingActivation {
    edge: Edge,
    offset: u32,
    target: NodeId,
    ready_at: Instant,
}

struct EdgeState {
    behavior: EdgeBehavior,
    pending_activation: Option<PendingActivation>,
    emergency_shortcut: EmergencyShortcut,
}

#[derive(Default)]
struct EmergencyShortcut {
    control: bool,
    alt: bool,
}

impl EmergencyShortcut {
    fn observe(&mut self, event: InputEvent) -> bool {
        let InputKind::Key { key, action } = event.kind else {
            return false;
        };
        let pressed = !matches!(action, KeyAction::Release);
        match key {
            LEFT_CONTROL | RIGHT_CONTROL => self.control = pressed,
            LEFT_ALT | RIGHT_ALT => self.alt = pressed,
            ESCAPE if matches!(action, KeyAction::Press) => return self.control && self.alt,
            _ => {}
        }
        false
    }
}

const LEFT_CONTROL: PhysicalKey = PhysicalKey::new(0x07, 0xe0);
const RIGHT_CONTROL: PhysicalKey = PhysicalKey::new(0x07, 0xe4);
const LEFT_ALT: PhysicalKey = PhysicalKey::new(0x07, 0xe2);
const RIGHT_ALT: PhysicalKey = PhysicalKey::new(0x07, 0xe6);
const ESCAPE: PhysicalKey = PhysicalKey::new(0x07, 0x29);

enum ControlReadEvent {
    Message(Session),
    Disconnected(String),
}

enum NetworkEvent {
    Message {
        peer: NodeId,
        session_id: u128,
        message: Session,
    },
    Disconnected {
        peer: NodeId,
        session_id: u128,
        reason: String,
    },
    ClipboardTransfer {
        peer: NodeId,
        session_id: u128,
        transfer: ClipboardText,
    },
    ClipboardFailed {
        peer: NodeId,
        session_id: u128,
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentConnectionOutcome {
    Stopped,
    Disconnected,
}

#[derive(Debug, Error)]
pub enum RuntimeStartError {
    #[error("trusted node `{0}` must be paired before starting the session")]
    UntrustedPeer(NodeId),
    #[error("controller topology must contain at least one remote screen")]
    NoRemoteScreens,
    #[error("prepared native input role does not match the configuration")]
    NativeRoleMismatch,
    #[error("could not start session worker: {0}")]
    Spawn(std::io::Error),
}

const fn runtime_role(role: &Role) -> RuntimeRole {
    match role {
        Role::Controller { .. } => RuntimeRole::Controller,
        Role::Agent { .. } => RuntimeRole::Agent,
    }
}

#[derive(Debug, Error)]
enum RuntimeError {
    #[error("transport failed: {0}")]
    Transport(String),
    #[error("native input failed: {0}")]
    Native(String),
    #[error("session failed: {0}")]
    Session(String),
    #[error("outbound queue for `{0}` is unavailable")]
    OutboundQueue(NodeId),
    #[error("reconnect attempts to `{0}` were exhausted")]
    ReconnectExhausted(NodeId),
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, SocketAddr},
        num::NonZeroU32,
        time::Duration,
    };

    use domain::{
        DesktopLayout, Edge, InputEvent, InputKind, KeyAction, NodeId, PhysicalKey, Point, Rect,
        ScreenPlacement, Size, Topology,
    };
    use identity::{IdentityStore, LocalIdentity, TrustStore};
    use protocol::Session;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use transport::{SecureClient, SecureServer, SessionLimits};

    use super::{
        EmergencyShortcut, NETWORK_EVENT_CAPACITY, NetworkEvent, activation_target, capture_edges,
        resize_topology_screen, session_profile, spawn_accept_worker, spawn_connection_worker,
    };

    fn key_event(usage_id: u16, action: KeyAction) -> InputEvent {
        InputEvent {
            elapsed_micros: 0,
            kind: InputKind::Key {
                key: PhysicalKey::new(0x07, usage_id),
                action,
            },
        }
    }

    fn node(value: &str) -> NodeId {
        NodeId::new(value).unwrap_or_else(|error| panic!("invalid test node: {error}"))
    }

    fn screen(node_id: &str, x: i32, y: i32, width: u32, height: u32) -> ScreenPlacement {
        ScreenPlacement::single(
            node(node_id),
            Rect::new(
                Point { x, y },
                Size::new(
                    NonZeroU32::new(width).unwrap_or(NonZeroU32::MIN),
                    NonZeroU32::new(height).unwrap_or(NonZeroU32::MIN),
                ),
            ),
        )
    }

    fn identity(directory: &TempDir, node_id: &str) -> LocalIdentity {
        IdentityStore::new(directory.path())
            .load_or_create(&node(node_id))
            .unwrap_or_else(|error| panic!("identity creation failed: {error}"))
    }

    fn trust(directory: &TempDir, remote: &LocalIdentity) -> TrustStore {
        let mut trust = IdentityStore::new(directory.path())
            .trust_store()
            .unwrap_or_else(|error| panic!("trust store creation failed: {error}"));
        let bundle = remote.pairing_bundle();
        let code = bundle.code().to_string();
        trust
            .trust(bundle, &code)
            .unwrap_or_else(|error| panic!("pairing failed: {error}"));
        trust
    }

    #[test]
    fn capture_edges_and_offsets_follow_the_local_topology() {
        let topology = Topology::new(vec![
            screen("local", 0, 180, 1920, 1080),
            screen("right", 1920, 0, 2560, 1440),
        ])
        .unwrap_or_else(|error| panic!("topology should be valid: {error}"));

        assert_eq!(
            capture_edges(
                &topology,
                &node("local"),
                crate::config::EdgeBehavior::default()
            ),
            vec![Edge::Right]
        );
        assert_eq!(
            activation_target(&topology, &node("local"), Edge::Right, None),
            Some((node("right"), 539))
        );
        assert_eq!(
            activation_target(&topology, &node("local"), Edge::Right, Some(0.05)),
            Some((node("right"), 54))
        );
    }

    #[test]
    fn capture_edges_include_partial_overlap_away_from_the_midpoint() {
        let topology = Topology::new(vec![
            screen("local", 0, 0, 1920, 1080),
            screen("right", 1920, -900, 2560, 1080),
        ])
        .unwrap_or_else(|error| panic!("topology should be valid: {error}"));

        assert_eq!(
            capture_edges(
                &topology,
                &node("local"),
                crate::config::EdgeBehavior::default()
            ),
            vec![Edge::Right]
        );
        assert_eq!(
            activation_target(&topology, &node("local"), Edge::Right, None),
            Some((node("right"), 89))
        );
    }

    #[test]
    fn disabled_edges_do_not_install_capture_barriers() {
        let topology = Topology::new(vec![
            screen("local", 0, 0, 1920, 1080),
            screen("right", 1920, 0, 1920, 1080),
        ])
        .unwrap_or_else(|error| panic!("topology should be valid: {error}"));
        let mut behavior = crate::config::EdgeBehavior::default();
        behavior.right.enabled = false;

        assert!(capture_edges(&topology, &node("local"), behavior).is_empty());
    }

    #[test]
    fn emergency_shortcut_requires_control_alt_and_escape() {
        let mut shortcut = EmergencyShortcut::default();

        assert!(!shortcut.observe(key_event(0xe0, KeyAction::Press)));
        assert!(!shortcut.observe(key_event(0xe2, KeyAction::Press)));
        assert!(shortcut.observe(key_event(0x29, KeyAction::Press)));

        assert!(!shortcut.observe(key_event(0xe2, KeyAction::Release)));
        assert!(!shortcut.observe(key_event(0x29, KeyAction::Press)));
    }

    #[test]
    fn reported_display_size_preserves_a_right_hand_attachment() {
        let topology = Topology::new(vec![
            screen("local", 0, 0, 1920, 1080),
            screen("right", 1920, 0, 1920, 1080),
        ])
        .unwrap_or_else(|error| panic!("topology should be valid: {error}"));

        let updated = resize_topology_screen(
            &topology,
            &node("right"),
            &DesktopLayout::single(Size::new(
                NonZeroU32::new(2560).unwrap_or(NonZeroU32::MIN),
                NonZeroU32::new(1440).unwrap_or(NonZeroU32::MIN),
            )),
        )
        .unwrap_or_else(|error| panic!("display update should be valid: {error}"));

        assert_eq!(
            updated.screen(&node("right")),
            Some(&screen("right", 1920, -180, 2560, 1440))
        );
    }

    #[test]
    fn reported_display_size_preserves_a_left_hand_attachment() {
        let topology = Topology::new(vec![
            screen("left", 0, 0, 1920, 1080),
            screen("local", 1920, 0, 1920, 1080),
        ])
        .unwrap_or_else(|error| panic!("topology should be valid: {error}"));

        let updated = resize_topology_screen(
            &topology,
            &node("left"),
            &DesktopLayout::single(Size::new(
                NonZeroU32::new(2560).unwrap_or(NonZeroU32::MIN),
                NonZeroU32::new(1440).unwrap_or(NonZeroU32::MIN),
            )),
        )
        .unwrap_or_else(|error| panic!("display update should be valid: {error}"));

        assert_eq!(
            updated.screen(&node("left")),
            Some(&screen("left", -640, -180, 2560, 1440))
        );
    }

    #[tokio::test]
    async fn connection_workers_preserve_bidirectional_control_traffic() {
        let controller_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let agent_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let controller = identity(&controller_directory, "controller");
        let agent = identity(&agent_directory, "agent");
        let controller_trust = trust(&controller_directory, &agent);
        let agent_trust = trust(&agent_directory, &controller);
        let server = SecureServer::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            controller,
            &controller_trust,
            session_profile(true),
            SessionLimits::default(),
        )
        .unwrap_or_else(|error| panic!("server bind failed: {error}"));
        let address = server
            .local_addr()
            .unwrap_or_else(|error| panic!("server address failed: {error}"));
        let (mut accepted, worker) = spawn_accept_worker(server);
        let client = SecureClient::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            agent,
            &agent_trust,
            session_profile(true),
            SessionLimits::default(),
        )
        .unwrap_or_else(|error| panic!("client bind failed: {error}"));

        let client_connection = client
            .connect(&node("controller"), address)
            .await
            .unwrap_or_else(|error| panic!("client handshake failed: {error}"));
        let server_connection = tokio::time::timeout(Duration::from_secs(1), accepted.recv())
            .await
            .unwrap_or_else(|_| panic!("accept worker timed out"))
            .unwrap_or_else(|| panic!("accept worker stopped"))
            .unwrap_or_else(|error| panic!("server handshake failed: {error}"));

        assert_eq!(client_connection.info().peer, node("controller"));
        assert_eq!(server_connection.info().peer, node("agent"));

        let (network_tx, mut network_rx) = mpsc::channel(NETWORK_EVENT_CAPACITY);
        let outbound = spawn_connection_worker(server_connection, network_tx);
        let (mut agent_sender, mut agent_receiver) = client_connection.split_control();
        for nonce in 0..64 {
            outbound
                .send(Session::Heartbeat { nonce })
                .await
                .unwrap_or_else(|error| panic!("outbound queue failed: {error}"));
        }
        for expected in 0..64 {
            let message = tokio::time::timeout(Duration::from_secs(1), agent_receiver.receive())
                .await
                .unwrap_or_else(|_| panic!("agent receive timed out"))
                .unwrap_or_else(|error| panic!("agent receive failed: {error}"));
            assert!(matches!(
                message,
                Session::Heartbeat { nonce } if nonce == expected
            ));
            agent_sender
                .send(Session::HeartbeatAcknowledged { nonce: expected })
                .await
                .unwrap_or_else(|error| panic!("acknowledgement failed: {error}"));
        }
        for expected in 0..64 {
            let event = tokio::time::timeout(Duration::from_secs(1), network_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("controller receive timed out"))
                .unwrap_or_else(|| panic!("controller reader stopped"));
            assert!(matches!(
                event,
                NetworkEvent::Message {
                    message: Session::HeartbeatAcknowledged { nonce },
                    ..
                } if nonce == expected
            ));
        }
        agent_sender.close();
        drop(outbound);
        worker.abort();
    }
}
