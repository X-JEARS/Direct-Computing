//! QUIC/TLS transport and length-delimited protocol streams.

use bytes::Bytes;
use dc_common::{DcError, Result};
use dc_protocol::{WireMessage, MAX_MESSAGE_SIZE};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{
    ClientConfig, Connection, Endpoint, RecvStream, SendStream, ServerConfig, TransportConfig,
    VarInt,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Authenticating,
    Connected,
    Closing,
}

/// Resolve an IP literal or DNS name in the same form accepted by the CLI.
pub async fn resolve_address(address: &str) -> Result<SocketAddr> {
    if let Ok(value) = address.parse() {
        return Ok(value);
    }
    let mut values = tokio::net::lookup_host(address)
        .await
        .map_err(DcError::Io)?;
    values
        .next()
        .ok_or_else(|| DcError::InvalidInput(format!("address has no usable records: {address}")))
}

pub struct QuicServer {
    endpoint: Endpoint,
    certificate_fingerprint: [u8; 32],
}

impl QuicServer {
    pub fn bind(address: SocketAddr) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cert = rcgen::generate_simple_self_signed(vec!["direct-computing".to_owned()])
            .map_err(|error| DcError::Platform(format!("generate TLS certificate: {error}")))?;
        let cert_der = CertificateDer::from(cert.cert.der().to_vec());
        let certificate_fingerprint = fingerprint(cert_der.as_ref());
        let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
        let crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .map_err(|error| DcError::Platform(format!("configure TLS server: {error}")))?;
        let crypto = QuicServerConfig::try_from(crypto)
            .map_err(|error| DcError::Platform(format!("configure QUIC TLS server: {error}")))?;
        let mut config = ServerConfig::with_crypto(Arc::new(crypto));
        config.transport_config(streaming_transport_config()?);
        let endpoint = Endpoint::server(config, address)
            .map_err(|error| DcError::Io(std::io::Error::other(error)))?;
        Ok(Self {
            endpoint,
            certificate_fingerprint,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint.local_addr().map_err(DcError::Io)
    }

    /// SHA-256 fingerprint of the self-signed certificate advertised by this endpoint.
    pub const fn certificate_fingerprint(&self) -> [u8; 32] {
        self.certificate_fingerprint
    }

    pub async fn accept(&self) -> Result<QuicConnection> {
        let incoming = self.endpoint.accept().await.ok_or_else(|| {
            DcError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "QUIC endpoint closed",
            ))
        })?;
        let connection = incoming
            .await
            .map_err(|error| DcError::Platform(format!("accept QUIC connection: {error}")))?;
        Ok(QuicConnection { connection })
    }
}

pub struct QuicClient {
    endpoint: Endpoint,
    verifier: Arc<PinningVerifier>,
}

impl QuicClient {
    pub fn bind(address: SocketAddr) -> Result<Self> {
        Self::bind_with_verifier(address, Arc::new(PinningVerifier::accept_any()))
    }

    /// Bind a client that accepts only the supplied SHA-256 certificate fingerprint.
    pub fn bind_pinned(address: SocketAddr, expected: [u8; 32]) -> Result<Self> {
        Self::bind_with_verifier(address, Arc::new(PinningVerifier::pinned(expected)))
    }

    /// Bind a TOFU client. The first certificate is exposed through
    /// [`Self::server_certificate_fingerprint`] for explicit confirmation and storage.
    pub fn bind_tofu(address: SocketAddr) -> Result<Self> {
        Self::bind_with_verifier(address, Arc::new(PinningVerifier::tofu()))
    }

    fn bind_with_verifier(address: SocketAddr, verifier: Arc<PinningVerifier>) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut endpoint = Endpoint::client(address).map_err(DcError::Io)?;
        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_no_client_auth();
        let crypto = QuicClientConfig::try_from(crypto)
            .map_err(|error| DcError::Platform(format!("configure QUIC TLS client: {error}")))?;
        let mut config = ClientConfig::new(Arc::new(crypto));
        config.transport_config(streaming_transport_config()?);
        endpoint.set_default_client_config(config);
        Ok(Self { endpoint, verifier })
    }

    pub async fn connect(&self, address: SocketAddr) -> Result<QuicConnection> {
        let connecting = self
            .endpoint
            .connect(address, "direct-computing")
            .map_err(|error| DcError::Platform(format!("start QUIC connection: {error}")))?;
        let connection = connecting
            .await
            .map_err(|error| DcError::Platform(format!("connect QUIC: {error}")))?;
        Ok(QuicConnection { connection })
    }

    pub fn server_certificate_fingerprint(&self) -> Option<[u8; 32]> {
        self.verifier.seen.lock().ok().and_then(|value| *value)
    }
}

#[derive(Clone)]
pub struct QuicConnection {
    connection: Connection,
}

impl QuicConnection {
    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }
    pub fn rtt(&self) -> Duration {
        self.connection.rtt()
    }
    /// Return a snapshot of QUIC path and congestion statistics.
    pub fn stats(&self) -> quinn::ConnectionStats {
        self.connection.stats()
    }
    /// Return the largest application datagram currently supported by the path.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.connection.max_datagram_size()
    }
    /// Send an unreliable, unordered application datagram.
    pub fn send_datagram(&self, data: Bytes) -> Result<()> {
        self.connection
            .send_datagram(data)
            .map_err(|error| DcError::Platform(format!("send QUIC datagram: {error}")))
    }
    /// Receive an unreliable, unordered application datagram.
    pub async fn receive_datagram(&self) -> Result<Bytes> {
        self.connection
            .read_datagram()
            .await
            .map_err(|error| DcError::Platform(format!("receive QUIC datagram: {error}")))
    }
    /// Immediately terminate the connection and notify the peer of the reason.
    pub fn close(&self, error_code: u32, reason: &[u8]) {
        self.connection.close(VarInt::from_u32(error_code), reason);
    }
    pub async fn open_stream(&self) -> Result<FramedStream> {
        let (send, recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|error| DcError::Platform(format!("open QUIC stream: {error}")))?;
        Ok(FramedStream { send, recv })
    }
    pub async fn accept_stream(&self) -> Result<FramedStream> {
        let (send, recv) = self
            .connection
            .accept_bi()
            .await
            .map_err(|error| DcError::Platform(format!("accept QUIC stream: {error}")))?;
        Ok(FramedStream { send, recv })
    }
}

pub struct FramedStream {
    send: SendStream,
    recv: RecvStream,
}

impl FramedStream {
    pub async fn send(&mut self, message: &WireMessage) -> Result<()> {
        let encoded = message.encode()?;
        let length = u32::try_from(encoded.len())
            .map_err(|_| DcError::InvalidInput("message is too large".into()))?;
        self.send
            .write_all(&length.to_be_bytes())
            .await
            .map_err(map_quic_io)?;
        self.send.write_all(&encoded).await.map_err(map_quic_io)?;
        Ok(())
    }

    pub async fn receive(&mut self) -> Result<WireMessage> {
        let mut length = [0; 4];
        self.recv
            .read_exact(&mut length)
            .await
            .map_err(map_quic_io)?;
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > MAX_MESSAGE_SIZE {
            return Err(DcError::Codec("invalid QUIC message length".into()));
        }
        let mut payload = vec![0; length];
        self.recv
            .read_exact(&mut payload)
            .await
            .map_err(map_quic_io)?;
        WireMessage::decode(&payload)
    }

    pub async fn close(mut self) -> Result<()> {
        self.send.finish().map_err(map_quic_io)
    }
}

fn map_quic_io(error: impl std::fmt::Display + std::fmt::Debug) -> DcError {
    DcError::Io(std::io::Error::other(format!("{error} ({error:?})")))
}

fn streaming_transport_config() -> Result<Arc<TransportConfig>> {
    let mut config = TransportConfig::default();
    // Enable a bounded unreliable media lane. Video packets are deliberately
    // allowed to be discarded under congestion instead of queueing stale
    // frames behind reliable stream retransmissions.
    config.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    config.datagram_send_buffer_size(2 * 1024 * 1024);
    config.keep_alive_interval(Some(Duration::from_secs(2)));
    // A low-bandwidth recovery keyframe can legitimately take tens of seconds
    // to cross a lossy path. Keep input/control alive across that stall instead
    // of treating it as a dead peer after only ten seconds.
    let idle_timeout = Duration::from_secs(60)
        .try_into()
        .map_err(|error| DcError::InvalidInput(format!("invalid QUIC idle timeout: {error}")))?;
    config.max_idle_timeout(Some(idle_timeout));
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct PinningVerifier {
    expected: Option<[u8; 32]>,
    tofu: bool,
    seen: Mutex<Option<[u8; 32]>>,
}

impl PinningVerifier {
    fn accept_any() -> Self {
        Self {
            expected: None,
            tofu: false,
            seen: Mutex::new(None),
        }
    }
    fn pinned(expected: [u8; 32]) -> Self {
        Self {
            expected: Some(expected),
            tofu: false,
            seen: Mutex::new(None),
        }
    }
    fn tofu() -> Self {
        Self {
            expected: None,
            tofu: true,
            seen: Mutex::new(None),
        }
    }
}

impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        let actual = fingerprint(end_entity.as_ref());
        if let Some(expected) = self.expected {
            if actual != expected {
                return Err(TlsError::General(
                    "server certificate fingerprint mismatch".into(),
                ));
            }
        } else if !self.tofu && self.expected.is_none() {
            // Legacy bind() keeps the original accept-any behavior.
        }
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(actual);
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
        ]
    }
}

fn fingerprint(certificate: &[u8]) -> [u8; 32] {
    Sha256::digest(certificate).into()
}

/// A small, line-oriented TOFU pin database. New pins are never written unless
/// `confirm_new` is true, allowing a CLI to display the fingerprint and ask the
/// user before trusting a first connection.
#[derive(Clone, Debug)]
pub struct CertificatePinStore {
    path: PathBuf,
    pins: BTreeMap<String, [u8; 32]>,
}

impl CertificatePinStore {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut pins = BTreeMap::new();
        if path.exists() {
            let text = fs::read_to_string(&path).map_err(DcError::Io)?;
            for (line_no, line) in text.lines().enumerate() {
                if line.trim().is_empty() || line.starts_with('#') {
                    continue;
                }
                let (endpoint, value) = line.split_once(' ').ok_or_else(|| {
                    DcError::InvalidInput(format!(
                        "invalid certificate pin at line {}",
                        line_no + 1
                    ))
                })?;
                let bytes = parse_fingerprint(value)?;
                pins.insert(endpoint.to_owned(), bytes);
            }
        }
        Ok(Self { path, pins })
    }

    pub fn fingerprint(&self, endpoint: &str) -> Option<[u8; 32]> {
        self.pins.get(endpoint).copied()
    }

    pub fn verify_or_record(
        &mut self,
        endpoint: &str,
        actual: [u8; 32],
        confirm_new: bool,
    ) -> Result<()> {
        match self.pins.get(endpoint).copied() {
            Some(expected) if expected == actual => Ok(()),
            Some(_) => Err(DcError::InvalidInput(format!(
                "certificate fingerprint changed for {endpoint}"
            ))),
            None if confirm_new => {
                self.pins.insert(endpoint.to_owned(), actual);
                self.save()
            }
            None => Err(DcError::InvalidInput(format!(
                "untrusted new certificate for {endpoint}; fingerprint={}",
                format_fingerprint(actual)
            ))),
        }
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(DcError::Io)?;
            }
        }
        let mut text = String::new();
        for (endpoint, fingerprint) in &self.pins {
            text.push_str(endpoint);
            text.push(' ');
            text.push_str(&format_fingerprint(*fingerprint));
            text.push('\n');
        }
        let temporary = self.path.with_extension("tmp");
        fs::write(&temporary, text).map_err(DcError::Io)?;
        fs::rename(temporary, &self.path).map_err(DcError::Io)
    }
}

pub fn format_fingerprint(value: [u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn parse_fingerprint(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64 {
        return Err(DcError::InvalidInput(
            "certificate fingerprint must contain 64 hex characters".into(),
        ));
    }
    let mut output = [0; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|_| {
            DcError::InvalidInput("certificate fingerprint is not valid hex".into())
        })?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn server_binds_an_ephemeral_local_port() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap();
                assert_ne!(server.local_addr().unwrap().port(), 0);
            });
    }

    #[test]
    fn certificate_pin_store_requires_confirmation_for_new_hosts() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("direct-computing-pins-{unique}"));
        let mut store = CertificatePinStore::load(&path).unwrap();
        let fingerprint = [0xabu8; 32];
        assert!(store
            .verify_or_record("127.0.0.1:22100", fingerprint, false)
            .is_err());
        store
            .verify_or_record("127.0.0.1:22100", fingerprint, true)
            .unwrap();
        let loaded = CertificatePinStore::load(&path).unwrap();
        assert_eq!(loaded.fingerprint("127.0.0.1:22100"), Some(fingerprint));
        assert!(loaded
            .clone()
            .verify_or_record("127.0.0.1:22100", [0x11; 32], false)
            .is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn fingerprint_format_is_stable() {
        let value = [0xabu8; 32];
        assert_eq!(format_fingerprint(value).len(), 64);
        assert_eq!(
            parse_fingerprint(&format_fingerprint(value)).unwrap(),
            value
        );
    }
}

#[test]
fn reliable_stream_and_datagram_lanes_coexist() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let client = QuicClient::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let address = server.local_addr().unwrap();
            let server_task = tokio::spawn(async move { server.accept().await.unwrap() });
            let client_connection = client.connect(address).await.unwrap();
            let server_connection = server_task.await.unwrap();
            let max_size = client_connection.max_datagram_size().unwrap();
            assert!(max_size >= 1_000);
            let reliable_receiver = server_connection.clone();
            let reliable_task = tokio::spawn(async move {
                let mut stream = reliable_receiver.accept_stream().await.unwrap();
                stream.receive().await.unwrap()
            });
            let mut reliable_stream = client_connection.open_stream().await.unwrap();
            reliable_stream.send(&WireMessage::Close).await.unwrap();
            client_connection
                .send_datagram(Bytes::from_static(b"datagram-test"))
                .unwrap();
            let received =
                tokio::time::timeout(Duration::from_secs(1), server_connection.receive_datagram())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(&received[..], b"datagram-test");
            assert_eq!(reliable_task.await.unwrap(), WireMessage::Close);
        });
}
