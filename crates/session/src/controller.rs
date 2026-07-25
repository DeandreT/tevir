use std::collections::{BTreeMap, BTreeSet, VecDeque};

use domain::{Edge, InputEvent, InputKind, NodeId, Point, ScreenPlacement, Size, Topology};
use protocol::{InputBatch, Session};
use thiserror::Error;

use crate::coalesce::EventBuffer;

const MICROPIXELS_PER_PIXEL: i64 = 1_000_000;
pub const MAX_PENDING_BATCHES_PER_PEER: usize = 64;

pub struct ControllerSession {
    topology: Topology,
    local_node: NodeId,
    focus: NodeId,
    focus_epoch: u64,
    focus_x_micropixels: i64,
    focus_y_micropixels: i64,
    buffer: EventBuffer,
    deliveries: BTreeMap<NodeId, PeerDelivery>,
}

impl ControllerSession {
    pub fn new(topology: Topology, local_node: NodeId) -> Result<Self, ControllerError> {
        let local_screen = topology
            .screen(&local_node)
            .ok_or_else(|| ControllerError::LocalNodeMissing(local_node.clone()))?;
        let focus_position = screen_center(local_screen);
        let deliveries = peer_nodes(&topology, &local_node)
            .map(|node| (node.clone(), PeerDelivery::default()))
            .collect();

        Ok(Self {
            topology,
            focus: local_node.clone(),
            local_node,
            focus_epoch: 1,
            focus_x_micropixels: i64::from(focus_position.x) * MICROPIXELS_PER_PIXEL,
            focus_y_micropixels: i64::from(focus_position.y) * MICROPIXELS_PER_PIXEL,
            buffer: EventBuffer::default(),
            deliveries,
        })
    }

    #[must_use]
    pub fn focus(&self) -> &NodeId {
        &self.focus
    }

    #[must_use]
    pub const fn focus_epoch(&self) -> u64 {
        self.focus_epoch
    }

    #[must_use]
    pub fn focus_position(&self) -> Point {
        local_point(self.focus_x_micropixels, self.focus_y_micropixels)
    }

    pub fn activate(
        &mut self,
        edge: Edge,
        offset: u32,
    ) -> Result<Vec<ControllerAction>, ControllerError> {
        if self.focus != self.local_node {
            return Err(ControllerError::CaptureAlreadyActive(self.focus.clone()));
        }
        let transition = self
            .topology
            .transition(&self.local_node, edge, offset)
            .ok_or(ControllerError::NoAdjacentScreen { edge, offset })?;
        let target = transition.target.clone();
        let position = transition.local_position;
        self.change_focus(target, position, None)
    }

    pub fn route_input(
        &mut self,
        event: InputEvent,
    ) -> Result<Vec<ControllerAction>, ControllerError> {
        if self.focus == self.local_node {
            return Err(ControllerError::InputWhileLocal);
        }

        if let InputKind::PointerRelative {
            dx_micropixels,
            dy_micropixels,
        } = event.kind
            && let Some((target, position)) =
                self.pointer_destination(dx_micropixels, dy_micropixels)?
            && target != self.focus
        {
            let mut actions = self.flush()?;
            actions.extend(self.change_focus(target, position, None)?);
            return Ok(actions);
        }

        let mut actions = Vec::new();
        if self.buffer.is_full() && !self.buffer.can_coalesce(event) {
            actions.extend(self.flush()?);
        }
        self.observe_pointer(event)?;
        if self.buffer.push(event) {
            actions.extend(self.flush()?);
        }
        Ok(actions)
    }

    pub fn flush(&mut self) -> Result<Vec<ControllerAction>, ControllerError> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }
        if self.focus == self.local_node {
            self.buffer.take();
            return Err(ControllerError::InputWhileLocal);
        }

        let delivery = self
            .deliveries
            .get_mut(&self.focus)
            .ok_or_else(|| ControllerError::UnknownPeer(self.focus.clone()))?;
        if delivery.pending.len() >= MAX_PENDING_BATCHES_PER_PEER {
            return Err(ControllerError::Backpressure {
                peer: self.focus.clone(),
                maximum: MAX_PENDING_BATCHES_PER_PEER,
            });
        }
        let sequence = delivery
            .last_sent
            .checked_add(1)
            .ok_or_else(|| ControllerError::SequenceExhausted(self.focus.clone()))?;
        let batch = InputBatch {
            focus_epoch: self.focus_epoch,
            sequence,
            events: self.buffer.take(),
        };
        delivery.last_sent = sequence;
        delivery.pending.push_back(sequence);

        Ok(vec![ControllerAction::Send {
            peer: self.focus.clone(),
            message: Session::Input(batch),
        }])
    }

    pub fn acknowledge(
        &mut self,
        peer: &NodeId,
        through_sequence: u64,
    ) -> Result<usize, ControllerError> {
        let delivery = self
            .deliveries
            .get_mut(peer)
            .ok_or_else(|| ControllerError::UnknownPeer(peer.clone()))?;
        if through_sequence > delivery.last_sent {
            return Err(ControllerError::AcknowledgementBeyondSent {
                peer: peer.clone(),
                acknowledged: through_sequence,
                last_sent: delivery.last_sent,
            });
        }
        if through_sequence <= delivery.last_acknowledged {
            return Ok(0);
        }

        delivery.last_acknowledged = through_sequence;
        let original = delivery.pending.len();
        while delivery
            .pending
            .front()
            .is_some_and(|sequence| *sequence <= through_sequence)
        {
            delivery.pending.pop_front();
        }
        Ok(original - delivery.pending.len())
    }

    pub fn reconcile_topology(
        &mut self,
        topology: Topology,
    ) -> Result<Vec<ControllerAction>, ControllerError> {
        if topology.screen(&self.local_node).is_none() {
            return Err(ControllerError::LocalNodeMissing(self.local_node.clone()));
        }

        let mut actions = self.flush()?;
        let previous_peers: BTreeSet<NodeId> = peer_nodes(&self.topology, &self.local_node)
            .cloned()
            .collect();
        let next_peers: BTreeSet<NodeId> =
            peer_nodes(&topology, &self.local_node).cloned().collect();
        let recipients = previous_peers.union(&next_peers).cloned().collect();

        if topology.screen(&self.focus).is_none() {
            self.focus = self.local_node.clone();
            let position = topology
                .screen(&self.local_node)
                .map(screen_center)
                .ok_or_else(|| ControllerError::LocalNodeMissing(self.local_node.clone()))?;
            self.set_focus_position(position);
        } else {
            let screen = topology
                .screen(&self.focus)
                .ok_or_else(|| ControllerError::UnknownPeer(self.focus.clone()))?;
            self.clamp_focus_position(screen.bounds.size);
        }

        self.topology = topology;
        self.deliveries.retain(|node, _| next_peers.contains(node));
        for node in next_peers {
            self.deliveries.entry(node).or_default();
        }
        actions.extend(self.advance_epoch_and_broadcast(Some(recipients))?);
        Ok(actions)
    }

    #[must_use]
    pub fn pending_batches(&self, peer: &NodeId) -> usize {
        self.deliveries
            .get(peer)
            .map_or(0, |delivery| delivery.pending.len())
    }

    fn change_focus(
        &mut self,
        target: NodeId,
        position: Point,
        recipients: Option<BTreeSet<NodeId>>,
    ) -> Result<Vec<ControllerAction>, ControllerError> {
        let mut actions = self.flush()?;
        let target_screen = self
            .topology
            .screen(&target)
            .ok_or_else(|| ControllerError::UnknownPeer(target.clone()))?;
        self.focus = target;
        self.set_focus_position(clamp_local_position(target_screen, position));
        actions.extend(self.advance_epoch_and_broadcast(recipients)?);
        Ok(actions)
    }

    fn advance_epoch_and_broadcast(
        &mut self,
        recipients: Option<BTreeSet<NodeId>>,
    ) -> Result<Vec<ControllerAction>, ControllerError> {
        self.focus_epoch = self
            .focus_epoch
            .checked_add(1)
            .ok_or(ControllerError::FocusEpochExhausted)?;
        let mut actions = Vec::new();
        if self.focus == self.local_node {
            actions.push(ControllerAction::ReleaseCapture);
        }
        let recipients = recipients.unwrap_or_else(|| {
            peer_nodes(&self.topology, &self.local_node)
                .cloned()
                .collect()
        });
        actions.extend(recipients.into_iter().map(|peer| ControllerAction::Send {
            peer,
            message: Session::FocusChanged {
                focus_epoch: self.focus_epoch,
                target: self.focus.clone(),
                entry_position: self.focus_position(),
            },
        }));
        Ok(actions)
    }

    fn pointer_destination(
        &self,
        dx_micropixels: i64,
        dy_micropixels: i64,
    ) -> Result<Option<(NodeId, Point)>, ControllerError> {
        let current = self
            .topology
            .screen(&self.focus)
            .ok_or_else(|| ControllerError::UnknownPeer(self.focus.clone()))?;
        let global_x = i64::from(current.bounds.origin.x)
            .saturating_mul(MICROPIXELS_PER_PIXEL)
            .saturating_add(self.focus_x_micropixels);
        let global_y = i64::from(current.bounds.origin.y)
            .saturating_mul(MICROPIXELS_PER_PIXEL)
            .saturating_add(self.focus_y_micropixels);
        let destination_x = global_x.saturating_add(dx_micropixels);
        let destination_y = global_y.saturating_add(dy_micropixels);
        let x = destination_x.div_euclid(MICROPIXELS_PER_PIXEL);
        let y = destination_y.div_euclid(MICROPIXELS_PER_PIXEL);
        let Some(destination) = self
            .topology
            .screens()
            .iter()
            .find(|screen| screen.bounds.contains_coordinates(x, y))
        else {
            return Ok(None);
        };
        if destination.node != current.node && !current.bounds.shares_edge(destination.bounds) {
            return Ok(None);
        }
        let local_x = i32::try_from(x - destination.bounds.left())
            .map_err(|_| ControllerError::PointerCoordinateOverflow)?;
        let local_y = i32::try_from(y - destination.bounds.top())
            .map_err(|_| ControllerError::PointerCoordinateOverflow)?;
        Ok(Some((
            destination.node.clone(),
            Point {
                x: local_x,
                y: local_y,
            },
        )))
    }

    fn observe_pointer(&mut self, event: InputEvent) -> Result<(), ControllerError> {
        match event.kind {
            InputKind::PointerRelative {
                dx_micropixels,
                dy_micropixels,
            } => {
                self.focus_x_micropixels = self.focus_x_micropixels.saturating_add(dx_micropixels);
                self.focus_y_micropixels = self.focus_y_micropixels.saturating_add(dy_micropixels);
            }
            InputKind::PointerAbsolute(position) => self.set_focus_position(position),
            InputKind::Key { .. } | InputKind::PointerButton { .. } | InputKind::Scroll(_) => {
                return Ok(());
            }
        }
        let screen = self
            .topology
            .screen(&self.focus)
            .ok_or_else(|| ControllerError::UnknownPeer(self.focus.clone()))?;
        self.clamp_focus_position(screen.bounds.size);
        Ok(())
    }

    fn set_focus_position(&mut self, position: Point) {
        self.focus_x_micropixels = i64::from(position.x) * MICROPIXELS_PER_PIXEL;
        self.focus_y_micropixels = i64::from(position.y) * MICROPIXELS_PER_PIXEL;
    }

    fn clamp_focus_position(&mut self, size: Size) {
        let maximum_x = i64::from(size.width.get())
            .saturating_mul(MICROPIXELS_PER_PIXEL)
            .saturating_sub(1);
        let maximum_y = i64::from(size.height.get())
            .saturating_mul(MICROPIXELS_PER_PIXEL)
            .saturating_sub(1);
        self.focus_x_micropixels = self.focus_x_micropixels.clamp(0, maximum_x);
        self.focus_y_micropixels = self.focus_y_micropixels.clamp(0, maximum_y);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControllerAction {
    Send { peer: NodeId, message: Session },
    ReleaseCapture,
}

#[derive(Debug, Default)]
struct PeerDelivery {
    last_sent: u64,
    last_acknowledged: u64,
    pending: VecDeque<u64>,
}

fn peer_nodes<'a>(
    topology: &'a Topology,
    local_node: &'a NodeId,
) -> impl Iterator<Item = &'a NodeId> {
    topology
        .screens()
        .iter()
        .filter(move |screen| &screen.node != local_node)
        .map(|screen| &screen.node)
}

fn screen_center(screen: &ScreenPlacement) -> Point {
    Point {
        x: i32::try_from(screen.bounds.size.width.get() / 2).unwrap_or(i32::MAX),
        y: i32::try_from(screen.bounds.size.height.get() / 2).unwrap_or(i32::MAX),
    }
}

fn clamp_local_position(screen: &ScreenPlacement, position: Point) -> Point {
    let maximum_x = i32::try_from(screen.bounds.size.width.get() - 1).unwrap_or(i32::MAX);
    let maximum_y = i32::try_from(screen.bounds.size.height.get() - 1).unwrap_or(i32::MAX);
    Point {
        x: position.x.clamp(0, maximum_x),
        y: position.y.clamp(0, maximum_y),
    }
}

fn local_point(x_micropixels: i64, y_micropixels: i64) -> Point {
    Point {
        x: i32::try_from(x_micropixels.div_euclid(MICROPIXELS_PER_PIXEL)).unwrap_or(i32::MAX),
        y: i32::try_from(y_micropixels.div_euclid(MICROPIXELS_PER_PIXEL)).unwrap_or(i32::MAX),
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ControllerError {
    #[error("local node `{0}` is missing from the topology")]
    LocalNodeMissing(NodeId),
    #[error("node `{0}` is not a routable peer")]
    UnknownPeer(NodeId),
    #[error("capture is already routing to `{0}`")]
    CaptureAlreadyActive(NodeId),
    #[error("edge {edge:?} at offset {offset} has no adjacent screen")]
    NoAdjacentScreen { edge: Edge, offset: u32 },
    #[error("captured input arrived while focus was local")]
    InputWhileLocal,
    #[error("peer `{peer}` has {maximum} unacknowledged batches")]
    Backpressure { peer: NodeId, maximum: usize },
    #[error("input sequence for peer `{0}` is exhausted")]
    SequenceExhausted(NodeId),
    #[error("focus epoch is exhausted")]
    FocusEpochExhausted,
    #[error("peer `{peer}` acknowledged sequence {acknowledged}, but only {last_sent} was sent")]
    AcknowledgementBeyondSent {
        peer: NodeId,
        acknowledged: u64,
        last_sent: u64,
    },
    #[error("pointer coordinates exceed the supported range")]
    PointerCoordinateOverflow,
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use domain::{
        Edge, InputEvent, InputKind, KeyAction, NodeId, PhysicalKey, Point, Rect, ScreenPlacement,
        Size, Topology,
    };
    use protocol::Session;

    use super::{
        ControllerAction, ControllerError, ControllerSession, MAX_PENDING_BATCHES_PER_PEER,
    };

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

    fn topology() -> Topology {
        Topology::new(vec![
            screen("left", -1920, 0, 1920, 1080),
            screen("right", 0, 0, 2560, 1440),
        ])
        .unwrap_or_else(|error| panic!("test topology should be valid: {error}"))
    }

    fn relative(time: u64, dx: i64, dy: i64) -> InputEvent {
        InputEvent {
            elapsed_micros: time,
            kind: InputKind::PointerRelative {
                dx_micropixels: dx,
                dy_micropixels: dy,
            },
        }
    }

    #[test]
    fn activation_routes_to_a_mixed_resolution_neighbor() {
        let mut controller = ControllerSession::new(topology(), node("left"))
            .unwrap_or_else(|error| panic!("controller creation failed: {error}"));

        let actions = controller
            .activate(Edge::Right, 540)
            .unwrap_or_else(|error| panic!("activation failed: {error}"));

        assert_eq!(controller.focus(), &node("right"));
        assert_eq!(controller.focus_position(), Point { x: 0, y: 540 });
        assert!(actions.iter().any(|action| {
            matches!(
                action,
                ControllerAction::Send {
                    peer,
                    message: Session::FocusChanged {
                        focus_epoch: 2,
                        target,
                        entry_position: Point { x: 0, y: 540 },
                    },
                } if peer == &node("right") && target == &node("right")
            )
        }));
    }

    #[test]
    fn batches_motion_without_crossing_key_boundaries() {
        let mut controller = ControllerSession::new(topology(), node("left"))
            .unwrap_or_else(|error| panic!("controller creation failed: {error}"));
        controller
            .activate(Edge::Right, 540)
            .unwrap_or_else(|error| panic!("activation failed: {error}"));
        controller
            .route_input(relative(1, 100, 200))
            .unwrap_or_else(|error| panic!("routing failed: {error}"));
        controller
            .route_input(relative(2, 300, -50))
            .unwrap_or_else(|error| panic!("routing failed: {error}"));
        controller
            .route_input(InputEvent {
                elapsed_micros: 3,
                kind: InputKind::Key {
                    key: PhysicalKey::new(0x07, 0x04),
                    action: KeyAction::Press,
                },
            })
            .unwrap_or_else(|error| panic!("routing failed: {error}"));
        let actions = controller
            .flush()
            .unwrap_or_else(|error| panic!("flush failed: {error}"));

        let batch = actions.iter().find_map(|action| match action {
            ControllerAction::Send {
                message: Session::Input(batch),
                ..
            } => Some(batch),
            _ => None,
        });
        let batch = batch.unwrap_or_else(|| panic!("input batch should be emitted"));
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[0], relative(2, 400, 150));
        assert!(matches!(batch.events[1].kind, InputKind::Key { .. }));
    }

    #[test]
    fn logical_cursor_accumulates_subpixel_motion() {
        let mut controller = ControllerSession::new(topology(), node("left"))
            .unwrap_or_else(|error| panic!("controller creation failed: {error}"));
        controller
            .activate(Edge::Right, 540)
            .unwrap_or_else(|error| panic!("activation failed: {error}"));

        controller
            .route_input(relative(1, 500_000, 250_000))
            .unwrap_or_else(|error| panic!("routing failed: {error}"));
        controller
            .route_input(relative(2, 500_000, 750_000))
            .unwrap_or_else(|error| panic!("routing failed: {error}"));

        assert_eq!(controller.focus_position(), Point { x: 1, y: 541 });
    }

    #[test]
    fn crossing_back_to_local_releases_capture_and_drops_crossing_motion() {
        let mut controller = ControllerSession::new(topology(), node("left"))
            .unwrap_or_else(|error| panic!("controller creation failed: {error}"));
        controller
            .activate(Edge::Right, 540)
            .unwrap_or_else(|error| panic!("activation failed: {error}"));

        let actions = controller
            .route_input(relative(10, -1_000_000, 0))
            .unwrap_or_else(|error| panic!("routing failed: {error}"));

        assert_eq!(controller.focus(), &node("left"));
        assert!(actions.contains(&ControllerAction::ReleaseCapture));
        assert!(!actions.iter().any(|action| {
            matches!(
                action,
                ControllerAction::Send {
                    message: Session::Input(_),
                    ..
                }
            )
        }));
    }

    #[test]
    fn topology_change_flushes_old_epoch_before_broadcasting_the_new_one() {
        let mut controller = ControllerSession::new(topology(), node("left"))
            .unwrap_or_else(|error| panic!("controller creation failed: {error}"));
        controller
            .activate(Edge::Right, 540)
            .unwrap_or_else(|error| panic!("activation failed: {error}"));
        controller
            .route_input(relative(1, 10, 10))
            .unwrap_or_else(|error| panic!("routing failed: {error}"));
        let changed = Topology::new(vec![
            screen("left", -1600, 0, 1600, 900),
            screen("right", 0, 0, 1920, 1080),
        ])
        .unwrap_or_else(|error| panic!("changed topology should be valid: {error}"));

        let actions = controller
            .reconcile_topology(changed)
            .unwrap_or_else(|error| panic!("reconcile failed: {error}"));

        assert!(matches!(
            actions.first(),
            Some(ControllerAction::Send {
                message: Session::Input(batch),
                ..
            }) if batch.focus_epoch == 2
        ));
        assert!(matches!(
            actions.last(),
            Some(ControllerAction::Send {
                message: Session::FocusChanged { focus_epoch: 3, .. },
                ..
            })
        ));
    }

    #[test]
    fn acknowledgements_are_bounded_by_the_last_sent_sequence() {
        let mut controller = ControllerSession::new(topology(), node("left"))
            .unwrap_or_else(|error| panic!("controller creation failed: {error}"));
        controller
            .activate(Edge::Right, 540)
            .unwrap_or_else(|error| panic!("activation failed: {error}"));
        controller
            .route_input(relative(1, 5, 5))
            .unwrap_or_else(|error| panic!("routing failed: {error}"));
        controller
            .flush()
            .unwrap_or_else(|error| panic!("flush failed: {error}"));

        assert_eq!(controller.acknowledge(&node("right"), 1), Ok(1));
        assert!(matches!(
            controller.acknowledge(&node("right"), 2),
            Err(ControllerError::AcknowledgementBeyondSent { .. })
        ));
    }

    #[test]
    fn backpressure_retains_the_next_batch_until_an_acknowledgement() {
        let mut controller = ControllerSession::new(topology(), node("left"))
            .unwrap_or_else(|error| panic!("controller creation failed: {error}"));
        controller
            .activate(Edge::Right, 540)
            .unwrap_or_else(|error| panic!("activation failed: {error}"));
        for sequence in 0..MAX_PENDING_BATCHES_PER_PEER {
            controller
                .route_input(relative(sequence as u64, 1, 0))
                .and_then(|_| controller.flush())
                .unwrap_or_else(|error| panic!("batch {sequence} failed: {error}"));
        }

        controller
            .route_input(relative(100, 1, 0))
            .unwrap_or_else(|error| panic!("buffering should succeed: {error}"));
        assert!(matches!(
            controller.flush(),
            Err(ControllerError::Backpressure { .. })
        ));
        assert_eq!(controller.acknowledge(&node("right"), 1), Ok(1));
        assert!(controller.flush().is_ok_and(|actions| actions.len() == 1));
    }

    #[test]
    fn backpressure_never_grows_a_batch_past_the_protocol_limit() {
        let mut controller = ControllerSession::new(topology(), node("left"))
            .unwrap_or_else(|error| panic!("controller creation failed: {error}"));
        controller
            .activate(Edge::Right, 540)
            .unwrap_or_else(|error| panic!("activation failed: {error}"));
        for sequence in 0..MAX_PENDING_BATCHES_PER_PEER {
            controller
                .route_input(relative(sequence as u64, 1, 0))
                .and_then(|_| controller.flush())
                .unwrap_or_else(|error| panic!("batch {sequence} failed: {error}"));
        }

        for offset in 0..protocol::MAX_INPUT_EVENTS_PER_BATCH {
            let result = controller.route_input(InputEvent {
                elapsed_micros: 100 + offset as u64,
                kind: InputKind::Key {
                    key: PhysicalKey::new(0x07, 0x04),
                    action: KeyAction::Press,
                },
            });
            if offset + 1 == protocol::MAX_INPUT_EVENTS_PER_BATCH {
                assert!(matches!(result, Err(ControllerError::Backpressure { .. })));
            } else {
                assert!(result.is_ok());
            }
        }
        assert!(matches!(
            controller.route_input(InputEvent {
                elapsed_micros: 1_000,
                kind: InputKind::Key {
                    key: PhysicalKey::new(0x07, 0x05),
                    action: KeyAction::Press,
                },
            }),
            Err(ControllerError::Backpressure { .. })
        ));

        assert_eq!(controller.acknowledge(&node("right"), 1), Ok(1));
        let actions = controller
            .flush()
            .unwrap_or_else(|error| panic!("retained batch failed: {error}"));
        assert!(matches!(
            actions.as_slice(),
            [ControllerAction::Send {
                message: Session::Input(batch),
                ..
            }] if batch.events.len() == protocol::MAX_INPUT_EVENTS_PER_BATCH
        ));
    }
}
