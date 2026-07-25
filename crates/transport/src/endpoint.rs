use std::{
    collections::BTreeMap,
    net::SocketAddr,
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::Duration,
};

use domain::{HostPlatform, NodeId};
use identity::{LocalIdentity, TrustStore, TrustedPeer};
use protocol::{
    CURRENT_PROTOCOL, Capabilities, ClipboardText, Envelope, Handshake, Hello, RejectReason,
    Session,
};
use quinn::{Connection, Endpoint, RecvStream, SendStream, VarInt};
use rand::random;
use rustls_pki_types::CertificateDer;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::time::timeout;

use crate::{
    SessionLimits, TlsError,
    frame::{FrameError, read_bulk, read_control, write_bulk, write_control},
    replay::ReplayGuard,
    tls::{client_config, server_config},
};

const SERVER_NAME: &str = "tevir.local";
const CONTROL_STREAM: u8 = 1;
const CLIPBOARD_STREAM: u8 = 2;
const CONTROL_PRIORITY: i32 = 10;
const CLIPBOARD_PRIORITY: i32 = 0;
const CLOSE_AUTHENTICATION: u32 = 1;
const CLOSE_PROTOCOL: u32 = 2;
const HEARTBEAT_INTERVAL_MS: u32 = 10_000;

#[derive(Clone, Copy, Debug)]
pub struct SessionProfile {
    pub platform: HostPlatform,
    pub capabilities: Capabilities,
}

pub struct SecureServer {
    endpoint: Endpoint,
    identity: LocalIdentity,
    peers_by_fingerprint: BTreeMap<[u8; 32], TrustedPeer>,
    profile: SessionProfile,
    limits: SessionLimits,
    replay_guard: Arc<Mutex<ReplayGuard>>,
}

impl SecureServer {
    pub fn bind(
        address: SocketAddr,
        identity: LocalIdentity,
        trust: &TrustStore,
        profile: SessionProfile,
        limits: SessionLimits,
    ) -> Result<Self, TransportError> {
        let config = server_config(&identity, trust, &limits)?;
        let endpoint = Endpoint::server(config, address).map_err(TransportError::Bind)?;
        let peers_by_fingerprint = trust
            .peers()
            .map(|peer| (peer.fingerprint(), peer.clone()))
            .collect();
        tracing::info!(node = %identity.node(), address = %address, "secure server bound");
        Ok(Self {
            endpoint,
            identity,
            peers_by_fingerprint,
            profile,
            limits,
            replay_guard: Arc::new(Mutex::new(ReplayGuard::new())),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        self.endpoint.local_addr().map_err(TransportError::Bind)
    }

    pub async fn accept(&self) -> Result<PeerConnection, TransportError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or(TransportError::EndpointClosed)?;
        let connection = timeout(self.limits.handshake_timeout(), incoming)
            .await
            .map_err(|_| TransportError::TimedOut("TLS handshake"))?
            .map_err(TransportError::Connection)?;
        let peer = match authenticated_peer(&connection, &self.peers_by_fingerprint) {
            Ok(peer) => peer,
            Err(error) => {
                tracing::warn!(error = %error, "incoming peer authentication rejected");
                connection.close(VarInt::from_u32(CLOSE_AUTHENTICATION), b"authentication");
                return Err(error);
            }
        };
        self.complete_handshake(connection, peer).await
    }

    async fn complete_handshake(
        &self,
        connection: Connection,
        authenticated_peer: TrustedPeer,
    ) -> Result<PeerConnection, TransportError> {
        let (mut send, mut receive) =
            operation(self.limits.handshake_timeout(), "control stream", async {
                connection
                    .accept_bi()
                    .await
                    .map_err(TransportError::Connection)
            })
            .await?;
        expect_stream_kind(
            &mut receive,
            CONTROL_STREAM,
            self.limits.handshake_timeout(),
        )
        .await?;
        send.set_priority(CONTROL_PRIORITY)
            .map_err(|_| TransportError::StreamClosed)?;
        let message = read_control_with_timeout(
            &mut receive,
            self.limits.maximum_control_frame_bytes(),
            self.limits.handshake_timeout(),
        )
        .await?;
        let hello = match message {
            Envelope::Handshake(Handshake::Hello(hello)) => hello,
            _ => {
                connection.close(VarInt::from_u32(CLOSE_PROTOCOL), b"handshake");
                return Err(TransportError::UnexpectedHandshake);
            }
        };

        if hello.node != *authenticated_peer.node() {
            reject(&mut send, RejectReason::AuthenticationFailed, &self.limits).await;
            connection.close(VarInt::from_u32(CLOSE_AUTHENTICATION), b"node identity");
            return Err(TransportError::NodeIdentityMismatch {
                certificate: authenticated_peer.node().clone(),
                claimed: hello.node,
            });
        }
        if !hello.version.is_current() {
            reject(&mut send, RejectReason::VersionMismatch, &self.limits).await;
            connection.close(VarInt::from_u32(CLOSE_PROTOCOL), b"version");
            return Err(TransportError::VersionMismatch);
        }
        let admitted = self
            .replay_guard
            .lock()
            .map_err(|_| TransportError::ReplayStateUnavailable)?
            .admit(hello.node.clone(), hello.nonce);
        if !admitted {
            reject(&mut send, RejectReason::ReplayDetected, &self.limits).await;
            connection.close(VarInt::from_u32(CLOSE_AUTHENTICATION), b"replay");
            return Err(TransportError::ReplayDetected);
        }

        let maximum_frame_bytes = self
            .limits
            .maximum_control_frame_bytes()
            .min(hello.maximum_frame_bytes.get() as usize);
        let maximum_frame_bytes_u32 =
            u32::try_from(maximum_frame_bytes).map_err(|_| TransportError::InvalidFrameLimit)?;
        let negotiated_capabilities = self.profile.capabilities.intersection(hello.capabilities);
        let session_id = random();
        let server_nonce = random();
        let accepted = Envelope::Handshake(Handshake::Accepted {
            session_id,
            controller: self.identity.node().clone(),
            client_nonce: hello.nonce,
            server_nonce,
            platform: self.profile.platform,
            negotiated_capabilities,
            maximum_frame_bytes: NonZeroU32::new(maximum_frame_bytes_u32)
                .ok_or(TransportError::InvalidFrameLimit)?,
            heartbeat_interval_ms: NonZeroU32::new(HEARTBEAT_INTERVAL_MS)
                .ok_or(TransportError::InvalidHeartbeatInterval)?,
        });
        write_control_with_timeout(
            &mut send,
            &accepted,
            maximum_frame_bytes,
            self.limits.handshake_timeout(),
        )
        .await?;
        tracing::info!(
            peer = %hello.node,
            session_id,
            "secure peer session accepted"
        );

        Ok(PeerConnection {
            connection,
            info: ConnectionInfo {
                peer: hello.node,
                session_id,
                negotiated_capabilities,
                peer_platform: hello.platform,
            },
            send,
            receive,
            maximum_control_frame_bytes: maximum_frame_bytes,
            maximum_clipboard_frame_bytes: self.limits.maximum_clipboard_frame_bytes(),
            operation_timeout: self.limits.operation_timeout(),
        })
    }
}

pub struct SecureClient {
    endpoint: Endpoint,
    identity: LocalIdentity,
    peers: BTreeMap<NodeId, TrustedPeer>,
    profile: SessionProfile,
    limits: SessionLimits,
}

impl SecureClient {
    pub fn bind(
        address: SocketAddr,
        identity: LocalIdentity,
        trust: &TrustStore,
        profile: SessionProfile,
        limits: SessionLimits,
    ) -> Result<Self, TransportError> {
        let endpoint = Endpoint::client(address).map_err(TransportError::Bind)?;
        let peers = trust
            .peers()
            .map(|peer| (peer.node().clone(), peer.clone()))
            .collect();
        tracing::info!(node = %identity.node(), address = %address, "secure client bound");
        Ok(Self {
            endpoint,
            identity,
            peers,
            profile,
            limits,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        self.endpoint.local_addr().map_err(TransportError::Bind)
    }

    pub async fn connect(
        &self,
        peer_node: &NodeId,
        address: SocketAddr,
    ) -> Result<PeerConnection, TransportError> {
        tracing::info!(peer = %peer_node, address = %address, "connecting to secure peer");
        let peer = self
            .peers
            .get(peer_node)
            .ok_or_else(|| TransportError::UnknownPeer(peer_node.clone()))?;
        let config = client_config(&self.identity, peer, &self.limits)?;
        let connecting = self
            .endpoint
            .connect_with(config, address, SERVER_NAME)
            .map_err(TransportError::Connect)?;
        let connection = timeout(self.limits.handshake_timeout(), connecting)
            .await
            .map_err(|_| TransportError::TimedOut("TLS handshake"))?
            .map_err(TransportError::Connection)?;
        authenticate_expected_peer(&connection, peer)?;
        self.complete_handshake(connection, peer_node.clone()).await
    }

    async fn complete_handshake(
        &self,
        connection: Connection,
        peer_node: NodeId,
    ) -> Result<PeerConnection, TransportError> {
        let (mut send, mut receive) =
            operation(self.limits.handshake_timeout(), "control stream", async {
                connection
                    .open_bi()
                    .await
                    .map_err(TransportError::Connection)
            })
            .await?;
        send.set_priority(CONTROL_PRIORITY)
            .map_err(|_| TransportError::StreamClosed)?;
        write_stream_kind(&mut send, CONTROL_STREAM, self.limits.handshake_timeout()).await?;

        let nonce = random();
        let maximum_frame_bytes = u32::try_from(self.limits.maximum_control_frame_bytes())
            .map_err(|_| TransportError::InvalidFrameLimit)?;
        let hello = Envelope::Handshake(Handshake::Hello(Hello {
            version: CURRENT_PROTOCOL,
            node: self.identity.node().clone(),
            nonce,
            platform: self.profile.platform,
            capabilities: self.profile.capabilities,
            maximum_frame_bytes: NonZeroU32::new(maximum_frame_bytes)
                .ok_or(TransportError::InvalidFrameLimit)?,
        }));
        write_control_with_timeout(
            &mut send,
            &hello,
            self.limits.maximum_control_frame_bytes(),
            self.limits.handshake_timeout(),
        )
        .await?;
        let response = read_control_with_timeout(
            &mut receive,
            self.limits.maximum_control_frame_bytes(),
            self.limits.handshake_timeout(),
        )
        .await?;

        let (
            session_id,
            controller,
            client_nonce,
            peer_platform,
            negotiated_capabilities,
            negotiated_maximum,
        ) = match response {
            Envelope::Handshake(Handshake::Accepted {
                session_id,
                controller,
                client_nonce,
                platform,
                negotiated_capabilities,
                maximum_frame_bytes,
                ..
            }) => (
                session_id,
                controller,
                client_nonce,
                platform,
                negotiated_capabilities,
                maximum_frame_bytes.get() as usize,
            ),
            Envelope::Handshake(Handshake::Rejected { reason }) => {
                return Err(TransportError::Rejected(reason));
            }
            _ => return Err(TransportError::UnexpectedHandshake),
        };
        if controller != peer_node || client_nonce != nonce {
            connection.close(VarInt::from_u32(CLOSE_AUTHENTICATION), b"handshake binding");
            return Err(TransportError::HandshakeBindingMismatch);
        }
        if negotiated_maximum == 0 || negotiated_maximum > self.limits.maximum_control_frame_bytes()
        {
            connection.close(VarInt::from_u32(CLOSE_PROTOCOL), b"frame limit");
            return Err(TransportError::InvalidFrameLimit);
        }
        tracing::info!(peer = %peer_node, session_id, "secure peer session established");

        Ok(PeerConnection {
            connection,
            info: ConnectionInfo {
                peer: peer_node,
                session_id,
                negotiated_capabilities,
                peer_platform,
            },
            send,
            receive,
            maximum_control_frame_bytes: negotiated_maximum,
            maximum_clipboard_frame_bytes: self.limits.maximum_clipboard_frame_bytes(),
            operation_timeout: self.limits.operation_timeout(),
        })
    }
}

pub struct PeerConnection {
    connection: Connection,
    info: ConnectionInfo,
    send: SendStream,
    receive: RecvStream,
    maximum_control_frame_bytes: usize,
    maximum_clipboard_frame_bytes: usize,
    operation_timeout: Duration,
}

impl PeerConnection {
    #[must_use]
    pub fn info(&self) -> &ConnectionInfo {
        &self.info
    }

    pub async fn send(&mut self, message: Session) -> Result<(), TransportError> {
        write_control_with_timeout(
            &mut self.send,
            &Envelope::Session(message),
            self.maximum_control_frame_bytes,
            self.operation_timeout,
        )
        .await
    }

    pub async fn receive(&mut self) -> Result<Session, TransportError> {
        match read_control_with_timeout(
            &mut self.receive,
            self.maximum_control_frame_bytes,
            self.operation_timeout,
        )
        .await?
        {
            Envelope::Session(message) => Ok(message),
            Envelope::Handshake(_) => Err(TransportError::UnexpectedHandshake),
        }
    }

    pub async fn open_clipboard(&self) -> Result<ClipboardStream, TransportError> {
        let (mut send, receive) =
            operation(self.operation_timeout, "opening clipboard stream", async {
                self.connection
                    .open_bi()
                    .await
                    .map_err(TransportError::Connection)
            })
            .await?;
        send.set_priority(CLIPBOARD_PRIORITY)
            .map_err(|_| TransportError::StreamClosed)?;
        write_stream_kind(&mut send, CLIPBOARD_STREAM, self.operation_timeout).await?;
        Ok(ClipboardStream {
            send,
            receive,
            maximum_frame_bytes: self.maximum_clipboard_frame_bytes,
            operation_timeout: self.operation_timeout,
        })
    }

    pub async fn accept_clipboard(&self) -> Result<ClipboardStream, TransportError> {
        let (send, mut receive) = operation(
            self.operation_timeout,
            "accepting clipboard stream",
            async {
                self.connection
                    .accept_bi()
                    .await
                    .map_err(TransportError::Connection)
            },
        )
        .await?;
        expect_stream_kind(&mut receive, CLIPBOARD_STREAM, self.operation_timeout).await?;
        send.set_priority(CLIPBOARD_PRIORITY)
            .map_err(|_| TransportError::StreamClosed)?;
        Ok(ClipboardStream {
            send,
            receive,
            maximum_frame_bytes: self.maximum_clipboard_frame_bytes,
            operation_timeout: self.operation_timeout,
        })
    }

    pub fn close(&self) {
        tracing::info!(peer = %self.info.peer, session_id = self.info.session_id, "closing peer session");
        self.connection.close(VarInt::from_u32(0), b"closed");
    }

    pub async fn closed(&self) {
        let _reason = self.connection.closed().await;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionInfo {
    pub peer: NodeId,
    pub session_id: u128,
    pub negotiated_capabilities: Capabilities,
    pub peer_platform: HostPlatform,
}

pub struct ClipboardStream {
    send: SendStream,
    receive: RecvStream,
    maximum_frame_bytes: usize,
    operation_timeout: Duration,
}

impl ClipboardStream {
    pub async fn send(&mut self, transfer: &ClipboardText) -> Result<(), TransportError> {
        let payload = transfer.encode()?;
        operation(self.operation_timeout, "writing clipboard frame", async {
            write_bulk(&mut self.send, &payload, self.maximum_frame_bytes)
                .await
                .map_err(TransportError::Frame)
        })
        .await
    }

    pub async fn receive(&mut self) -> Result<ClipboardText, TransportError> {
        let payload = operation(self.operation_timeout, "reading clipboard frame", async {
            read_bulk(&mut self.receive, self.maximum_frame_bytes)
                .await
                .map_err(TransportError::Frame)
        })
        .await?;
        ClipboardText::decode(&payload).map_err(TransportError::Clipboard)
    }

    pub fn finish(&mut self) -> Result<(), TransportError> {
        self.send.finish().map_err(|_| TransportError::StreamClosed)
    }
}

fn authenticated_peer(
    connection: &Connection,
    peers: &BTreeMap<[u8; 32], TrustedPeer>,
) -> Result<TrustedPeer, TransportError> {
    let fingerprint = peer_anchor_fingerprint(connection)?;
    peers
        .get(&fingerprint)
        .cloned()
        .ok_or(TransportError::UntrustedCertificate)
}

fn authenticate_expected_peer(
    connection: &Connection,
    peer: &TrustedPeer,
) -> Result<(), TransportError> {
    if peer_anchor_fingerprint(connection)? == peer.fingerprint() {
        Ok(())
    } else {
        Err(TransportError::UntrustedCertificate)
    }
}

fn peer_anchor_fingerprint(connection: &Connection) -> Result<[u8; 32], TransportError> {
    let identity = connection
        .peer_identity()
        .ok_or(TransportError::MissingPeerCertificate)?;
    let certificates = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| TransportError::UnsupportedPeerIdentity)?;
    let anchor = certificates
        .last()
        .ok_or(TransportError::MissingPeerCertificate)?;
    Ok(Sha256::digest(anchor.as_ref()).into())
}

async fn write_stream_kind(
    send: &mut SendStream,
    kind: u8,
    deadline: Duration,
) -> Result<(), TransportError> {
    operation(deadline, "writing stream type", async {
        tokio::io::AsyncWriteExt::write_all(send, &[kind])
            .await
            .map_err(|error| TransportError::Frame(FrameError::Io(error)))
    })
    .await
}

async fn expect_stream_kind(
    receive: &mut RecvStream,
    expected: u8,
    deadline: Duration,
) -> Result<(), TransportError> {
    let mut actual = [0];
    operation(deadline, "reading stream type", async {
        tokio::io::AsyncReadExt::read_exact(receive, &mut actual)
            .await
            .map_err(|error| TransportError::Frame(FrameError::Io(error)))
    })
    .await?;
    if actual[0] != expected {
        return Err(TransportError::UnexpectedStreamKind(actual[0]));
    }
    Ok(())
}

async fn write_control_with_timeout(
    send: &mut SendStream,
    message: &Envelope,
    maximum: usize,
    deadline: Duration,
) -> Result<(), TransportError> {
    operation(deadline, "writing control frame", async {
        write_control(send, message, maximum)
            .await
            .map_err(TransportError::Frame)
    })
    .await
}

async fn read_control_with_timeout(
    receive: &mut RecvStream,
    maximum: usize,
    deadline: Duration,
) -> Result<Envelope, TransportError> {
    operation(deadline, "reading control frame", async {
        read_control(receive, maximum)
            .await
            .map_err(TransportError::Frame)
    })
    .await
}

async fn reject(send: &mut SendStream, reason: RejectReason, limits: &SessionLimits) {
    let message = Envelope::Handshake(Handshake::Rejected { reason });
    let _result = write_control_with_timeout(
        send,
        &message,
        limits.maximum_control_frame_bytes(),
        limits.handshake_timeout(),
    )
    .await;
}

async fn operation<T>(
    duration: Duration,
    name: &'static str,
    future: impl Future<Output = Result<T, TransportError>>,
) -> Result<T, TransportError> {
    timeout(duration, future)
        .await
        .map_err(|_| TransportError::TimedOut(name))?
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("could not bind the QUIC endpoint: {0}")]
    Bind(std::io::Error),
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error("could not begin the QUIC connection: {0}")]
    Connect(quinn::ConnectError),
    #[error("the QUIC connection failed: {0}")]
    Connection(quinn::ConnectionError),
    #[error("the endpoint has closed")]
    EndpointClosed,
    #[error("operation timed out while {0}")]
    TimedOut(&'static str),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Clipboard(#[from] protocol::ClipboardError),
    #[error("peer did not present a certificate")]
    MissingPeerCertificate,
    #[error("peer identity has an unsupported representation")]
    UnsupportedPeerIdentity,
    #[error("peer certificate is not paired")]
    UntrustedCertificate,
    #[error("node `{0}` is not paired")]
    UnknownPeer(NodeId),
    #[error("certificate belongs to `{certificate}`, but the peer claimed `{claimed}`")]
    NodeIdentityMismatch {
        certificate: NodeId,
        claimed: NodeId,
    },
    #[error("peer uses an incompatible protocol version")]
    VersionMismatch,
    #[error("peer replayed a previously accepted handshake")]
    ReplayDetected,
    #[error("the replay admission state is unavailable")]
    ReplayStateUnavailable,
    #[error("peer rejected the handshake: {0:?}")]
    Rejected(RejectReason),
    #[error("peer sent a message that is invalid for the handshake state")]
    UnexpectedHandshake,
    #[error("peer sent unknown stream type {0}")]
    UnexpectedStreamKind(u8),
    #[error("peer handshake was not bound to the authenticated connection")]
    HandshakeBindingMismatch,
    #[error("the negotiated frame limit is invalid")]
    InvalidFrameLimit,
    #[error("the heartbeat interval is invalid")]
    InvalidHeartbeatInterval,
    #[error("the QUIC stream has already closed")]
    StreamClosed,
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, SocketAddr},
        num::NonZeroU64,
    };

    use domain::NodeId;
    use identity::{IdentityStore, LocalIdentity, TrustStore};
    use protocol::{Capabilities, ClipboardGeneration, ClipboardText, HostPlatform, Session};
    use tempfile::TempDir;

    use super::{SecureClient, SecureServer, SessionLimits, SessionProfile, TransportError};

    fn node(value: &str) -> NodeId {
        NodeId::new(value).unwrap_or_else(|error| panic!("invalid test node: {error}"))
    }

    fn create_identity(directory: &TempDir, id: &str) -> LocalIdentity {
        IdentityStore::new(directory.path())
            .load_or_create(&node(id))
            .unwrap_or_else(|error| panic!("identity creation failed: {error}"))
    }

    fn trust(directory: &TempDir, remote: &LocalIdentity) -> TrustStore {
        let store = IdentityStore::new(directory.path());
        let mut trust = store
            .trust_store()
            .unwrap_or_else(|error| panic!("trust store failed: {error}"));
        let bundle = remote.pairing_bundle();
        let code = bundle.code().to_string();
        trust
            .trust(bundle, &code)
            .unwrap_or_else(|error| panic!("pairing failed: {error}"));
        trust
    }

    fn profile() -> SessionProfile {
        SessionProfile {
            platform: HostPlatform::LinuxWayland,
            capabilities: Capabilities {
                keyboard: true,
                relative_pointer: true,
                absolute_pointer: false,
                clipboard_text: true,
            },
        }
    }

    async fn connected_pair() -> (PeerConnection, PeerConnection) {
        let left_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let right_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let left = create_identity(&left_directory, "left");
        let right = create_identity(&right_directory, "right");
        let left_trust = trust(&left_directory, &right);
        let right_trust = trust(&right_directory, &left);
        let server = SecureServer::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            left,
            &left_trust,
            profile(),
            SessionLimits::default(),
        )
        .unwrap_or_else(|error| panic!("server bind failed: {error}"));
        let client = SecureClient::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            right,
            &right_trust,
            profile(),
            SessionLimits::default(),
        )
        .unwrap_or_else(|error| panic!("client bind failed: {error}"));
        let address = server
            .local_addr()
            .unwrap_or_else(|error| panic!("local address failed: {error}"));

        let server_node = node("left");
        let (accepted, connected) =
            tokio::join!(server.accept(), client.connect(&server_node, address));
        (
            accepted.unwrap_or_else(|error| panic!("server handshake failed: {error}")),
            connected.unwrap_or_else(|error| panic!("client handshake failed: {error}")),
        )
    }

    use super::PeerConnection;

    #[tokio::test]
    async fn exchanges_control_and_clipboard_on_separate_streams() {
        let (mut server_connection, mut client_connection) = connected_pair().await;
        client_connection
            .send(Session::Heartbeat { nonce: 42 })
            .await
            .unwrap_or_else(|error| panic!("control send failed: {error}"));
        assert!(matches!(
            server_connection
                .receive()
                .await
                .map(|message| matches!(message, Session::Heartbeat { nonce: 42 })),
            Ok(true)
        ));

        let (server_clipboard, client_clipboard) = tokio::join!(
            server_connection.accept_clipboard(),
            client_connection.open_clipboard()
        );
        let mut server_clipboard =
            server_clipboard.unwrap_or_else(|error| panic!("clipboard accept failed: {error}"));
        let mut client_clipboard =
            client_clipboard.unwrap_or_else(|error| panic!("clipboard open failed: {error}"));
        let transfer = ClipboardText::new(
            ClipboardGeneration::new(node("right"), NonZeroU64::new(1).unwrap_or(NonZeroU64::MIN)),
            "clipboard payload",
        )
        .unwrap_or_else(|error| panic!("clipboard payload failed: {error}"));
        client_clipboard
            .send(&transfer)
            .await
            .unwrap_or_else(|error| panic!("clipboard send failed: {error}"));
        assert_eq!(
            server_clipboard
                .receive()
                .await
                .unwrap_or_else(|error| panic!("clipboard receive failed: {error}")),
            transfer
        );
    }

    #[tokio::test]
    async fn rejects_a_client_that_the_server_has_not_paired() {
        let server_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let client_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let decoy_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let server_identity = create_identity(&server_directory, "server");
        let client_identity = create_identity(&client_directory, "client");
        let decoy_identity = create_identity(&decoy_directory, "decoy");
        let server_trust = trust(&server_directory, &decoy_identity);
        let client_trust = trust(&client_directory, &server_identity);
        let server = SecureServer::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            server_identity,
            &server_trust,
            profile(),
            SessionLimits::default(),
        )
        .unwrap_or_else(|error| panic!("server bind failed: {error}"));
        let client = SecureClient::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            client_identity,
            &client_trust,
            profile(),
            SessionLimits::default(),
        )
        .unwrap_or_else(|error| panic!("client bind failed: {error}"));
        let address = server
            .local_addr()
            .unwrap_or_else(|error| panic!("local address failed: {error}"));

        let server_node = node("server");
        let (accepted, connected) =
            tokio::join!(server.accept(), client.connect(&server_node, address));
        assert!(accepted.is_err());
        assert!(connected.is_err());
    }

    #[tokio::test]
    async fn reports_connection_loss_to_the_control_reader() {
        let (mut server_connection, client_connection) = connected_pair().await;
        client_connection.close();

        assert!(matches!(
            server_connection.receive().await,
            Err(TransportError::Frame(_)) | Err(TransportError::Connection(_))
        ));
    }
}
