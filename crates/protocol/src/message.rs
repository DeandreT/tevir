use std::num::NonZeroU32;

use domain::{HostPlatform, InputEvent, NodeId, Point, Size};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ClipboardControl, ClipboardError};

pub const CURRENT_PROTOCOL: ProtocolVersion = ProtocolVersion { major: 1, minor: 3 };
pub const MAX_INPUT_EVENTS_PER_BATCH: usize = 512;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    /// Tevir intentionally has no legacy compatibility layer.
    #[must_use]
    pub const fn is_current(self) -> bool {
        self.major == CURRENT_PROTOCOL.major && self.minor == CURRENT_PROTOCOL.minor
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Capabilities {
    pub keyboard: bool,
    pub relative_pointer: bool,
    pub absolute_pointer: bool,
    pub clipboard_text: bool,
}

impl Capabilities {
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self {
            keyboard: self.keyboard && other.keyboard,
            relative_pointer: self.relative_pointer && other.relative_pointer,
            absolute_pointer: self.absolute_pointer && other.absolute_pointer,
            clipboard_text: self.clipboard_text && other.clipboard_text,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Hello {
    pub version: ProtocolVersion,
    pub node: NodeId,
    pub nonce: [u8; 32],
    pub platform: HostPlatform,
    pub capabilities: Capabilities,
    pub maximum_frame_bytes: NonZeroU32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    VersionMismatch,
    UnknownNode,
    AuthenticationFailed,
    ReplayDetected,
    AlreadyConnected,
    PolicyDenied,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Handshake {
    Hello(Hello),
    Accepted {
        session_id: u128,
        controller: NodeId,
        client_nonce: [u8; 32],
        server_nonce: [u8; 32],
        platform: HostPlatform,
        negotiated_capabilities: Capabilities,
        maximum_frame_bytes: NonZeroU32,
        heartbeat_interval_ms: NonZeroU32,
    },
    Rejected {
        reason: RejectReason,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputBatch {
    pub focus_epoch: u64,
    pub sequence: u64,
    pub events: Vec<InputEvent>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Session {
    DisplayChanged {
        size: Size,
        monitor_count: NonZeroU32,
    },
    FocusChanged {
        focus_epoch: u64,
        target: NodeId,
        /// Position in the target node's local desktop coordinates.
        entry_position: Point,
    },
    Input(InputBatch),
    InputAcknowledged {
        through_sequence: u64,
    },
    Heartbeat {
        nonce: u64,
    },
    HeartbeatAcknowledged {
        nonce: u64,
    },
    Clipboard(ClipboardControl),
    Disconnect,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Envelope {
    Handshake(Handshake),
    Session(Session),
}

impl Envelope {
    pub(crate) fn validate(&self) -> Result<(), MessageValidationError> {
        if let Self::Session(message) = self {
            match message {
                Session::Input(batch) => {
                    if batch.events.is_empty() {
                        return Err(MessageValidationError::EmptyInputBatch);
                    }
                    if batch.events.len() > MAX_INPUT_EVENTS_PER_BATCH {
                        return Err(MessageValidationError::TooManyInputEvents {
                            actual: batch.events.len(),
                            maximum: MAX_INPUT_EVENTS_PER_BATCH,
                        });
                    }
                }
                Session::Clipboard(ClipboardControl::Offered(offer)) => offer.validate()?,
                Session::DisplayChanged { .. }
                | Session::FocusChanged { .. }
                | Session::InputAcknowledged { .. }
                | Session::Heartbeat { .. }
                | Session::HeartbeatAcknowledged { .. }
                | Session::Clipboard(ClipboardControl::Applied { .. })
                | Session::Disconnect => {}
            }
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MessageValidationError {
    #[error("an input batch cannot be empty")]
    EmptyInputBatch,
    #[error("input batch has {actual} events; the maximum is {maximum}")]
    TooManyInputEvents { actual: usize, maximum: usize },
    #[error(transparent)]
    InvalidClipboard(#[from] ClipboardError),
}

#[cfg(test)]
mod tests {
    use super::{CURRENT_PROTOCOL, Capabilities, ProtocolVersion};

    #[test]
    fn compatibility_requires_the_exact_current_version() {
        assert!(CURRENT_PROTOCOL.is_current());
        assert!(!ProtocolVersion { major: 1, minor: 4 }.is_current());
        assert!(!ProtocolVersion { major: 1, minor: 2 }.is_current());
    }

    #[test]
    fn capability_negotiation_is_an_intersection() {
        let controller = Capabilities {
            keyboard: true,
            relative_pointer: true,
            absolute_pointer: false,
            clipboard_text: true,
        };
        let agent = Capabilities {
            keyboard: true,
            relative_pointer: false,
            absolute_pointer: true,
            clipboard_text: true,
        };

        assert_eq!(
            controller.intersection(agent),
            Capabilities {
                keyboard: true,
                relative_pointer: false,
                absolute_pointer: false,
                clipboard_text: true,
            }
        );
    }
}
