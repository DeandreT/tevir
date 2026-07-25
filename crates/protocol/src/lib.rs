//! Tevir's native, versioned wire contract.

mod clipboard;
mod codec;
mod message;

pub use clipboard::{
    ClipboardControl, ClipboardError, ClipboardGeneration, ClipboardOffer, ClipboardText,
    MAX_CLIPBOARD_FRAME_BYTES, MAX_CLIPBOARD_TEXT_BYTES,
};
pub use codec::{CodecError, DEFAULT_MAX_FRAME_BYTES, FrameCodec, HARD_MAX_FRAME_BYTES};
pub use domain::HostPlatform;
pub use message::{
    CURRENT_PROTOCOL, Capabilities, Envelope, Handshake, Hello, InputBatch,
    MAX_INPUT_EVENTS_PER_BATCH, MessageValidationError, ProtocolVersion, RejectReason, Session,
};
