use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use domain::NodeId;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::pairing::{PairingBundle, certificate_fingerprint};

const CREDENTIAL_VERSION: u8 = 1;
const TRUST_VERSION: u8 = 1;
const MAX_TRUSTED_PEERS: usize = 256;
const CREDENTIAL_FILE: &str = "identity.toml";
const PEER_DIRECTORY: &str = "peers";

#[derive(Clone)]
pub struct LocalIdentity {
    node: NodeId,
    trust_anchor: Vec<u8>,
    certificate: Vec<u8>,
    private_key: Vec<u8>,
}

impl LocalIdentity {
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    #[must_use]
    pub fn pairing_bundle(&self) -> PairingBundle {
        PairingBundle::new(self.node.clone(), self.trust_anchor.clone())
    }

    #[must_use]
    pub fn certificate_chain(&self) -> [&[u8]; 2] {
        [&self.certificate, &self.trust_anchor]
    }

    #[must_use]
    pub fn private_key_der(&self) -> &[u8] {
        &self.private_key
    }
}

pub struct IdentityStore {
    root: PathBuf,
}

impl IdentityStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn load_or_create(&self, node: &NodeId) -> Result<LocalIdentity, IdentityError> {
        create_private_directory(&self.root)?;
        let path = self.root.join(CREDENTIAL_FILE);
        match fs::read_to_string(&path) {
            Ok(contents) => parse_identity(&contents, node),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let identity = generate_identity(node)?;
                let contents = encode_identity(&identity)?;
                match write_new_private_file(&path, contents.as_bytes()) {
                    Ok(()) => Ok(identity),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        let contents =
                            fs::read_to_string(&path).map_err(IdentityError::ReadCredential)?;
                        parse_identity(&contents, node)
                    }
                    Err(error) => Err(IdentityError::WriteCredential(error)),
                }
            }
            Err(error) => Err(IdentityError::ReadCredential(error)),
        }
    }

    pub fn trust_store(&self) -> Result<TrustStore, TrustError> {
        TrustStore::open(self.root.join(PEER_DIRECTORY))
    }
}

pub struct TrustStore {
    directory: PathBuf,
    peers: BTreeMap<NodeId, TrustedPeer>,
}

impl TrustStore {
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self, TrustError> {
        let directory = directory.into();
        create_private_directory(&directory)?;
        let mut peers = BTreeMap::new();

        for entry in fs::read_dir(&directory).map_err(TrustError::ReadDirectory)? {
            let entry = entry.map_err(TrustError::ReadDirectory)?;
            if entry
                .file_type()
                .map_err(TrustError::ReadDirectory)?
                .is_dir()
            {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("toml") {
                continue;
            }
            let contents = fs::read_to_string(&path).map_err(|source| TrustError::ReadPeer {
                path: path.clone(),
                source,
            })?;
            let peer = parse_peer(&contents).map_err(|source| TrustError::ParsePeer {
                path: path.clone(),
                source,
            })?;
            if peers.insert(peer.node.clone(), peer).is_some() {
                return Err(TrustError::DuplicateNodeFile(path));
            }
            if peers.len() > MAX_TRUSTED_PEERS {
                return Err(TrustError::TooManyPeers {
                    maximum: MAX_TRUSTED_PEERS,
                });
            }
        }

        ensure_unique_certificates(&peers)?;
        Ok(Self { directory, peers })
    }

    pub fn trust(&mut self, bundle: PairingBundle, displayed_code: &str) -> Result<(), TrustError> {
        if !bundle.code().matches(displayed_code) {
            return Err(TrustError::PairingCodeMismatch);
        }
        if self.peers.len() >= MAX_TRUSTED_PEERS && !self.peers.contains_key(bundle.node()) {
            return Err(TrustError::TooManyPeers {
                maximum: MAX_TRUSTED_PEERS,
            });
        }

        let fingerprint = bundle.fingerprint();
        if let Some(peer) = self.peers.get(bundle.node()) {
            if peer.fingerprint == fingerprint {
                return Ok(());
            }
            return Err(TrustError::NodeAlreadyTrusted(bundle.node().clone()));
        }
        if let Some(peer) = self
            .peers
            .values()
            .find(|peer| peer.fingerprint == fingerprint)
        {
            return Err(TrustError::CertificateAlreadyTrusted(peer.node.clone()));
        }

        let peer = TrustedPeer {
            node: bundle.node().clone(),
            trust_anchor: bundle.trust_anchor().to_vec(),
            fingerprint,
        };
        let contents = encode_peer(&peer)?;
        let path = self.peer_path(&peer.node);
        write_new_private_file(&path, contents.as_bytes()).map_err(|source| {
            TrustError::WritePeer {
                path: path.clone(),
                source,
            }
        })?;
        self.peers.insert(peer.node.clone(), peer);
        Ok(())
    }

    pub fn remove(&mut self, node: &NodeId) -> Result<bool, TrustError> {
        if self.peers.remove(node).is_none() {
            return Ok(false);
        }
        let path = self.peer_path(node);
        fs::remove_file(&path).map_err(|source| TrustError::RemovePeer { path, source })?;
        Ok(true)
    }

    #[must_use]
    pub fn peers(&self) -> impl ExactSizeIterator<Item = &TrustedPeer> {
        self.peers.values()
    }

    #[must_use]
    pub fn get(&self, node: &NodeId) -> Option<&TrustedPeer> {
        self.peers.get(node)
    }

    fn peer_path(&self, node: &NodeId) -> PathBuf {
        self.directory.join(format!("{node}.toml"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedPeer {
    node: NodeId,
    trust_anchor: Vec<u8>,
    fingerprint: [u8; 32],
}

impl TrustedPeer {
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    #[must_use]
    pub fn trust_anchor_der(&self) -> &[u8] {
        &self.trust_anchor
    }

    #[must_use]
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CredentialFile {
    version: u8,
    node: NodeId,
    trust_anchor: String,
    certificate: String,
    private_key: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PeerFile {
    version: u8,
    node: NodeId,
    trust_anchor: String,
}

fn generate_identity(node: &NodeId) -> Result<LocalIdentity, IdentityError> {
    let mut ca_params = CertificateParams::new(Vec::new())?;
    ca_params
        .distinguished_name
        .push(DnType::CommonName, format!("{node} local trust anchor"));
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let ca_key = KeyPair::generate()?;
    let ca_certificate = ca_params.self_signed(&ca_key)?;
    let issuer = Issuer::new(ca_params, ca_key);

    let mut leaf_params = CertificateParams::new(vec!["tevir.local".to_owned()])?;
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, node.as_str());
    leaf_params.use_authority_key_identifier_extension = true;
    leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf_params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let leaf_key = KeyPair::generate()?;
    let leaf_certificate = leaf_params.signed_by(&leaf_key, &issuer)?;

    Ok(LocalIdentity {
        node: node.clone(),
        trust_anchor: ca_certificate.der().to_vec(),
        certificate: leaf_certificate.der().to_vec(),
        private_key: leaf_key.serialize_der(),
    })
}

fn encode_identity(identity: &LocalIdentity) -> Result<String, IdentityError> {
    toml::to_string_pretty(&CredentialFile {
        version: CREDENTIAL_VERSION,
        node: identity.node.clone(),
        trust_anchor: STANDARD.encode(&identity.trust_anchor),
        certificate: STANDARD.encode(&identity.certificate),
        private_key: STANDARD.encode(&identity.private_key),
    })
    .map_err(IdentityError::EncodeCredential)
}

fn parse_identity(contents: &str, expected_node: &NodeId) -> Result<LocalIdentity, IdentityError> {
    let file: CredentialFile = toml::from_str(contents).map_err(IdentityError::ParseCredential)?;
    if file.version != CREDENTIAL_VERSION {
        return Err(IdentityError::UnsupportedCredentialVersion(file.version));
    }
    if file.node != *expected_node {
        return Err(IdentityError::NodeMismatch {
            expected: expected_node.clone(),
            actual: file.node,
        });
    }
    Ok(LocalIdentity {
        node: expected_node.clone(),
        trust_anchor: decode_material(&file.trust_anchor)?,
        certificate: decode_material(&file.certificate)?,
        private_key: decode_material(&file.private_key)?,
    })
}

fn encode_peer(peer: &TrustedPeer) -> Result<String, TrustError> {
    toml::to_string_pretty(&PeerFile {
        version: TRUST_VERSION,
        node: peer.node.clone(),
        trust_anchor: STANDARD.encode(&peer.trust_anchor),
    })
    .map_err(TrustError::EncodePeer)
}

fn parse_peer(contents: &str) -> Result<TrustedPeer, PeerFileError> {
    let file: PeerFile = toml::from_str(contents)?;
    if file.version != TRUST_VERSION {
        return Err(PeerFileError::UnsupportedVersion(file.version));
    }
    let trust_anchor = STANDARD
        .decode(file.trust_anchor)
        .map_err(|_| PeerFileError::InvalidCertificateEncoding)?;
    if trust_anchor.is_empty() {
        return Err(PeerFileError::EmptyCertificate);
    }
    let fingerprint = certificate_fingerprint(&trust_anchor);
    Ok(TrustedPeer {
        node: file.node,
        trust_anchor,
        fingerprint,
    })
}

fn decode_material(encoded: &str) -> Result<Vec<u8>, IdentityError> {
    let material = STANDARD
        .decode(encoded)
        .map_err(|_| IdentityError::InvalidCredentialEncoding)?;
    if material.is_empty() {
        return Err(IdentityError::EmptyCredentialMaterial);
    }
    Ok(material)
}

fn ensure_unique_certificates(peers: &BTreeMap<NodeId, TrustedPeer>) -> Result<(), TrustError> {
    let mut fingerprints = BTreeMap::new();
    for peer in peers.values() {
        if let Some(previous) = fingerprints.insert(peer.fingerprint, peer.node.clone()) {
            return Err(TrustError::CertificateAlreadyTrusted(previous));
        }
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_private_directory_permissions(path)
}

fn write_new_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_mode(&mut options);
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_file_mode(_options: &mut OpenOptions) {}

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("could not create the identity directory: {0}")]
    CreateDirectory(#[from] io::Error),
    #[error("could not read the identity credential: {0}")]
    ReadCredential(io::Error),
    #[error("could not write the identity credential: {0}")]
    WriteCredential(io::Error),
    #[error("could not generate identity material: {0}")]
    GenerateCertificate(#[from] rcgen::Error),
    #[error("could not encode the identity credential: {0}")]
    EncodeCredential(toml::ser::Error),
    #[error("could not parse the identity credential: {0}")]
    ParseCredential(toml::de::Error),
    #[error("identity credential version {0} is not supported")]
    UnsupportedCredentialVersion(u8),
    #[error("identity credential belongs to `{actual}`, not `{expected}`")]
    NodeMismatch { expected: NodeId, actual: NodeId },
    #[error("identity credential contains invalid base64")]
    InvalidCredentialEncoding,
    #[error("identity credential contains empty key or certificate material")]
    EmptyCredentialMaterial,
}

#[derive(Debug, Error)]
pub enum TrustError {
    #[error("could not access the peer directory: {0}")]
    ReadDirectory(#[from] io::Error),
    #[error("could not read trusted peer `{}`: {source}", path.display())]
    ReadPeer {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse trusted peer `{}`: {source}", path.display())]
    ParsePeer {
        path: PathBuf,
        #[source]
        source: PeerFileError,
    },
    #[error("could not encode trusted peer: {0}")]
    EncodePeer(toml::ser::Error),
    #[error("could not write trusted peer `{}`: {source}", path.display())]
    WritePeer {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not remove trusted peer `{}`: {source}", path.display())]
    RemovePeer {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("trusted peer file duplicates node identity: {}", .0.display())]
    DuplicateNodeFile(PathBuf),
    #[error("the pairing code does not match the pairing bundle")]
    PairingCodeMismatch,
    #[error("node `{0}` is already trusted with a different certificate")]
    NodeAlreadyTrusted(NodeId),
    #[error("the certificate is already trusted for node `{0}`")]
    CertificateAlreadyTrusted(NodeId),
    #[error("the trust store allows at most {maximum} peers")]
    TooManyPeers { maximum: usize },
}

#[derive(Debug, Error)]
pub enum PeerFileError {
    #[error("trusted peer data is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("trusted peer version {0} is not supported")]
    UnsupportedVersion(u8),
    #[error("trusted peer certificate is not valid base64")]
    InvalidCertificateEncoding,
    #[error("trusted peer certificate is empty")]
    EmptyCertificate,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use domain::NodeId;
    use tempfile::TempDir;

    use super::{IdentityError, IdentityStore, TrustError};

    fn node(value: &str) -> NodeId {
        NodeId::new(value).unwrap_or_else(|error| panic!("invalid test node: {error}"))
    }

    #[test]
    fn identity_is_stable_across_loads() {
        let directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let store = IdentityStore::new(directory.path());
        let first = store
            .load_or_create(&node("left"))
            .unwrap_or_else(|error| panic!("identity creation failed: {error}"));
        let second = store
            .load_or_create(&node("left"))
            .unwrap_or_else(|error| panic!("identity load failed: {error}"));

        assert_eq!(
            first.pairing_bundle().fingerprint(),
            second.pairing_bundle().fingerprint()
        );
        assert_eq!(first.private_key_der(), second.private_key_der());
    }

    #[test]
    fn rejects_identity_reuse_for_another_node() {
        let directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let store = IdentityStore::new(directory.path());
        store
            .load_or_create(&node("left"))
            .unwrap_or_else(|error| panic!("identity creation failed: {error}"));

        assert!(matches!(
            store.load_or_create(&node("right")),
            Err(IdentityError::NodeMismatch { .. })
        ));
    }

    #[test]
    fn trust_requires_matching_out_of_band_code() {
        let left_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let right_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let left_store = IdentityStore::new(left_directory.path());
        let right = IdentityStore::new(right_directory.path())
            .load_or_create(&node("right"))
            .unwrap_or_else(|error| panic!("identity creation failed: {error}"));
        let mut trust = left_store
            .trust_store()
            .unwrap_or_else(|error| panic!("trust store failed: {error}"));

        assert!(matches!(
            trust.trust(right.pairing_bundle(), "wrong"),
            Err(TrustError::PairingCodeMismatch)
        ));
        assert_eq!(trust.peers().len(), 0);
    }

    #[test]
    fn trusted_peer_survives_reload_and_can_be_removed() {
        let left_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let right_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let left_store = IdentityStore::new(left_directory.path());
        let right = IdentityStore::new(right_directory.path())
            .load_or_create(&node("right"))
            .unwrap_or_else(|error| panic!("identity creation failed: {error}"));
        let bundle = right.pairing_bundle();
        let code = bundle.code().to_string();
        left_store
            .trust_store()
            .and_then(|mut trust| trust.trust(bundle, &code))
            .unwrap_or_else(|error| panic!("pairing failed: {error}"));

        let mut reloaded = left_store
            .trust_store()
            .unwrap_or_else(|error| panic!("trust reload failed: {error}"));
        assert!(reloaded.get(&node("right")).is_some());
        assert!(matches!(reloaded.remove(&node("right")), Ok(true)));
        assert!(matches!(reloaded.remove(&node("right")), Ok(false)));
    }

    #[cfg(unix)]
    #[test]
    fn credential_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        IdentityStore::new(directory.path())
            .load_or_create(&node("left"))
            .unwrap_or_else(|error| panic!("identity creation failed: {error}"));
        let mode = fs::metadata(directory.path().join("identity.toml"))
            .unwrap_or_else(|error| panic!("metadata failed: {error}"))
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o600);
    }
}
