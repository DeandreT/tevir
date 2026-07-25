use std::num::NonZeroU64;

use domain::NodeId;
use protocol::{
    ClipboardControl, ClipboardError, ClipboardGeneration, ClipboardOffer, ClipboardText,
};
use thiserror::Error;

pub struct ClipboardSession {
    local_node: NodeId,
    peer_node: NodeId,
    next_local_sequence: Option<NonZeroU64>,
    last_sent_local_sequence: u64,
    last_acknowledged_local_sequence: u64,
    last_applied_remote_sequence: u64,
    highest_remote_sequence: u64,
    pending_local: Option<ClipboardGeneration>,
    incoming: Option<IncomingClipboard>,
    applying: Option<IncomingClipboard>,
    remote_text: Option<String>,
}

impl ClipboardSession {
    #[must_use]
    pub fn new(local_node: NodeId, peer_node: NodeId) -> Self {
        Self {
            local_node,
            peer_node,
            next_local_sequence: Some(NonZeroU64::MIN),
            last_sent_local_sequence: 0,
            last_acknowledged_local_sequence: 0,
            last_applied_remote_sequence: 0,
            highest_remote_sequence: 0,
            pending_local: None,
            incoming: None,
            applying: None,
            remote_text: None,
        }
    }

    pub fn local_changed(
        &mut self,
        text: impl Into<String>,
    ) -> Result<Vec<ClipboardAction>, ClipboardSessionError> {
        let text = text.into();
        if self
            .remote_text
            .as_deref()
            .is_some_and(|remote| remote == text)
        {
            tracing::debug!(peer = %self.peer_node, "suppressed remote clipboard echo");
            return Ok(Vec::new());
        }

        let sequence = self
            .next_local_sequence
            .ok_or(ClipboardSessionError::LocalSequenceExhausted)?;
        let generation = ClipboardGeneration::new(self.local_node.clone(), sequence);
        let transfer = ClipboardText::new(generation.clone(), text)?;
        self.next_local_sequence = sequence.get().checked_add(1).and_then(NonZeroU64::new);
        self.last_sent_local_sequence = sequence.get();
        self.pending_local = Some(generation);
        self.remote_text = None;

        tracing::debug!(
            peer = %self.peer_node,
            sequence = sequence.get(),
            bytes = transfer.text().len(),
            "local clipboard change staged"
        );
        Ok(vec![
            ClipboardAction::SendControl(ClipboardControl::Offered(transfer.offer())),
            ClipboardAction::SendTransfer(transfer),
        ])
    }

    pub fn receive_control(
        &mut self,
        control: ClipboardControl,
    ) -> Result<Vec<ClipboardAction>, ClipboardSessionError> {
        match control {
            ClipboardControl::Offered(offer) => self.receive_offer(offer),
            ClipboardControl::Applied { generation } => {
                self.receive_acknowledgement(generation)?;
                Ok(Vec::new())
            }
        }
    }

    pub fn receive_transfer(
        &mut self,
        transfer: ClipboardText,
    ) -> Result<Vec<ClipboardAction>, ClipboardSessionError> {
        let generation = transfer.generation().clone();
        match self.stage_generation(&generation)? {
            Stage::Ignore => return Ok(Vec::new()),
            Stage::Applying => {
                let applying = self
                    .applying
                    .as_ref()
                    .ok_or(ClipboardSessionError::StateInvariant)?;
                return if applying.transfer.as_ref() == Some(&transfer) {
                    Ok(Vec::new())
                } else {
                    Err(ClipboardSessionError::ConflictingTransfer(generation))
                };
            }
            Stage::Incoming => {}
        }

        let incoming = self
            .incoming
            .as_mut()
            .ok_or(ClipboardSessionError::StateInvariant)?;
        if let Some(existing) = incoming.transfer.as_ref() {
            if existing != &transfer {
                return Err(ClipboardSessionError::ConflictingTransfer(generation));
            }
            return Ok(Vec::new());
        }
        incoming.transfer = Some(transfer);
        self.finish_incoming()
    }

    pub fn confirm_applied(
        &mut self,
        generation: &ClipboardGeneration,
    ) -> Result<Vec<ClipboardAction>, ClipboardSessionError> {
        let applying = self
            .applying
            .as_ref()
            .ok_or(ClipboardSessionError::NoApplicationPending)?;
        if &applying.generation != generation {
            return Err(ClipboardSessionError::ApplicationGenerationMismatch {
                expected: applying.generation.clone(),
                received: generation.clone(),
            });
        }
        self.validate_incoming()?;

        let applied = self
            .applying
            .take()
            .ok_or(ClipboardSessionError::StateInvariant)?;
        let transfer = applied
            .transfer
            .ok_or(ClipboardSessionError::StateInvariant)?;
        self.last_applied_remote_sequence = generation.sequence.get();
        self.remote_text = Some(transfer.into_text());
        tracing::debug!(
            peer = %self.peer_node,
            owner = %generation.owner,
            sequence = generation.sequence.get(),
            "remote clipboard applied"
        );

        let mut actions = vec![ClipboardAction::SendControl(ClipboardControl::Applied {
            generation: generation.clone(),
        })];
        if let Some(action) = self.begin_application()? {
            actions.push(action);
        }
        Ok(actions)
    }

    #[must_use]
    pub fn pending_application(&self) -> Option<&ClipboardGeneration> {
        self.applying.as_ref().map(|pending| &pending.generation)
    }

    #[must_use]
    pub fn pending_local(&self) -> Option<&ClipboardGeneration> {
        self.pending_local.as_ref()
    }

    #[must_use]
    pub const fn last_acknowledged_local_sequence(&self) -> u64 {
        self.last_acknowledged_local_sequence
    }

    fn receive_offer(
        &mut self,
        offer: ClipboardOffer,
    ) -> Result<Vec<ClipboardAction>, ClipboardSessionError> {
        let generation = offer.generation().clone();
        match self.stage_generation(&generation)? {
            Stage::Ignore => return Ok(Vec::new()),
            Stage::Applying => {
                let applying = self
                    .applying
                    .as_ref()
                    .ok_or(ClipboardSessionError::StateInvariant)?;
                return if applying.offer.as_ref() == Some(&offer) {
                    Ok(Vec::new())
                } else {
                    Err(ClipboardSessionError::ConflictingOffer(generation))
                };
            }
            Stage::Incoming => {}
        }

        let incoming = self
            .incoming
            .as_mut()
            .ok_or(ClipboardSessionError::StateInvariant)?;
        if let Some(existing) = incoming.offer.as_ref() {
            if existing != &offer {
                return Err(ClipboardSessionError::ConflictingOffer(generation));
            }
            return Ok(Vec::new());
        }
        incoming.offer = Some(offer);
        self.finish_incoming()
    }

    fn receive_acknowledgement(
        &mut self,
        generation: ClipboardGeneration,
    ) -> Result<(), ClipboardSessionError> {
        if generation.owner != self.local_node {
            return Err(ClipboardSessionError::UnexpectedGenerationOwner {
                expected: self.local_node.clone(),
                received: generation.owner,
            });
        }
        let sequence = generation.sequence.get();
        if sequence > self.last_sent_local_sequence {
            return Err(ClipboardSessionError::AcknowledgementBeyondSent {
                acknowledged: sequence,
                last_sent: self.last_sent_local_sequence,
            });
        }
        if sequence <= self.last_acknowledged_local_sequence {
            return Ok(());
        }

        self.last_acknowledged_local_sequence = sequence;
        if self
            .pending_local
            .as_ref()
            .is_some_and(|pending| pending.sequence.get() <= sequence)
        {
            self.pending_local = None;
        }
        tracing::debug!(
            peer = %self.peer_node,
            sequence,
            "clipboard delivery acknowledged"
        );
        Ok(())
    }

    fn stage_generation(
        &mut self,
        generation: &ClipboardGeneration,
    ) -> Result<Stage, ClipboardSessionError> {
        if generation.owner != self.peer_node {
            return Err(ClipboardSessionError::UnexpectedGenerationOwner {
                expected: self.peer_node.clone(),
                received: generation.owner.clone(),
            });
        }
        let sequence = generation.sequence.get();
        if sequence <= self.last_applied_remote_sequence || sequence < self.highest_remote_sequence
        {
            return Ok(Stage::Ignore);
        }
        if self
            .applying
            .as_ref()
            .is_some_and(|pending| pending.generation == *generation)
        {
            return Ok(Stage::Applying);
        }

        if sequence > self.highest_remote_sequence {
            self.highest_remote_sequence = sequence;
            self.incoming = Some(IncomingClipboard::new(generation.clone()));
        } else if self.incoming.is_none() {
            self.incoming = Some(IncomingClipboard::new(generation.clone()));
        }
        if self
            .incoming
            .as_ref()
            .is_none_or(|incoming| incoming.generation != *generation)
        {
            return Err(ClipboardSessionError::StateInvariant);
        }
        Ok(Stage::Incoming)
    }

    fn finish_incoming(&mut self) -> Result<Vec<ClipboardAction>, ClipboardSessionError> {
        if let Err(error) = self.validate_incoming() {
            self.incoming = None;
            return Err(error);
        }
        if self.applying.is_some() {
            return Ok(Vec::new());
        }
        Ok(self.begin_application()?.into_iter().collect())
    }

    fn validate_incoming(&self) -> Result<(), ClipboardSessionError> {
        let Some(incoming) = self.incoming.as_ref() else {
            return Ok(());
        };
        if let (Some(offer), Some(transfer)) = (incoming.offer.as_ref(), incoming.transfer.as_ref())
        {
            offer.verify(transfer)?;
        }
        Ok(())
    }

    fn begin_application(&mut self) -> Result<Option<ClipboardAction>, ClipboardSessionError> {
        if self.applying.is_some() {
            return Ok(None);
        }
        let ready = self
            .incoming
            .as_ref()
            .is_some_and(IncomingClipboard::is_ready);
        if !ready {
            return Ok(None);
        }
        let incoming = self
            .incoming
            .take()
            .ok_or(ClipboardSessionError::StateInvariant)?;
        incoming.verify()?;
        let transfer = incoming
            .transfer
            .as_ref()
            .ok_or(ClipboardSessionError::StateInvariant)?
            .clone();
        tracing::debug!(
            peer = %self.peer_node,
            owner = %incoming.generation.owner,
            sequence = incoming.generation.sequence.get(),
            bytes = transfer.text().len(),
            "remote clipboard ready for native application"
        );
        self.applying = Some(incoming);
        Ok(Some(ClipboardAction::ApplyRemote(transfer)))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardAction {
    SendControl(ClipboardControl),
    SendTransfer(ClipboardText),
    ApplyRemote(ClipboardText),
}

#[derive(Clone, Debug)]
struct IncomingClipboard {
    generation: ClipboardGeneration,
    offer: Option<ClipboardOffer>,
    transfer: Option<ClipboardText>,
}

impl IncomingClipboard {
    const fn new(generation: ClipboardGeneration) -> Self {
        Self {
            generation,
            offer: None,
            transfer: None,
        }
    }

    const fn is_ready(&self) -> bool {
        self.offer.is_some() && self.transfer.is_some()
    }

    fn verify(&self) -> Result<(), ClipboardSessionError> {
        let offer = self
            .offer
            .as_ref()
            .ok_or(ClipboardSessionError::StateInvariant)?;
        let transfer = self
            .transfer
            .as_ref()
            .ok_or(ClipboardSessionError::StateInvariant)?;
        offer.verify(transfer)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Ignore,
    Applying,
    Incoming,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ClipboardSessionError {
    #[error(transparent)]
    Clipboard(#[from] ClipboardError),
    #[error("local clipboard generation sequence is exhausted")]
    LocalSequenceExhausted,
    #[error("clipboard generation belongs to `{received}`, expected `{expected}`")]
    UnexpectedGenerationOwner { expected: NodeId, received: NodeId },
    #[error("clipboard acknowledgement {acknowledged} exceeds last sent sequence {last_sent}")]
    AcknowledgementBeyondSent { acknowledged: u64, last_sent: u64 },
    #[error("clipboard generation {0:?} has conflicting offers")]
    ConflictingOffer(ClipboardGeneration),
    #[error("clipboard generation {0:?} has conflicting transfers")]
    ConflictingTransfer(ClipboardGeneration),
    #[error("there is no clipboard application awaiting native confirmation")]
    NoApplicationPending,
    #[error("applied clipboard generation {received:?} does not match pending {expected:?}")]
    ApplicationGenerationMismatch {
        expected: ClipboardGeneration,
        received: ClipboardGeneration,
    },
    #[error("clipboard state invariant was violated")]
    StateInvariant,
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use domain::NodeId;
    use protocol::{ClipboardControl, ClipboardGeneration, ClipboardText};

    use super::{ClipboardAction, ClipboardSession, ClipboardSessionError};

    fn node(value: &str) -> NodeId {
        NodeId::new(value).unwrap_or_else(|error| panic!("invalid test node: {error}"))
    }

    fn generation(owner: &str, sequence: u64) -> ClipboardGeneration {
        ClipboardGeneration::new(
            node(owner),
            NonZeroU64::new(sequence).unwrap_or(NonZeroU64::MIN),
        )
    }

    fn transfer(owner: &str, sequence: u64, text: &str) -> ClipboardText {
        ClipboardText::new(generation(owner, sequence), text)
            .unwrap_or_else(|error| panic!("invalid test transfer: {error}"))
    }

    #[test]
    fn local_changes_offer_control_before_the_bulk_transfer() {
        let mut session = ClipboardSession::new(node("left"), node("right"));

        let actions = session
            .local_changed("hello")
            .unwrap_or_else(|error| panic!("local change failed: {error}"));

        assert!(matches!(
            actions.as_slice(),
            [
                ClipboardAction::SendControl(ClipboardControl::Offered(offer)),
                ClipboardAction::SendTransfer(payload),
            ] if offer.generation() == payload.generation()
                && offer.verify(payload).is_ok()
                && payload.generation() == &generation("left", 1)
        ));
        assert_eq!(session.pending_local(), Some(&generation("left", 1)));
    }

    #[test]
    fn transfer_may_arrive_before_its_control_offer() {
        let mut session = ClipboardSession::new(node("right"), node("left"));
        let payload = transfer("left", 1, "hello");

        assert_eq!(session.receive_transfer(payload.clone()), Ok(Vec::new()));
        assert_eq!(
            session.receive_control(ClipboardControl::Offered(payload.offer())),
            Ok(vec![ClipboardAction::ApplyRemote(payload)])
        );
        assert_eq!(session.pending_application(), Some(&generation("left", 1)));
    }

    #[test]
    fn confirmation_acknowledges_only_after_native_application() {
        let mut sender = ClipboardSession::new(node("left"), node("right"));
        let mut receiver = ClipboardSession::new(node("right"), node("left"));
        let actions = sender
            .local_changed("hello")
            .unwrap_or_else(|error| panic!("local change failed: {error}"));
        let mut application = None;

        for action in actions {
            let received = match action {
                ClipboardAction::SendControl(control) => receiver.receive_control(control),
                ClipboardAction::SendTransfer(transfer) => receiver.receive_transfer(transfer),
                ClipboardAction::ApplyRemote(_) => panic!("sender cannot apply its own clipboard"),
            }
            .unwrap_or_else(|error| panic!("receive failed: {error}"));
            application = application.or_else(|| {
                received.into_iter().find_map(|action| match action {
                    ClipboardAction::ApplyRemote(transfer) => Some(transfer),
                    ClipboardAction::SendControl(_) | ClipboardAction::SendTransfer(_) => None,
                })
            });
        }

        let application = application.unwrap_or_else(|| panic!("application action missing"));
        assert_eq!(sender.last_acknowledged_local_sequence(), 0);
        let acknowledgements = receiver
            .confirm_applied(application.generation())
            .unwrap_or_else(|error| panic!("confirmation failed: {error}"));
        assert!(matches!(
            acknowledgements.as_slice(),
            [ClipboardAction::SendControl(ClipboardControl::Applied { generation })]
                if generation == application.generation()
        ));
        for action in acknowledgements {
            if let ClipboardAction::SendControl(control) = action {
                sender
                    .receive_control(control)
                    .unwrap_or_else(|error| panic!("acknowledgement failed: {error}"));
            }
        }
        assert_eq!(sender.last_acknowledged_local_sequence(), 1);
        assert!(sender.pending_local().is_none());
    }

    #[test]
    fn native_notification_of_remote_text_is_not_sent_back() {
        let mut session = ClipboardSession::new(node("right"), node("left"));
        let payload = transfer("left", 4, "remote text");
        session
            .receive_control(ClipboardControl::Offered(payload.offer()))
            .and_then(|_| session.receive_transfer(payload.clone()))
            .and_then(|_| session.confirm_applied(payload.generation()))
            .unwrap_or_else(|error| panic!("remote application failed: {error}"));

        assert_eq!(session.local_changed("remote text"), Ok(Vec::new()));
        assert!(matches!(
            session.local_changed("new local text"),
            Ok(actions) if matches!(
                actions.as_slice(),
                [ClipboardAction::SendControl(_), ClipboardAction::SendTransfer(_)]
            )
        ));
    }

    #[test]
    fn newest_complete_generation_waits_behind_native_application() {
        let mut session = ClipboardSession::new(node("right"), node("left"));
        let first = transfer("left", 1, "first");
        let latest = transfer("left", 3, "latest");

        assert_eq!(
            session.receive_control(ClipboardControl::Offered(first.offer())),
            Ok(Vec::new())
        );
        assert_eq!(
            session.receive_transfer(first.clone()),
            Ok(vec![ClipboardAction::ApplyRemote(first.clone())])
        );
        assert_eq!(
            session.receive_transfer(transfer("left", 2, "discarded")),
            Ok(Vec::new())
        );
        assert_eq!(
            session.receive_control(ClipboardControl::Offered(latest.offer())),
            Ok(Vec::new())
        );
        assert_eq!(session.receive_transfer(latest.clone()), Ok(Vec::new()));

        assert_eq!(
            session.confirm_applied(first.generation()),
            Ok(vec![
                ClipboardAction::SendControl(ClipboardControl::Applied {
                    generation: first.generation().clone(),
                }),
                ClipboardAction::ApplyRemote(latest.clone()),
            ])
        );
        assert_eq!(session.pending_application(), Some(latest.generation()));
    }

    #[test]
    fn stale_incomplete_generation_cannot_replace_a_newer_one() {
        let mut session = ClipboardSession::new(node("right"), node("left"));
        let old = transfer("left", 1, "old");
        let new = transfer("left", 2, "new");

        assert_eq!(session.receive_transfer(old), Ok(Vec::new()));
        assert_eq!(session.receive_transfer(new.clone()), Ok(Vec::new()));
        assert_eq!(
            session.receive_control(ClipboardControl::Offered(
                transfer("left", 1, "old").offer()
            )),
            Ok(Vec::new())
        );
        assert_eq!(
            session.receive_control(ClipboardControl::Offered(new.offer())),
            Ok(vec![ClipboardAction::ApplyRemote(new)])
        );
    }

    #[test]
    fn rejects_payload_that_does_not_match_the_offer() {
        let mut session = ClipboardSession::new(node("right"), node("left"));
        let offered = transfer("left", 1, "hello");
        let modified = transfer("left", 1, "jello");

        assert_eq!(
            session.receive_control(ClipboardControl::Offered(offered.offer())),
            Ok(Vec::new())
        );
        assert!(matches!(
            session.receive_transfer(modified),
            Err(ClipboardSessionError::Clipboard(_))
        ));
        assert!(session.pending_application().is_none());
    }

    #[test]
    fn rejects_generations_owned_by_another_node() {
        let mut session = ClipboardSession::new(node("right"), node("left"));
        let payload = transfer("third", 1, "hello");

        assert_eq!(
            session.receive_transfer(payload),
            Err(ClipboardSessionError::UnexpectedGenerationOwner {
                expected: node("left"),
                received: node("third"),
            })
        );
    }

    #[test]
    fn rejects_acknowledgement_for_unsent_generation() {
        let mut session = ClipboardSession::new(node("left"), node("right"));

        assert_eq!(
            session.receive_control(ClipboardControl::Applied {
                generation: generation("left", 1),
            }),
            Err(ClipboardSessionError::AcknowledgementBeyondSent {
                acknowledged: 1,
                last_sent: 0,
            })
        );
    }
}
