use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;

use crate::{Envelope, MessageValidationError};

const LENGTH_PREFIX_BYTES: usize = size_of::<u32>();
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const HARD_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// A length-delimited codec whose payload is one Postcard-encoded [`Envelope`].
#[derive(Clone, Debug)]
pub struct FrameCodec {
    maximum_frame_bytes: usize,
}

impl FrameCodec {
    pub fn new(maximum_frame_bytes: usize) -> Result<Self, CodecError> {
        if maximum_frame_bytes == 0 || maximum_frame_bytes > HARD_MAX_FRAME_BYTES {
            return Err(CodecError::InvalidMaximum {
                requested: maximum_frame_bytes,
                hard_maximum: HARD_MAX_FRAME_BYTES,
            });
        }

        Ok(Self {
            maximum_frame_bytes,
        })
    }

    pub fn encode(&self, message: &Envelope) -> Result<BytesMut, CodecError> {
        message.validate()?;
        let payload = postcard::to_stdvec(message).map_err(CodecError::Serialize)?;
        self.validate_frame_length(payload.len())?;

        let length = u32::try_from(payload.len()).map_err(|_| CodecError::FrameTooLarge {
            actual: payload.len(),
            maximum: self.maximum_frame_bytes,
        })?;
        let mut frame = BytesMut::with_capacity(LENGTH_PREFIX_BYTES + payload.len());
        frame.put_u32(length);
        frame.extend_from_slice(&payload);
        Ok(frame)
    }

    pub fn decode(&self, source: &mut BytesMut) -> Result<Option<Envelope>, CodecError> {
        if source.len() < LENGTH_PREFIX_BYTES {
            return Ok(None);
        }

        let payload_length = u32::from_be_bytes(
            source[..LENGTH_PREFIX_BYTES]
                .try_into()
                .map_err(|_| CodecError::InvalidLengthPrefix)?,
        ) as usize;
        self.validate_frame_length(payload_length)?;

        let frame_length = LENGTH_PREFIX_BYTES + payload_length;
        if source.len() < frame_length {
            return Ok(None);
        }

        source.advance(LENGTH_PREFIX_BYTES);
        let payload = source.split_to(payload_length);
        let message =
            postcard::from_bytes::<Envelope>(&payload).map_err(CodecError::Deserialize)?;
        message.validate()?;
        Ok(Some(message))
    }

    fn validate_frame_length(&self, actual: usize) -> Result<(), CodecError> {
        if actual > self.maximum_frame_bytes {
            return Err(CodecError::FrameTooLarge {
                actual,
                maximum: self.maximum_frame_bytes,
            });
        }
        Ok(())
    }
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self {
            maximum_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        }
    }
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("maximum frame length {requested} is outside 1..={hard_maximum}")]
    InvalidMaximum {
        requested: usize,
        hard_maximum: usize,
    },
    #[error("frame is {actual} bytes; the configured maximum is {maximum}")]
    FrameTooLarge { actual: usize, maximum: usize },
    #[error("frame has an invalid length prefix")]
    InvalidLengthPrefix,
    #[error("could not serialize protocol message: {0}")]
    Serialize(postcard::Error),
    #[error("could not deserialize protocol message: {0}")]
    Deserialize(postcard::Error),
    #[error(transparent)]
    InvalidMessage(#[from] MessageValidationError),
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroUsize};

    use bytes::{BufMut, BytesMut};
    use domain::NodeId;

    use super::{CodecError, FrameCodec};
    use crate::{
        CURRENT_PROTOCOL, Capabilities, DEFAULT_MAX_FRAME_BYTES, Envelope, Handshake, Hello,
        HostPlatform,
    };

    fn hello() -> Envelope {
        let maximum_frame_bytes = NonZeroU32::new(DEFAULT_MAX_FRAME_BYTES as u32)
            .unwrap_or_else(|| panic!("default maximum must be non-zero"));
        let node =
            NodeId::new("left-desk").unwrap_or_else(|error| panic!("invalid test node: {error}"));
        Envelope::Handshake(Handshake::Hello(Hello {
            version: CURRENT_PROTOCOL,
            node,
            nonce: [7; 32],
            platform: HostPlatform::LinuxWayland,
            capabilities: Capabilities {
                keyboard: true,
                relative_pointer: true,
                absolute_pointer: false,
                clipboard_text: true,
            },
            maximum_frame_bytes,
        }))
    }

    #[test]
    fn round_trips_one_message() {
        let codec = FrameCodec::default();
        let mut frame = codec
            .encode(&hello())
            .unwrap_or_else(|error| panic!("encoding failed: {error}"));

        let decoded = codec
            .decode(&mut frame)
            .unwrap_or_else(|error| panic!("decoding failed: {error}"));

        assert_eq!(decoded, Some(hello()));
        assert!(frame.is_empty());
    }

    #[test]
    fn waits_for_a_complete_payload() {
        let codec = FrameCodec::default();
        let complete = codec
            .encode(&hello())
            .unwrap_or_else(|error| panic!("encoding failed: {error}"));
        let split = complete.len() - 1;
        let mut partial = BytesMut::from(&complete[..split]);

        assert_eq!(
            codec
                .decode(&mut partial)
                .unwrap_or_else(|error| panic!("decoding failed: {error}")),
            None
        );
        partial.extend_from_slice(&complete[split..]);
        assert_eq!(
            codec
                .decode(&mut partial)
                .unwrap_or_else(|error| panic!("decoding failed: {error}")),
            Some(hello())
        );
    }

    #[test]
    fn rejects_an_oversized_declared_payload_before_allocation() {
        let maximum = NonZeroUsize::new(32)
            .map(NonZeroUsize::get)
            .unwrap_or_else(|| panic!("test maximum must be non-zero"));
        let codec =
            FrameCodec::new(maximum).unwrap_or_else(|error| panic!("invalid codec: {error}"));
        let mut source = BytesMut::new();
        source.put_u32(33);

        assert!(matches!(
            codec.decode(&mut source),
            Err(CodecError::FrameTooLarge {
                actual: 33,
                maximum: 32
            })
        ));
    }
}
