use std::fmt;

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use domain::NodeId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const BUNDLE_PREFIX: &str = "tevir-pair-v1.";
const BUNDLE_VERSION: u8 = 1;
const MAX_BUNDLE_BYTES: usize = 64 * 1024;
const MAX_CERTIFICATE_BYTES: usize = 16 * 1024;
const CODE_BYTES: usize = 12;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PairingBundle {
    node: NodeId,
    trust_anchor: Vec<u8>,
}

impl PairingBundle {
    pub(crate) fn new(node: NodeId, trust_anchor: Vec<u8>) -> Self {
        Self { node, trust_anchor }
    }

    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    #[must_use]
    pub fn code(&self) -> PairingCode {
        PairingCode::from_certificate(&self.trust_anchor)
    }

    #[must_use]
    pub fn fingerprint(&self) -> [u8; 32] {
        certificate_fingerprint(&self.trust_anchor)
    }

    #[must_use]
    pub fn encode(&self) -> String {
        let payload = PairingPayload {
            version: BUNDLE_VERSION,
            node: self.node.clone(),
            trust_anchor: STANDARD.encode(&self.trust_anchor),
        };
        let encoded = serde_json::to_vec(&payload)
            .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
            .unwrap_or_default();
        format!("{BUNDLE_PREFIX}{encoded}")
    }

    pub fn decode(value: &str) -> Result<Self, PairingError> {
        if value.len() > MAX_BUNDLE_BYTES {
            return Err(PairingError::BundleTooLarge);
        }
        let encoded = value
            .trim()
            .strip_prefix(BUNDLE_PREFIX)
            .ok_or(PairingError::InvalidPrefix)?;
        let payload_bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| PairingError::InvalidEncoding)?;
        if payload_bytes.len() > MAX_BUNDLE_BYTES {
            return Err(PairingError::BundleTooLarge);
        }
        let payload: PairingPayload =
            serde_json::from_slice(&payload_bytes).map_err(PairingError::InvalidPayload)?;
        if payload.version != BUNDLE_VERSION {
            return Err(PairingError::UnsupportedVersion(payload.version));
        }
        let trust_anchor = STANDARD
            .decode(payload.trust_anchor)
            .map_err(|_| PairingError::InvalidCertificateEncoding)?;
        if trust_anchor.is_empty() || trust_anchor.len() > MAX_CERTIFICATE_BYTES {
            return Err(PairingError::InvalidCertificateLength {
                actual: trust_anchor.len(),
                maximum: MAX_CERTIFICATE_BYTES,
            });
        }

        Ok(Self::new(payload.node, trust_anchor))
    }

    pub(crate) fn trust_anchor(&self) -> &[u8] {
        &self.trust_anchor
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PairingCode(String);

impl PairingCode {
    fn from_certificate(certificate: &[u8]) -> Self {
        let digest = certificate_fingerprint(certificate);
        let groups = digest[..CODE_BYTES]
            .chunks_exact(2)
            .map(|chunk| format!("{:02X}{:02X}", chunk[0], chunk[1]))
            .collect::<Vec<_>>();
        Self(groups.join("-"))
    }

    pub fn matches(&self, candidate: &str) -> bool {
        let normalized: String = candidate
            .chars()
            .filter(|character| !character.is_ascii_whitespace() && *character != '-')
            .map(|character| character.to_ascii_uppercase())
            .collect();
        let expected: String = self
            .0
            .chars()
            .filter(|character| *character != '-')
            .collect();
        normalized == expected
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PairingCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PairingPayload {
    version: u8,
    node: NodeId,
    trust_anchor: String,
}

pub(crate) fn certificate_fingerprint(certificate: &[u8]) -> [u8; 32] {
    Sha256::digest(certificate).into()
}

#[derive(Debug, Error)]
pub enum PairingError {
    #[error("pairing bundle has an invalid prefix")]
    InvalidPrefix,
    #[error("pairing bundle exceeds the size limit")]
    BundleTooLarge,
    #[error("pairing bundle is not valid base64")]
    InvalidEncoding,
    #[error("pairing bundle payload is invalid: {0}")]
    InvalidPayload(serde_json::Error),
    #[error("pairing bundle version {0} is not supported")]
    UnsupportedVersion(u8),
    #[error("pairing certificate is not valid base64")]
    InvalidCertificateEncoding,
    #[error("pairing certificate is {actual} bytes; the allowed range is 1..={maximum}")]
    InvalidCertificateLength { actual: usize, maximum: usize },
}

#[cfg(test)]
mod tests {
    use domain::NodeId;

    use super::{PairingBundle, PairingError};

    fn node() -> NodeId {
        NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"))
    }

    #[test]
    fn pairing_bundle_round_trips_without_private_material() {
        let bundle = PairingBundle::new(node(), vec![1, 2, 3, 4]);
        let encoded = bundle.encode();
        let decoded = PairingBundle::decode(&encoded)
            .unwrap_or_else(|error| panic!("bundle should decode: {error}"));

        assert_eq!(decoded, bundle);
        assert!(!encoded.contains("PRIVATE"));
    }

    #[test]
    fn pairing_code_accepts_readable_variants() {
        let bundle = PairingBundle::new(node(), vec![9, 8, 7, 6]);
        let code = bundle.code();
        let compact = code.as_str().replace('-', "").to_ascii_lowercase();

        assert!(code.matches(&compact));
        assert!(!code.matches("0000-0000-0000-0000-0000-0000"));
    }

    #[test]
    fn rejects_unknown_bundle_versions() {
        let encoded = PairingBundle::new(node(), vec![1]).encode();
        let replacement = encoded.replacen("v1", "v2", 1);

        assert!(matches!(
            PairingBundle::decode(&replacement),
            Err(PairingError::InvalidPrefix)
        ));
    }
}
