use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_NODE_ID_BYTES: usize = 63;

/// A stable, configuration-facing identifier for a Tevir node.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeId(String);

impl NodeId {
    /// Creates an identifier using a lowercase DNS-label-like syntax.
    pub fn new(value: impl Into<String>) -> Result<Self, NodeIdError> {
        let value = value.into();

        if value.is_empty() {
            return Err(NodeIdError::Empty);
        }
        if value.len() > MAX_NODE_ID_BYTES {
            return Err(NodeIdError::TooLong {
                actual: value.len(),
                maximum: MAX_NODE_ID_BYTES,
            });
        }

        let bytes = value.as_bytes();
        if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
            return Err(NodeIdError::InvalidBoundary);
        }
        if let Some((index, character)) = bytes.iter().copied().enumerate().find(|(_, byte)| {
            !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        }) {
            return Err(NodeIdError::InvalidCharacter { index, character });
        }

        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for NodeId {
    type Error = NodeIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<NodeId> for String {
    fn from(value: NodeId) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum NodeIdError {
    #[error("node ID cannot be empty")]
    Empty,
    #[error("node ID is {actual} bytes; the maximum is {maximum}")]
    TooLong { actual: usize, maximum: usize },
    #[error("node ID must start and end with an ASCII letter or digit")]
    InvalidBoundary,
    #[error("node ID contains invalid byte {character:#04x} at index {index}")]
    InvalidCharacter { index: usize, character: u8 },
}

#[cfg(test)]
mod tests {
    use super::{NodeId, NodeIdError};

    #[test]
    fn accepts_dns_label_style_ids() {
        let node = NodeId::new("workstation-2");

        assert_eq!(node.as_ref().map(NodeId::as_str), Ok("workstation-2"));
    }

    #[test]
    fn rejects_uppercase_and_invalid_boundaries() {
        assert!(matches!(
            NodeId::new("-left"),
            Err(NodeIdError::InvalidBoundary)
        ));
        assert!(matches!(
            NodeId::new("Desk"),
            Err(NodeIdError::InvalidCharacter { index: 0, .. })
        ));
    }
}
