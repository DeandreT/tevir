use domain::{NodeId, Point, Size};
use protocol::{ClipboardControl, InputBatch, MAX_INPUT_EVENTS_PER_BATCH, Session};
use thiserror::Error;

pub struct AgentSession {
    local_node: NodeId,
    display_size: Size,
    highest_focus_epoch: u64,
    active_focus_epoch: Option<u64>,
    last_focus: Option<FocusState>,
    last_applied_sequence: u64,
    pending_batch: Option<InputBatch>,
    closed: bool,
}

impl AgentSession {
    #[must_use]
    pub fn new(local_node: NodeId, display_size: Size) -> Self {
        Self {
            local_node,
            display_size,
            highest_focus_epoch: 0,
            active_focus_epoch: None,
            last_focus: None,
            last_applied_sequence: 0,
            pending_batch: None,
            closed: false,
        }
    }

    #[must_use]
    pub const fn active_focus_epoch(&self) -> Option<u64> {
        self.active_focus_epoch
    }

    #[must_use]
    pub const fn last_applied_sequence(&self) -> u64 {
        self.last_applied_sequence
    }

    pub fn handle(&mut self, message: Session) -> Result<Vec<AgentAction>, AgentError> {
        if self.closed {
            return Err(AgentError::SessionClosed);
        }

        match message {
            Session::DisplayChanged { .. } => Err(AgentError::UnexpectedDisplayChange),
            Session::FocusChanged {
                focus_epoch,
                target,
                entry_position,
            } => self.change_focus(focus_epoch, target, entry_position),
            Session::Input(batch) => self.receive_input(batch),
            Session::InputAcknowledged { .. } => Err(AgentError::UnexpectedAcknowledgement),
            Session::Heartbeat { nonce } => {
                Ok(vec![AgentAction::Send(Session::HeartbeatAcknowledged {
                    nonce,
                })])
            }
            Session::HeartbeatAcknowledged { .. } => Ok(Vec::new()),
            Session::Clipboard(control) => Ok(vec![AgentAction::Clipboard(control)]),
            Session::Disconnect => Ok(self.close()),
        }
    }

    /// Confirms that the pending batch was fully applied by the native backend.
    ///
    /// The returned acknowledgement must only be sent after this method succeeds.
    pub fn confirm_applied(&mut self, sequence: u64) -> Result<Vec<AgentAction>, AgentError> {
        if self.closed {
            return Err(AgentError::SessionClosed);
        }
        let pending = self
            .pending_batch
            .as_ref()
            .ok_or(AgentError::NoInputApplicationPending)?;
        if pending.sequence != sequence {
            return Err(AgentError::AppliedSequenceMismatch {
                expected: pending.sequence,
                received: sequence,
            });
        }

        self.last_applied_sequence = sequence;
        self.pending_batch = None;
        Ok(vec![AgentAction::Send(Session::InputAcknowledged {
            through_sequence: sequence,
        })])
    }

    /// Invalidates remote focus after the native display geometry changes.
    #[must_use]
    pub fn reconcile_display(&mut self, display_size: Size) -> Vec<AgentAction> {
        self.display_size = display_size;
        self.active_focus_epoch = None;
        self.pending_batch = None;
        tracing::info!(
            width = display_size.width.get(),
            height = display_size.height.get(),
            "display change invalidated remote focus"
        );
        vec![AgentAction::ReleaseAllInput]
    }

    /// Closes the state machine after a transport or platform failure.
    #[must_use]
    pub fn connection_lost(&mut self) -> Vec<AgentAction> {
        self.close()
    }

    fn change_focus(
        &mut self,
        focus_epoch: u64,
        target: NodeId,
        entry_position: Point,
    ) -> Result<Vec<AgentAction>, AgentError> {
        if focus_epoch < self.highest_focus_epoch {
            tracing::debug!(
                received = focus_epoch,
                current = self.highest_focus_epoch,
                "ignored stale focus change"
            );
            return Ok(Vec::new());
        }

        let next_focus = FocusState {
            epoch: focus_epoch,
            target,
            entry_position,
        };
        if focus_epoch == self.highest_focus_epoch {
            return if self.last_focus.as_ref() == Some(&next_focus) {
                Ok(Vec::new())
            } else {
                Err(AgentError::ConflictingFocusEpoch { focus_epoch })
            };
        }
        if let Some(pending) = &self.pending_batch {
            return Err(AgentError::InputApplicationPending {
                sequence: pending.sequence,
            });
        }
        if next_focus.target == self.local_node
            && !contains_local(self.display_size, next_focus.entry_position)
        {
            return Err(AgentError::EntryPositionOutsideDisplay {
                position: next_focus.entry_position,
                width: self.display_size.width.get(),
                height: self.display_size.height.get(),
            });
        }

        let was_active = self.active_focus_epoch.take().is_some();
        self.highest_focus_epoch = focus_epoch;
        self.last_focus = Some(next_focus.clone());
        tracing::info!(
            focus_epoch,
            target = %next_focus.target,
            local = next_focus.target == self.local_node,
            "agent focus changed"
        );

        let mut actions = Vec::new();
        if was_active {
            actions.push(AgentAction::ReleaseAllInput);
        }
        if next_focus.target == self.local_node {
            self.active_focus_epoch = Some(focus_epoch);
            actions.push(AgentAction::FocusEntered {
                position: next_focus.entry_position,
            });
        }
        Ok(actions)
    }

    fn receive_input(&mut self, batch: InputBatch) -> Result<Vec<AgentAction>, AgentError> {
        validate_batch(&batch)?;
        if batch.focus_epoch < self.highest_focus_epoch {
            return Err(AgentError::StaleInputEpoch {
                received: batch.focus_epoch,
                current: self.highest_focus_epoch,
            });
        }
        if self.active_focus_epoch != Some(batch.focus_epoch) {
            return Err(AgentError::InactiveInputEpoch {
                received: batch.focus_epoch,
                active: self.active_focus_epoch,
            });
        }
        if let Some(pending) = &self.pending_batch {
            return Err(AgentError::InputApplicationPending {
                sequence: pending.sequence,
            });
        }
        if batch.sequence <= self.last_applied_sequence {
            return Ok(vec![AgentAction::Send(Session::InputAcknowledged {
                through_sequence: self.last_applied_sequence,
            })]);
        }
        let expected = self
            .last_applied_sequence
            .checked_add(1)
            .ok_or(AgentError::InputSequenceExhausted)?;
        if batch.sequence != expected {
            return Err(AgentError::InputSequenceGap {
                expected,
                received: batch.sequence,
            });
        }

        self.pending_batch = Some(batch.clone());
        Ok(vec![AgentAction::ApplyInput(batch)])
    }

    fn close(&mut self) -> Vec<AgentAction> {
        self.active_focus_epoch = None;
        self.pending_batch = None;
        self.closed = true;
        tracing::info!("agent session closed");
        vec![AgentAction::ReleaseAllInput, AgentAction::CloseConnection]
    }
}

/// Actions are ordered. A caller must finish `ApplyInput` before confirming the
/// batch and processing any later network action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentAction {
    FocusEntered { position: Point },
    ApplyInput(InputBatch),
    ReleaseAllInput,
    Send(Session),
    Clipboard(ClipboardControl),
    CloseConnection,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum AgentError {
    #[error("the agent session is closed")]
    SessionClosed,
    #[error("focus epoch {focus_epoch} conflicts with an earlier focus message")]
    ConflictingFocusEpoch { focus_epoch: u64 },
    #[error(
        "entry position ({}, {}) is outside the local {}x{} display",
        position.x,
        position.y,
        width,
        height
    )]
    EntryPositionOutsideDisplay {
        position: Point,
        width: u32,
        height: u32,
    },
    #[error("input batch epoch {received} is stale; the current epoch is {current}")]
    StaleInputEpoch { received: u64, current: u64 },
    #[error("input batch epoch {received} is not active; active epoch is {active:?}")]
    InactiveInputEpoch { received: u64, active: Option<u64> },
    #[error("input sequence gap: expected {expected}, received {received}")]
    InputSequenceGap { expected: u64, received: u64 },
    #[error("input sequence is exhausted")]
    InputSequenceExhausted,
    #[error("input batch cannot be empty")]
    EmptyInputBatch,
    #[error("input batch has {actual} events; the maximum is {maximum}")]
    TooManyInputEvents { actual: usize, maximum: usize },
    #[error("input batch {sequence} is still awaiting native application")]
    InputApplicationPending { sequence: u64 },
    #[error("there is no input batch awaiting native application")]
    NoInputApplicationPending,
    #[error("applied input sequence {received} does not match pending sequence {expected}")]
    AppliedSequenceMismatch { expected: u64, received: u64 },
    #[error("an input acknowledgement is not valid on an agent session")]
    UnexpectedAcknowledgement,
    #[error("a display change is not valid from a controller")]
    UnexpectedDisplayChange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FocusState {
    epoch: u64,
    target: NodeId,
    entry_position: Point,
}

fn contains_local(size: Size, point: Point) -> bool {
    point.x >= 0
        && point.y >= 0
        && i64::from(point.x) < i64::from(size.width.get())
        && i64::from(point.y) < i64::from(size.height.get())
}

fn validate_batch(batch: &InputBatch) -> Result<(), AgentError> {
    if batch.events.is_empty() {
        return Err(AgentError::EmptyInputBatch);
    }
    if batch.events.len() > MAX_INPUT_EVENTS_PER_BATCH {
        return Err(AgentError::TooManyInputEvents {
            actual: batch.events.len(),
            maximum: MAX_INPUT_EVENTS_PER_BATCH,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use domain::{
        Edge, InputEvent, InputKind, KeyAction, NodeId, PhysicalKey, Point, Rect, ScreenPlacement,
        Size, Topology,
    };
    use protocol::{InputBatch, Session};

    use super::{AgentAction, AgentError, AgentSession};
    use crate::{ControllerAction, ControllerSession};

    fn node(value: &str) -> NodeId {
        NodeId::new(value).unwrap_or_else(|error| panic!("invalid test node: {error}"))
    }

    fn size(width: u32, height: u32) -> Size {
        Size::new(
            NonZeroU32::new(width).unwrap_or(NonZeroU32::MIN),
            NonZeroU32::new(height).unwrap_or(NonZeroU32::MIN),
        )
    }

    fn event() -> InputEvent {
        InputEvent {
            elapsed_micros: 1,
            kind: InputKind::Key {
                key: PhysicalKey::new(0x07, 0x04),
                action: KeyAction::Press,
            },
        }
    }

    fn focus(epoch: u64, target: &str) -> Session {
        Session::FocusChanged {
            focus_epoch: epoch,
            target: node(target),
            entry_position: Point { x: 10, y: 20 },
        }
    }

    fn batch(epoch: u64, sequence: u64) -> Session {
        Session::Input(InputBatch {
            focus_epoch: epoch,
            sequence,
            events: vec![event()],
        })
    }

    fn focused_agent() -> AgentSession {
        let mut agent = AgentSession::new(node("agent"), size(1920, 1080));
        agent
            .handle(focus(2, "agent"))
            .unwrap_or_else(|error| panic!("focus failed: {error}"));
        agent
    }

    #[test]
    fn acknowledges_only_after_native_application_is_confirmed() {
        let mut agent = focused_agent();

        let actions = agent
            .handle(batch(2, 1))
            .unwrap_or_else(|error| panic!("batch failed: {error}"));
        assert!(matches!(
            actions.as_slice(),
            [AgentAction::ApplyInput(InputBatch { sequence: 1, .. })]
        ));
        assert_eq!(agent.last_applied_sequence(), 0);

        let actions = agent
            .confirm_applied(1)
            .unwrap_or_else(|error| panic!("confirmation failed: {error}"));
        assert_eq!(
            actions,
            vec![AgentAction::Send(Session::InputAcknowledged {
                through_sequence: 1,
            })]
        );
        assert_eq!(agent.last_applied_sequence(), 1);
    }

    #[test]
    fn rejects_stale_input_without_applying_it() {
        let mut agent = focused_agent();

        assert_eq!(
            agent.handle(batch(1, 1)),
            Err(AgentError::StaleInputEpoch {
                received: 1,
                current: 2,
            })
        );
        assert_eq!(agent.last_applied_sequence(), 0);
    }

    #[test]
    fn duplicate_sequence_only_repeats_the_acknowledgement() {
        let mut agent = focused_agent();
        agent
            .handle(batch(2, 1))
            .and_then(|_| agent.confirm_applied(1))
            .unwrap_or_else(|error| panic!("initial batch failed: {error}"));

        assert_eq!(
            agent.handle(batch(2, 1)),
            Ok(vec![AgentAction::Send(Session::InputAcknowledged {
                through_sequence: 1,
            })])
        );
    }

    #[test]
    fn rejects_a_sequence_gap() {
        let mut agent = focused_agent();

        assert_eq!(
            agent.handle(batch(2, 2)),
            Err(AgentError::InputSequenceGap {
                expected: 1,
                received: 2,
            })
        );
    }

    #[test]
    fn newer_focus_releases_input_before_leaving_the_agent() {
        let mut agent = focused_agent();

        assert_eq!(
            agent.handle(focus(3, "controller")),
            Ok(vec![AgentAction::ReleaseAllInput])
        );
        assert_eq!(agent.active_focus_epoch(), None);
        assert!(matches!(
            agent.handle(batch(2, 1)),
            Err(AgentError::StaleInputEpoch { .. })
        ));
    }

    #[test]
    fn display_change_invalidates_focus_until_a_new_epoch() {
        let mut agent = focused_agent();

        assert_eq!(
            agent.reconcile_display(size(2560, 1440)),
            vec![AgentAction::ReleaseAllInput]
        );
        assert_eq!(agent.active_focus_epoch(), None);
        assert_eq!(agent.handle(focus(2, "agent")), Ok(Vec::new()));
        assert!(matches!(
            agent.handle(batch(2, 1)),
            Err(AgentError::InactiveInputEpoch { .. })
        ));
        assert!(
            agent
                .handle(focus(3, "agent"))
                .is_ok_and(|actions| matches!(
                    actions.as_slice(),
                    [AgentAction::FocusEntered { .. }]
                ))
        );
    }

    #[test]
    fn display_change_with_the_same_bounds_still_invalidates_focus() {
        let mut agent = focused_agent();

        assert_eq!(
            agent.reconcile_display(size(1920, 1080)),
            vec![AgentAction::ReleaseAllInput]
        );
        assert_eq!(agent.active_focus_epoch(), None);
    }

    #[test]
    fn disconnect_releases_input_and_closes_the_session() {
        let mut agent = focused_agent();

        assert_eq!(
            agent.connection_lost(),
            vec![AgentAction::ReleaseAllInput, AgentAction::CloseConnection]
        );
        assert_eq!(
            agent.handle(Session::Heartbeat { nonce: 7 }),
            Err(AgentError::SessionClosed)
        );
    }

    #[test]
    fn rejects_an_entry_position_outside_the_display() {
        let mut agent = AgentSession::new(node("agent"), size(1920, 1080));

        assert!(matches!(
            agent.handle(Session::FocusChanged {
                focus_epoch: 2,
                target: node("agent"),
                entry_position: Point { x: 1920, y: 0 },
            }),
            Err(AgentError::EntryPositionOutsideDisplay { .. })
        ));
        assert_eq!(agent.active_focus_epoch(), None);
    }

    #[test]
    fn controller_and_agent_complete_an_acknowledged_batch() {
        let topology = Topology::new(vec![
            ScreenPlacement {
                node: node("controller"),
                bounds: Rect::new(Point { x: -1920, y: 0 }, size(1920, 1080)),
            },
            ScreenPlacement {
                node: node("agent"),
                bounds: Rect::new(Point { x: 0, y: 0 }, size(2560, 1440)),
            },
        ])
        .unwrap_or_else(|error| panic!("topology failed: {error}"));
        let mut controller = ControllerSession::new(topology, node("controller"))
            .unwrap_or_else(|error| panic!("controller failed: {error}"));
        let mut agent = AgentSession::new(node("agent"), size(2560, 1440));

        let focus_message = controller
            .activate(Edge::Right, 540)
            .unwrap_or_else(|error| panic!("activation failed: {error}"))
            .into_iter()
            .find_map(message_for_agent)
            .unwrap_or_else(|| panic!("focus message missing"));
        assert!(agent.handle(focus_message).is_ok());

        controller
            .route_input(event())
            .unwrap_or_else(|error| panic!("routing failed: {error}"));
        let input_message = controller
            .flush()
            .unwrap_or_else(|error| panic!("flush failed: {error}"))
            .into_iter()
            .find_map(message_for_agent)
            .unwrap_or_else(|| panic!("input message missing"));
        assert!(matches!(
            agent.handle(input_message),
            Ok(actions) if matches!(
                actions.as_slice(),
                [AgentAction::ApplyInput(InputBatch { sequence: 1, .. })]
            )
        ));

        let acknowledgement = agent
            .confirm_applied(1)
            .unwrap_or_else(|error| panic!("confirmation failed: {error}"))
            .into_iter()
            .find_map(|action| match action {
                AgentAction::Send(Session::InputAcknowledged { through_sequence }) => {
                    Some(through_sequence)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("acknowledgement missing"));
        assert_eq!(
            controller.acknowledge(&node("agent"), acknowledgement),
            Ok(1)
        );
        assert_eq!(controller.pending_batches(&node("agent")), 0);
    }

    fn message_for_agent(action: ControllerAction) -> Option<Session> {
        match action {
            ControllerAction::Send { peer, message } if peer == node("agent") => Some(message),
            ControllerAction::Send { .. } | ControllerAction::ReleaseCapture => None,
        }
    }
}
