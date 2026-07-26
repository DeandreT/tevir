use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    thread::{self, JoinHandle},
    time::Duration,
};

use domain::{Edge, NodeId, ScreenPlacement, Topology};
use identity::{LocalIdentity, TrustStore};
use platform::{
    BackendKind, CaptureService, CaptureServiceEvent, InjectionService, InjectionServiceEvent,
};
use protocol::{Capabilities, Session};
use session::{AgentAction, AgentSession, ControllerAction, ControllerSession};
use thiserror::Error;
use tokio::sync::mpsc as tokio_mpsc;
use transport::{
    PeerConnection, ReconnectPolicy, SecureClient, SecureServer, SessionLimits, SessionProfile,
    TransportError,
};

use crate::config::{Config, Role};

const COMMAND_CAPACITY: usize = 4;
const EVENT_CAPACITY: usize = 256;
const ACCEPT_CAPACITY: usize = 8;
const NETWORK_EVENT_CAPACITY: usize = 256;
const OUTBOUND_CAPACITY: usize = 128;
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
    ) -> Result<Self, RuntimeStartError> {
        validate_trust(&config, &trust)?;
        let (commands, command_rx) = tokio_mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, events) = mpsc::sync_channel(EVENT_CAPACITY);
        let worker = thread::Builder::new()
            .name(String::from("tevir-session"))
            .spawn(move || run_worker(config, identity, trust, command_rx, event_tx))
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
    Error {
        message: String,
    },
    Stopped,
}

enum RuntimeCommand {
    Stop,
}

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
    let result = match config.role.clone() {
        Role::Controller { listen, topology } => runtime.block_on(run_controller(
            config.node,
            listen,
            topology,
            identity,
            trust,
            commands,
            events.clone(),
        )),
        Role::Agent {
            controller_node,
            controller,
            display_size,
        } => runtime.block_on(run_agent(
            config.node,
            controller_node,
            controller,
            display_size,
            identity,
            trust,
            commands,
            events.clone(),
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
    topology: Topology,
    identity: LocalIdentity,
    trust: TrustStore,
    mut commands: tokio_mpsc::Receiver<RuntimeCommand>,
    events: SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    send_event(
        &events,
        RuntimeEvent::Starting {
            role: RuntimeRole::Controller,
        },
    );
    let profile = session_profile();
    let server = SecureServer::bind(listen, identity, &trust, profile, SessionLimits::default())
        .map_err(|error| RuntimeError::Transport(error.to_string()))?;
    let address = server
        .local_addr()
        .map_err(|error| RuntimeError::Transport(error.to_string()))?;
    send_event(&events, RuntimeEvent::Listening { address });
    let (mut accepted, accept_worker) = spawn_accept_worker(server);

    let edges = capture_edges(&topology, &local_node);
    let capture =
        CaptureService::start(&edges).map_err(|error| RuntimeError::Native(error.to_string()))?;
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

    loop {
        tokio::select! {
            command = commands.recv() => {
                if command.is_none() || matches!(command, Some(RuntimeCommand::Stop)) {
                    break;
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
                        &mut controller,
                        &capture,
                        &mut peers,
                        &events,
                    )?;
                }
            }
            _ = input_tick.tick() => {
                drain_capture_events(
                    &capture,
                    &topology,
                    &local_node,
                    &mut controller,
                    &peers,
                    &events,
                )?;
                let actions = controller
                    .flush()
                    .map_err(|error| RuntimeError::Session(error.to_string()))?;
                apply_controller_actions(actions, &capture, &peers, &events)?;
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
    let outbound = spawn_connection_worker(connection, network_tx.clone());
    let initial_focus = Session::FocusChanged {
        focus_epoch: controller.focus_epoch(),
        target: controller.focus().clone(),
        entry_position: controller.focus_position(),
    };
    outbound
        .try_send(initial_focus)
        .map_err(|_| RuntimeError::OutboundQueue(info.peer.clone()))?;
    peers.insert(
        info.peer.clone(),
        ActivePeer {
            session_id: info.session_id,
            outbound,
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
    topology: &Topology,
    local_node: &NodeId,
    controller: &mut ControllerSession,
    peers: &BTreeMap<NodeId, ActivePeer>,
    events: &SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    while let Ok(event) = capture.try_recv() {
        match event {
            CaptureServiceEvent::Ready { backend } => {
                send_event(events, RuntimeEvent::NativeReady { backend });
            }
            CaptureServiceEvent::Activated { edge } => {
                let Some((target, offset)) = activation_target(topology, local_node, edge) else {
                    capture
                        .release()
                        .map_err(|error| RuntimeError::Native(error.to_string()))?;
                    continue;
                };
                if !peers.contains_key(&target) {
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
            }
            CaptureServiceEvent::Input(event) => {
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
            CaptureServiceEvent::Released => {}
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

fn handle_controller_network(
    event: NetworkEvent,
    controller: &mut ControllerSession,
    capture: &CaptureService,
    peers: &mut BTreeMap<NodeId, ActivePeer>,
    events: &SyncSender<RuntimeEvent>,
) -> Result<(), RuntimeError> {
    match event {
        NetworkEvent::Message {
            peer,
            session_id,
            message,
        } if active_session(peers, &peer, session_id) => match message {
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
            Session::FocusChanged { .. } | Session::Input(_) | Session::Clipboard(_) => {
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
        NetworkEvent::Message { .. } => {}
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
    display_size: domain::Size,
    identity: LocalIdentity,
    trust: TrustStore,
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
        session_profile(),
        SessionLimits::default(),
    )
    .map_err(|error| RuntimeError::Transport(error.to_string()))?;
    let injection =
        InjectionService::start().map_err(|error| RuntimeError::Native(error.to_string()))?;
    let reconnect = ReconnectPolicy::default();
    let mut attempt = 0u32;

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
            Ok(mut connection) => {
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
                    display_size,
                    &injection,
                    &mut connection,
                    &mut commands,
                    &events,
                )
                .await?;
                connection.close();
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

    let _ = injection.release_all();
    Ok(())
}

async fn run_agent_connection(
    local_node: &NodeId,
    controller_node: &NodeId,
    display_size: domain::Size,
    injection: &InjectionService,
    connection: &mut PeerConnection,
    commands: &mut tokio_mpsc::Receiver<RuntimeCommand>,
    events: &SyncSender<RuntimeEvent>,
) -> Result<AgentConnectionOutcome, RuntimeError> {
    let mut agent = AgentSession::new(local_node.clone(), display_size);
    let mut input_tick = tokio::time::interval(INPUT_POLL_INTERVAL);
    input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut controlled = false;
    let mut application_pending = false;

    loop {
        tokio::select! {
            command = commands.recv() => {
                if command.is_none() || matches!(command, Some(RuntimeCommand::Stop)) {
                    let _ = connection.send(Session::Disconnect).await;
                    let _ = injection.release_all();
                    return Ok(AgentConnectionOutcome::Stopped);
                }
            }
            message = connection.receive(), if !application_pending => {
                let message = match message {
                    Ok(message) => message,
                    Err(_) => {
                        let _ = injection.release_all();
                        return Ok(AgentConnectionOutcome::Disconnected);
                    }
                };
                let actions = agent
                    .handle(message)
                    .map_err(|error| RuntimeError::Session(error.to_string()))?;
                application_pending = actions
                    .iter()
                    .any(|action| matches!(action, AgentAction::ApplyInput(_)));
                apply_agent_actions(actions, injection, connection).await?;
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
                while let Ok(event) = injection.try_recv() {
                    match event {
                        InjectionServiceEvent::Ready { backend } => {
                            send_event(events, RuntimeEvent::NativeReady { backend });
                        }
                        InjectionServiceEvent::Applied { sequence } => {
                            application_pending = false;
                            let actions = agent
                                .confirm_applied(sequence)
                                .map_err(|error| RuntimeError::Session(error.to_string()))?;
                            apply_agent_actions(actions, injection, connection).await?;
                        }
                        InjectionServiceEvent::Released => {}
                        InjectionServiceEvent::Failed { operation, reason } => {
                            return Err(RuntimeError::Native(format!("{operation}: {reason}")));
                        }
                        InjectionServiceEvent::Stopped => {
                            return Err(RuntimeError::Native(String::from(
                                "Input injection service stopped",
                            )));
                        }
                    }
                }
            }
        }
    }
}

async fn apply_agent_actions(
    actions: Vec<AgentAction>,
    injection: &InjectionService,
    connection: &mut PeerConnection,
) -> Result<(), RuntimeError> {
    for action in actions {
        match action {
            AgentAction::FocusEntered { .. } => {}
            AgentAction::ApplyInput(batch) => {
                injection
                    .apply_batch(batch.sequence, batch.events)
                    .map_err(|error| RuntimeError::Native(error.to_string()))?
            }
            AgentAction::ReleaseAllInput => injection
                .release_all()
                .map_err(|error| RuntimeError::Native(error.to_string()))?,
            AgentAction::Send(message) => {
                connection
                    .send(message)
                    .await
                    .map_err(|error| RuntimeError::Transport(error.to_string()))?;
            }
            AgentAction::Clipboard(_) => {}
            AgentAction::CloseConnection => connection.close(),
        }
    }
    Ok(())
}

fn spawn_connection_worker(
    mut connection: PeerConnection,
    events: tokio_mpsc::Sender<NetworkEvent>,
) -> tokio_mpsc::Sender<Session> {
    let peer = connection.info().peer.clone();
    let session_id = connection.info().session_id;
    let (outbound, mut outbound_rx) = tokio_mpsc::channel(OUTBOUND_CAPACITY);
    tokio::spawn(async move {
        let reason = loop {
            tokio::select! {
                message = outbound_rx.recv() => {
                    let Some(message) = message else {
                        break String::from("Connection replaced");
                    };
                    if let Err(error) = connection.send(message).await {
                        break error.to_string();
                    }
                }
                message = connection.receive() => {
                    match message {
                        Ok(message) => {
                            if events.send(NetworkEvent::Message {
                                peer: peer.clone(),
                                session_id,
                                message,
                            }).await.is_err() {
                                return;
                            }
                        }
                        Err(error) => break error.to_string(),
                    }
                }
            }
        };
        connection.close();
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

fn capture_edges(topology: &Topology, local_node: &NodeId) -> Vec<Edge> {
    [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
        .into_iter()
        .filter(|edge| activation_target(topology, local_node, *edge).is_some())
        .collect()
}

fn activation_target(
    topology: &Topology,
    local_node: &NodeId,
    edge: Edge,
) -> Option<(NodeId, u32)> {
    let source = topology.screen(local_node)?;
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
    let (source_start, candidate_start, candidate_end) = match edge {
        Edge::Left if candidate.bounds.right() == source.bounds.left() => (
            source.bounds.top(),
            candidate.bounds.top(),
            candidate.bounds.bottom(),
        ),
        Edge::Right if candidate.bounds.left() == source.bounds.right() => (
            source.bounds.top(),
            candidate.bounds.top(),
            candidate.bounds.bottom(),
        ),
        Edge::Top if candidate.bounds.bottom() == source.bounds.top() => (
            source.bounds.left(),
            candidate.bounds.left(),
            candidate.bounds.right(),
        ),
        Edge::Bottom if candidate.bounds.top() == source.bounds.bottom() => (
            source.bounds.left(),
            candidate.bounds.left(),
            candidate.bounds.right(),
        ),
        _ => return None,
    };
    let source_end = match edge {
        Edge::Left | Edge::Right => source.bounds.bottom(),
        Edge::Top | Edge::Bottom => source.bounds.right(),
    };
    let overlap_start = source_start.max(candidate_start);
    let overlap_end = source_end.min(candidate_end);
    if overlap_start >= overlap_end {
        return None;
    }
    u32::try_from((overlap_start + overlap_end - 1) / 2 - source_start).ok()
}

fn session_profile() -> SessionProfile {
    SessionProfile {
        platform: platform::probe_host().platform,
        capabilities: Capabilities {
            keyboard: true,
            relative_pointer: true,
            absolute_pointer: false,
            clipboard_text: false,
        },
    }
}

fn send_event(events: &SyncSender<RuntimeEvent>, event: RuntimeEvent) {
    let _ = events.send(event);
}

struct ActivePeer {
    session_id: u128,
    outbound: tokio_mpsc::Sender<Session>,
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
    #[error("could not start session worker: {0}")]
    Spawn(std::io::Error),
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

    use domain::{Edge, NodeId, Point, Rect, ScreenPlacement, Size, Topology};
    use identity::{IdentityStore, LocalIdentity, TrustStore};
    use tempfile::TempDir;
    use transport::{SecureClient, SecureServer, SessionLimits};

    use super::{activation_target, capture_edges, session_profile, spawn_accept_worker};

    fn node(value: &str) -> NodeId {
        NodeId::new(value).unwrap_or_else(|error| panic!("invalid test node: {error}"))
    }

    fn screen(node_id: &str, x: i32, y: i32, width: u32, height: u32) -> ScreenPlacement {
        ScreenPlacement {
            node: node(node_id),
            bounds: Rect::new(
                Point { x, y },
                Size::new(
                    NonZeroU32::new(width).unwrap_or(NonZeroU32::MIN),
                    NonZeroU32::new(height).unwrap_or(NonZeroU32::MIN),
                ),
            ),
        }
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
            screen("local", 0, 0, 1920, 1080),
            screen("right", 1920, 200, 2560, 1440),
        ])
        .unwrap_or_else(|error| panic!("topology should be valid: {error}"));

        assert_eq!(capture_edges(&topology, &node("local")), vec![Edge::Right]);
        assert_eq!(
            activation_target(&topology, &node("local"), Edge::Right),
            Some((node("right"), 639))
        );
    }

    #[tokio::test]
    async fn accept_worker_owns_the_handshake_until_completion() {
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
            session_profile(),
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
            session_profile(),
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
        worker.abort();
    }
}
