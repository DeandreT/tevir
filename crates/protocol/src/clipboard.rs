use std::num::NonZeroU64;

use domain::NodeId;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_CLIPBOARD_FRAME_BYTES: usize = MAX_CLIPBOARD_TEXT_BYTES + 256;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ClipboardGeneration {
    pub owner: NodeId,
    pub sequence: NonZeroU64,
}

impl ClipboardGeneration {
    #[must_use]
    pub const fn new(owner: NodeId, sequence: NonZeroU64) -> Self {
        Self { owner, sequence }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClipboardOffer {
    generation: ClipboardGeneration,
    text_bytes: u32,
    digest: [u8; 32],
}

impl ClipboardOffer {
    #[must_use]
    pub fn generation(&self) -> &ClipboardGeneration {
        &self.generation
    }

    #[must_use]
    pub const fn text_bytes(&self) -> u32 {
        self.text_bytes
    }

    pub fn verify(&self, transfer: &ClipboardText) -> Result<(), ClipboardError> {
        if self.generation != transfer.generation {
            return Err(ClipboardError::GenerationMismatch);
        }
        let actual_bytes = transfer.text.len();
        if self.text_bytes as usize != actual_bytes {
            return Err(ClipboardError::LengthMismatch {
                offered: self.text_bytes as usize,
                actual: actual_bytes,
            });
        }
        if self.digest != text_digest(&transfer.text) {
            return Err(ClipboardError::DigestMismatch);
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<(), ClipboardError> {
        validate_text_length(self.text_bytes as usize)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClipboardText {
    generation: ClipboardGeneration,
    text: String,
}

impl ClipboardText {
    pub fn new(
        generation: ClipboardGeneration,
        text: impl Into<String>,
    ) -> Result<Self, ClipboardError> {
        let text = text.into();
        validate_text_length(text.len())?;
        Ok(Self { generation, text })
    }

    #[must_use]
    pub fn generation(&self) -> &ClipboardGeneration {
        &self.generation
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }

    #[must_use]
    pub fn offer(&self) -> ClipboardOffer {
        ClipboardOffer {
            generation: self.generation.clone(),
            text_bytes: u32::try_from(self.text.len()).unwrap_or(u32::MAX),
            digest: text_digest(&self.text),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, ClipboardError> {
        validate_text_length(self.text.len())?;
        let encoded = postcard::to_stdvec(self).map_err(ClipboardError::Serialize)?;
        if encoded.len() > MAX_CLIPBOARD_FRAME_BYTES {
            return Err(ClipboardError::FrameTooLarge {
                actual: encoded.len(),
                maximum: MAX_CLIPBOARD_FRAME_BYTES,
            });
        }
        Ok(encoded)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, ClipboardError> {
        if encoded.len() > MAX_CLIPBOARD_FRAME_BYTES {
            return Err(ClipboardError::FrameTooLarge {
                actual: encoded.len(),
                maximum: MAX_CLIPBOARD_FRAME_BYTES,
            });
        }
        let transfer =
            postcard::from_bytes::<Self>(encoded).map_err(ClipboardError::Deserialize)?;
        validate_text_length(transfer.text.len())?;
        Ok(transfer)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardControl {
    Offered(ClipboardOffer),
    Applied { generation: ClipboardGeneration },
}

fn validate_text_length(actual: usize) -> Result<(), ClipboardError> {
    if actual > MAX_CLIPBOARD_TEXT_BYTES {
        return Err(ClipboardError::TextTooLarge {
            actual,
            maximum: MAX_CLIPBOARD_TEXT_BYTES,
        });
    }
    Ok(())
}

fn text_digest(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ClipboardError {
    #[error("clipboard text is {actual} bytes; the maximum is {maximum}")]
    TextTooLarge { actual: usize, maximum: usize },
    #[error("clipboard frame is {actual} bytes; the maximum is {maximum}")]
    FrameTooLarge { actual: usize, maximum: usize },
    #[error("could not serialize clipboard text: {0}")]
    Serialize(postcard::Error),
    #[error("could not deserialize clipboard text: {0}")]
    Deserialize(postcard::Error),
    #[error("clipboard transfer generation does not match its offer")]
    GenerationMismatch,
    #[error("clipboard transfer is {actual} bytes, but its offer declared {offered}")]
    LengthMismatch { offered: usize, actual: usize },
    #[error("clipboard transfer digest does not match its offer")]
    DigestMismatch,
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use domain::NodeId;

    use super::{
        ClipboardError, ClipboardGeneration, ClipboardText, MAX_CLIPBOARD_FRAME_BYTES,
        MAX_CLIPBOARD_TEXT_BYTES,
    };

    fn generation(owner: &str, sequence: u64) -> ClipboardGeneration {
        ClipboardGeneration::new(
            NodeId::new(owner).unwrap_or_else(|error| panic!("invalid node: {error}")),
            NonZeroU64::new(sequence).unwrap_or(NonZeroU64::MIN),
        )
    }

    #[test]
    fn text_transfer_round_trips_and_matches_its_offer() {
        let transfer = ClipboardText::new(generation("left", 7), "hello \u{1f30d}")
            .unwrap_or_else(|error| panic!("transfer failed: {error}"));
        let offer = transfer.offer();
        let encoded = transfer
            .encode()
            .unwrap_or_else(|error| panic!("encode failed: {error}"));
        let decoded = ClipboardText::decode(&encoded)
            .unwrap_or_else(|error| panic!("decode failed: {error}"));

        assert_eq!(decoded, transfer);
        assert_eq!(offer.text_bytes(), 10);
        assert_eq!(offer.verify(&decoded), Ok(()));
    }

    #[test]
    fn rejects_text_over_the_explicit_limit() {
        let text = "x".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1);

        assert_eq!(
            ClipboardText::new(generation("left", 1), text),
            Err(ClipboardError::TextTooLarge {
                actual: MAX_CLIPBOARD_TEXT_BYTES + 1,
                maximum: MAX_CLIPBOARD_TEXT_BYTES,
            })
        );
    }

    #[test]
    fn exact_text_limit_fits_the_clipboard_frame() {
        let transfer =
            ClipboardText::new(generation("left", 1), "x".repeat(MAX_CLIPBOARD_TEXT_BYTES))
                .unwrap_or_else(|error| panic!("transfer failed: {error}"));
        let encoded = transfer
            .encode()
            .unwrap_or_else(|error| panic!("encode failed: {error}"));

        assert!(encoded.len() <= MAX_CLIPBOARD_FRAME_BYTES);
        assert_eq!(
            ClipboardText::decode(&encoded)
                .unwrap_or_else(|error| panic!("decode failed: {error}")),
            transfer
        );
    }

    #[test]
    fn rejects_a_frame_before_deserializing_it() {
        let encoded = vec![0; MAX_CLIPBOARD_FRAME_BYTES + 1];

        assert_eq!(
            ClipboardText::decode(&encoded),
            Err(ClipboardError::FrameTooLarge {
                actual: MAX_CLIPBOARD_FRAME_BYTES + 1,
                maximum: MAX_CLIPBOARD_FRAME_BYTES,
            })
        );
    }

    #[test]
    fn offer_detects_the_wrong_generation() {
        let offered = ClipboardText::new(generation("left", 1), "hello")
            .unwrap_or_else(|error| panic!("transfer failed: {error}"));
        let received = ClipboardText::new(generation("right", 1), "hello")
            .unwrap_or_else(|error| panic!("transfer failed: {error}"));

        assert_eq!(
            offered.offer().verify(&received),
            Err(ClipboardError::GenerationMismatch)
        );
    }

    #[test]
    fn offer_detects_modified_text() {
        let offered = ClipboardText::new(generation("left", 1), "hello")
            .unwrap_or_else(|error| panic!("transfer failed: {error}"));
        let received = ClipboardText::new(generation("left", 1), "jello")
            .unwrap_or_else(|error| panic!("transfer failed: {error}"));

        assert_eq!(
            offered.offer().verify(&received),
            Err(ClipboardError::DigestMismatch)
        );
    }
}
