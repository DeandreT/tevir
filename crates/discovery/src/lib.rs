use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
};

use domain::{HostPlatform, NodeId};
use identity::PairingBundle;
use mdns_sd::{DaemonEvent, Receiver, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use protocol::{CURRENT_PROTOCOL, Capabilities};
use thiserror::Error;

const SERVICE_TYPE: &str = "_tevir-pair._udp.local.";
const BUNDLE_CHUNK_BYTES: usize = 220;
const MAX_BUNDLE_CHUNKS: usize = 8;
const MAX_NEARBY_NODES: usize = 64;

const PROPERTY_NODE: &str = "node";
const PROPERTY_PROTOCOL: &str = "protocol";
const PROPERTY_PLATFORM: &str = "platform";
const PROPERTY_CAPABILITIES: &str = "capabilities";
const PROPERTY_FINGERPRINT: &str = "fingerprint";
const PROPERTY_BUNDLE_CHUNKS: &str = "bundle-chunks";
const PROPERTY_BUNDLE_PREFIX: &str = "bundle-";

pub struct DiscoveryService {
    daemon: ServiceDaemon,
    events: Receiver<ServiceEvent>,
    monitor: Receiver<DaemonEvent>,
    fullname: String,
    local_node: NodeId,
}

impl DiscoveryService {
    pub fn start(
        bundle: PairingBundle,
        platform: HostPlatform,
        capabilities: Capabilities,
    ) -> Result<Self, DiscoveryError> {
        let local_node = bundle.node().clone();
        let service = advertisement(&bundle, platform, capabilities, "")?;
        let fullname = service.get_fullname().to_owned();
        let daemon = ServiceDaemon::new().map_err(DiscoveryError::daemon)?;
        let monitor = daemon.monitor().map_err(DiscoveryError::daemon)?;
        let events = daemon
            .browse(SERVICE_TYPE)
            .map_err(DiscoveryError::daemon)?;
        daemon.register(service).map_err(DiscoveryError::daemon)?;

        tracing::info!(
            node = %local_node,
            service_type = SERVICE_TYPE,
            "local network discovery started"
        );
        Ok(Self {
            daemon,
            events,
            monitor,
            fullname,
            local_node,
        })
    }

    pub fn poll(&self, nearby: &mut NearbyNodes) -> DiscoveryPoll {
        let mut result = DiscoveryPoll::default();

        while let Ok(event) = self.events.try_recv() {
            match event {
                ServiceEvent::ServiceResolved(service) => {
                    if service.get_fullname() == self.fullname {
                        continue;
                    }
                    match DiscoveredNode::from_resolved(&service) {
                        Ok(node) if node.node() != &self.local_node => {
                            let node_id = node.node().clone();
                            let address_count = node.addresses().len();
                            match nearby.upsert(node) {
                                RegistryUpdate::Added => {
                                    tracing::info!(
                                        node = %node_id,
                                        address_count,
                                        "nearby node discovered"
                                    );
                                    result.changed = true;
                                }
                                RegistryUpdate::Refreshed => {
                                    result.changed = true;
                                }
                                RegistryUpdate::Unchanged => {}
                                RegistryUpdate::Full => {
                                    tracing::warn!(
                                        node = %node_id,
                                        maximum = MAX_NEARBY_NODES,
                                        "nearby node limit reached"
                                    );
                                    result.error = Some(format!(
                                        "Nearby node limit reached ({MAX_NEARBY_NODES})"
                                    ));
                                }
                                RegistryUpdate::IdentityConflict => {
                                    tracing::warn!(
                                        node = %node_id,
                                        "conflicting discovery identity rejected"
                                    );
                                    result.error = Some(format!(
                                        "Conflicting identity advertised for {node_id}"
                                    ));
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(
                                service = service.get_fullname(),
                                error = %error,
                                "invalid discovery record rejected"
                            );
                        }
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    if let Some(node) = nearby.remove_fullname(&fullname) {
                        tracing::info!(node = %node, "nearby node departed");
                        result.changed = true;
                    }
                }
                ServiceEvent::SearchStarted(_)
                | ServiceEvent::ServiceFound(_, _)
                | ServiceEvent::SearchStopped(_) => {}
                _ => {}
            }
        }

        while let Ok(event) = self.monitor.try_recv() {
            if let DaemonEvent::Error(error) = event {
                tracing::warn!(error = %error, "local network discovery error");
                result.error = Some(error.to_string());
            }
        }

        result
    }
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        if let Err(error) = self.daemon.stop_browse(SERVICE_TYPE) {
            tracing::debug!(error = %error, "could not stop discovery browser");
        }
        if let Err(error) = self.daemon.unregister(&self.fullname) {
            tracing::debug!(error = %error, "could not unregister discovery service");
        }
        if let Err(error) = self.daemon.shutdown() {
            tracing::debug!(error = %error, "could not stop discovery daemon");
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredNode {
    fullname: String,
    node: NodeId,
    platform: HostPlatform,
    capabilities: Capabilities,
    addresses: BTreeSet<IpAddr>,
    pairing_bundle: PairingBundle,
}

impl DiscoveredNode {
    fn from_resolved(service: &ResolvedService) -> Result<Self, RecordError> {
        let property = |key| {
            service
                .get_property_val_str(key)
                .filter(|value| !value.is_empty())
                .ok_or(RecordError::MissingProperty(key))
        };

        let node = NodeId::new(property(PROPERTY_NODE)?)
            .map_err(|error| RecordError::InvalidNode(error.to_string()))?;
        verify_protocol(property(PROPERTY_PROTOCOL)?)?;
        let platform = parse_platform(property(PROPERTY_PLATFORM)?)?;
        let capabilities = parse_capabilities(property(PROPERTY_CAPABILITIES)?)?;
        let advertised_fingerprint = parse_fingerprint(property(PROPERTY_FINGERPRINT)?)?;
        let chunk_count = property(PROPERTY_BUNDLE_CHUNKS)?
            .parse::<usize>()
            .map_err(|_| RecordError::InvalidBundleChunkCount)?;
        if chunk_count == 0 || chunk_count > MAX_BUNDLE_CHUNKS {
            return Err(RecordError::InvalidBundleChunkCount);
        }

        let mut encoded_bundle = String::new();
        for index in 0..chunk_count {
            let key = format!("{PROPERTY_BUNDLE_PREFIX}{index}");
            let chunk = service
                .get_property_val_str(&key)
                .filter(|value| !value.is_empty())
                .ok_or(RecordError::MissingBundleChunk(index))?;
            if chunk.len() > BUNDLE_CHUNK_BYTES {
                return Err(RecordError::BundleChunkTooLarge(index));
            }
            encoded_bundle.push_str(chunk);
        }
        let pairing_bundle = PairingBundle::decode(&encoded_bundle)
            .map_err(|error| RecordError::InvalidPairingBundle(error.to_string()))?;
        if pairing_bundle.node() != &node {
            return Err(RecordError::BundleNodeMismatch);
        }
        if pairing_bundle.fingerprint() != advertised_fingerprint {
            return Err(RecordError::FingerprintMismatch);
        }

        let addresses = service
            .get_addresses()
            .iter()
            .map(mdns_sd::ScopedIp::to_ip_addr)
            .filter(|address| !address.is_loopback() && !address.is_unspecified())
            .collect();

        Ok(Self {
            fullname: service.get_fullname().to_owned(),
            node,
            platform,
            capabilities,
            addresses,
            pairing_bundle,
        })
    }

    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    #[must_use]
    pub const fn platform(&self) -> HostPlatform {
        self.platform
    }

    #[must_use]
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    #[must_use]
    pub fn addresses(&self) -> &BTreeSet<IpAddr> {
        &self.addresses
    }

    #[must_use]
    pub fn fingerprint(&self) -> [u8; 32] {
        self.pairing_bundle.fingerprint()
    }

    #[must_use]
    pub fn pairing_bundle(&self) -> &PairingBundle {
        &self.pairing_bundle
    }
}

#[derive(Default)]
pub struct NearbyNodes {
    entries: BTreeMap<NodeId, DiscoveredNode>,
}

impl NearbyNodes {
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &DiscoveredNode> {
        self.entries.values()
    }

    fn upsert(&mut self, node: DiscoveredNode) -> RegistryUpdate {
        if let Some(current) = self.entries.get_mut(node.node()) {
            if current.fingerprint() != node.fingerprint() {
                return RegistryUpdate::IdentityConflict;
            }
            let changed = current != &node;
            *current = node;
            return if changed {
                RegistryUpdate::Refreshed
            } else {
                RegistryUpdate::Unchanged
            };
        }
        if self.entries.len() >= MAX_NEARBY_NODES {
            return RegistryUpdate::Full;
        }
        self.entries.insert(node.node().clone(), node);
        RegistryUpdate::Added
    }

    fn remove_fullname(&mut self, fullname: &str) -> Option<NodeId> {
        let node = self
            .entries
            .iter()
            .find_map(|(node, entry)| (entry.fullname == fullname).then(|| node.clone()))?;
        self.entries.remove(&node);
        Some(node)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryPoll {
    pub changed: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistryUpdate {
    Added,
    Refreshed,
    Unchanged,
    Full,
    IdentityConflict,
}

fn advertisement(
    bundle: &PairingBundle,
    platform: HostPlatform,
    capabilities: Capabilities,
    addresses: impl mdns_sd::AsIpAddrs,
) -> Result<ServiceInfo, DiscoveryError> {
    let encoded_bundle = bundle.encode();
    let chunks = encoded_bundle
        .as_bytes()
        .chunks(BUNDLE_CHUNK_BYTES)
        .collect::<Vec<_>>();
    if chunks.is_empty() || chunks.len() > MAX_BUNDLE_CHUNKS {
        return Err(DiscoveryError::PairingBundleTooLarge {
            actual: encoded_bundle.len(),
            maximum: BUNDLE_CHUNK_BYTES * MAX_BUNDLE_CHUNKS,
        });
    }

    let mut properties = vec![
        (PROPERTY_NODE.to_owned(), bundle.node().to_string()),
        (
            PROPERTY_PROTOCOL.to_owned(),
            format!("{}.{}", CURRENT_PROTOCOL.major, CURRENT_PROTOCOL.minor),
        ),
        (
            PROPERTY_PLATFORM.to_owned(),
            platform_name(platform).to_owned(),
        ),
        (
            PROPERTY_CAPABILITIES.to_owned(),
            capability_bits(capabilities).to_string(),
        ),
        (
            PROPERTY_FINGERPRINT.to_owned(),
            encode_fingerprint(bundle.fingerprint()),
        ),
        (PROPERTY_BUNDLE_CHUNKS.to_owned(), chunks.len().to_string()),
    ];
    for (index, chunk) in chunks.into_iter().enumerate() {
        let value =
            std::str::from_utf8(chunk).map_err(|_| DiscoveryError::InvalidPairingBundleEncoding)?;
        properties.push((format!("{PROPERTY_BUNDLE_PREFIX}{index}"), value.to_owned()));
    }

    ServiceInfo::new(
        SERVICE_TYPE,
        bundle.node().as_str(),
        &format!("{}.local.", bundle.node()),
        addresses,
        0,
        &properties[..],
    )
    .map(ServiceInfo::enable_addr_auto)
    .map_err(DiscoveryError::daemon)
}

fn verify_protocol(value: &str) -> Result<(), RecordError> {
    let expected = format!("{}.{}", CURRENT_PROTOCOL.major, CURRENT_PROTOCOL.minor);
    if value == expected {
        Ok(())
    } else {
        Err(RecordError::UnsupportedProtocol(value.to_owned()))
    }
}

const fn platform_name(platform: HostPlatform) -> &'static str {
    match platform {
        HostPlatform::LinuxWayland => "linux-wayland",
        HostPlatform::Windows => "windows",
    }
}

fn parse_platform(value: &str) -> Result<HostPlatform, RecordError> {
    match value {
        "linux-wayland" => Ok(HostPlatform::LinuxWayland),
        "windows" => Ok(HostPlatform::Windows),
        _ => Err(RecordError::UnsupportedPlatform(value.to_owned())),
    }
}

fn capability_bits(capabilities: Capabilities) -> u8 {
    u8::from(capabilities.keyboard)
        | (u8::from(capabilities.relative_pointer) << 1)
        | (u8::from(capabilities.absolute_pointer) << 2)
        | (u8::from(capabilities.clipboard_text) << 3)
}

fn parse_capabilities(value: &str) -> Result<Capabilities, RecordError> {
    let bits = value
        .parse::<u8>()
        .map_err(|_| RecordError::InvalidCapabilities)?;
    if bits & !0b1111 != 0 {
        return Err(RecordError::InvalidCapabilities);
    }
    Ok(Capabilities {
        keyboard: bits & 0b0001 != 0,
        relative_pointer: bits & 0b0010 != 0,
        absolute_pointer: bits & 0b0100 != 0,
        clipboard_text: bits & 0b1000 != 0,
    })
}

fn encode_fingerprint(fingerprint: [u8; 32]) -> String {
    fingerprint
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn parse_fingerprint(value: &str) -> Result<[u8; 32], RecordError> {
    if value.len() != 64 || !value.is_ascii() {
        return Err(RecordError::InvalidFingerprint);
    }
    let mut fingerprint = [0; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let encoded = std::str::from_utf8(chunk).map_err(|_| RecordError::InvalidFingerprint)?;
        fingerprint[index] =
            u8::from_str_radix(encoded, 16).map_err(|_| RecordError::InvalidFingerprint)?;
    }
    Ok(fingerprint)
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("discovery service failed: {0}")]
    Daemon(String),
    #[error("pairing bundle is {actual} bytes; discovery allows at most {maximum}")]
    PairingBundleTooLarge { actual: usize, maximum: usize },
    #[error("pairing bundle contains invalid text encoding")]
    InvalidPairingBundleEncoding,
}

impl DiscoveryError {
    fn daemon(error: impl std::fmt::Display) -> Self {
        Self::Daemon(error.to_string())
    }
}

#[derive(Debug, Error)]
enum RecordError {
    #[error("missing `{0}` property")]
    MissingProperty(&'static str),
    #[error("node ID is invalid: {0}")]
    InvalidNode(String),
    #[error("protocol `{0}` is not supported")]
    UnsupportedProtocol(String),
    #[error("platform `{0}` is not supported")]
    UnsupportedPlatform(String),
    #[error("capability flags are invalid")]
    InvalidCapabilities,
    #[error("certificate fingerprint is invalid")]
    InvalidFingerprint,
    #[error("bundle chunk count is invalid")]
    InvalidBundleChunkCount,
    #[error("bundle chunk {0} is missing")]
    MissingBundleChunk(usize),
    #[error("bundle chunk {0} exceeds its size limit")]
    BundleChunkTooLarge(usize),
    #[error("pairing bundle is invalid: {0}")]
    InvalidPairingBundle(String),
    #[error("pairing bundle node does not match the advertised node")]
    BundleNodeMismatch,
    #[error("pairing bundle fingerprint does not match the advertised fingerprint")]
    FingerprintMismatch,
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use domain::{HostPlatform, NodeId};
    use identity::IdentityStore;
    use protocol::Capabilities;
    use tempfile::TempDir;

    use super::{
        DiscoveredNode, MAX_NEARBY_NODES, NearbyNodes, PROPERTY_BUNDLE_CHUNKS,
        PROPERTY_FINGERPRINT, RecordError, RegistryUpdate, advertisement,
    };

    fn bundle(node: &str) -> identity::PairingBundle {
        let directory =
            TempDir::new().unwrap_or_else(|error| panic!("temporary directory failed: {error}"));
        let node =
            NodeId::new(node).unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        IdentityStore::new(directory.path())
            .load_or_create(&node)
            .unwrap_or_else(|error| panic!("test identity failed: {error}"))
            .pairing_bundle()
    }

    fn capabilities() -> Capabilities {
        Capabilities {
            keyboard: true,
            relative_pointer: true,
            absolute_pointer: false,
            clipboard_text: true,
        }
    }

    fn resolved(node: &str) -> mdns_sd::ResolvedService {
        advertisement(
            &bundle(node),
            HostPlatform::LinuxWayland,
            capabilities(),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8)),
        )
        .unwrap_or_else(|error| panic!("advertisement failed: {error}"))
        .as_resolved_service()
    }

    #[test]
    fn discovery_record_round_trips_public_pairing_material() {
        let resolved = resolved("studio-left");
        let discovered = DiscoveredNode::from_resolved(&resolved)
            .unwrap_or_else(|error| panic!("record should resolve: {error}"));

        assert_eq!(discovered.node().as_str(), "studio-left");
        assert_eq!(discovered.platform(), HostPlatform::LinuxWayland);
        assert_eq!(discovered.capabilities(), capabilities());
        assert_eq!(
            discovered.addresses().iter().next(),
            Some(&IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8)))
        );
        assert_eq!(
            discovered.pairing_bundle().fingerprint(),
            discovered.fingerprint()
        );
    }

    #[test]
    fn rejects_missing_bundle_chunks_before_decoding() {
        let bundle = bundle("studio-left");
        let mut properties = vec![
            ("node", "studio-left".to_owned()),
            ("protocol", "1.1".to_owned()),
            ("platform", "linux-wayland".to_owned()),
            ("capabilities", "3".to_owned()),
            (
                "fingerprint",
                super::encode_fingerprint(bundle.fingerprint()),
            ),
            (PROPERTY_BUNDLE_CHUNKS, "1".to_owned()),
        ];
        properties.push(("unrelated", "present".to_owned()));
        let service = mdns_sd::ServiceInfo::new(
            super::SERVICE_TYPE,
            "studio-left",
            "studio-left.local.",
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &properties[..],
        )
        .unwrap_or_else(|error| panic!("service should build: {error}"))
        .as_resolved_service();

        assert!(matches!(
            DiscoveredNode::from_resolved(&service),
            Err(RecordError::MissingBundleChunk(0))
        ));
    }

    #[test]
    fn rejects_a_fingerprint_that_does_not_bind_the_bundle() {
        let bundle = bundle("studio-left");
        let mut service = advertisement(
            &bundle,
            HostPlatform::Windows,
            capabilities(),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .unwrap_or_else(|error| panic!("advertisement failed: {error}"));
        let mut properties = service
            .get_properties()
            .iter()
            .map(|property| (property.key().to_owned(), property.val_str().to_owned()))
            .collect::<Vec<_>>();
        let fingerprint = properties
            .iter_mut()
            .find(|(key, _)| key == PROPERTY_FINGERPRINT)
            .map(|(_, value)| value);
        assert!(fingerprint.is_some());
        if let Some(fingerprint) = fingerprint {
            fingerprint.replace_range(..2, "ff");
        }
        service = mdns_sd::ServiceInfo::new(
            super::SERVICE_TYPE,
            "studio-left",
            "studio-left.local.",
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &properties[..],
        )
        .unwrap_or_else(|error| panic!("service should build: {error}"));

        assert!(matches!(
            DiscoveredNode::from_resolved(&service.as_resolved_service()),
            Err(RecordError::FingerprintMismatch)
        ));
    }

    #[test]
    fn registry_is_bounded_and_rejects_identity_conflicts() {
        let mut nearby = NearbyNodes::default();
        let original = DiscoveredNode::from_resolved(&resolved("shared-node"))
            .unwrap_or_else(|error| panic!("record should resolve: {error}"));
        let conflict = DiscoveredNode::from_resolved(&resolved("shared-node"))
            .unwrap_or_else(|error| panic!("record should resolve: {error}"));

        assert_eq!(nearby.upsert(original), RegistryUpdate::Added);
        assert_eq!(nearby.upsert(conflict), RegistryUpdate::IdentityConflict);

        for index in 1..MAX_NEARBY_NODES {
            let node = format!("node-{index}");
            let discovered = DiscoveredNode::from_resolved(&resolved(&node))
                .unwrap_or_else(|error| panic!("record should resolve: {error}"));
            assert_eq!(nearby.upsert(discovered), RegistryUpdate::Added);
        }
        let overflow = DiscoveredNode::from_resolved(&resolved("overflow"))
            .unwrap_or_else(|error| panic!("record should resolve: {error}"));
        assert_eq!(nearby.upsert(overflow), RegistryUpdate::Full);
        assert_eq!(nearby.len(), MAX_NEARBY_NODES);
    }
}
