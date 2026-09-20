//! QUIC/TLS transport and length-delimited protocol streams.

use dc_common::{DcError, Result};
use dc_protocol::{WireMessage, MAX_MESSAGE_SIZE};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Connection, Endpoint, RecvStream, SendStream, ServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use std::{net::SocketAddr, sync::Arc, time::Duration};

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
}

impl QuicServer {
    pub fn bind(address: SocketAddr) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cert = rcgen::generate_simple_self_signed(vec!["direct-computing".to_owned()])
            .map_err(|error| DcError::Platform(format!("generate TLS certificate: {error}")))?;
        let cert_der = CertificateDer::from(cert.cert.der().to_vec());
        let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
        let crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .map_err(|error| DcError::Platform(format!("configure TLS server: {error}")))?;
        let crypto = QuicServerConfig::try_from(crypto)
            .map_err(|error| DcError::Platform(format!("configure QUIC TLS server: {error}")))?;
        let endpoint = Endpoint::server(ServerConfig::with_crypto(Arc::new(crypto)), address)
            .map_err(|error| DcError::Io(std::io::Error::other(error)))?;
        Ok(Self { endpoint })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint.local_addr().map_err(DcError::Io)
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
}

impl QuicClient {
    pub fn bind(address: SocketAddr) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut endpoint = Endpoint::client(address).map_err(DcError::Io)?;
        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate))
            .with_no_client_auth();
        let crypto = QuicClientConfig::try_from(crypto)
            .map_err(|error| DcError::Platform(format!("configure QUIC TLS client: {error}")))?;
        endpoint.set_default_client_config(ClientConfig::new(Arc::new(crypto)));
        Ok(Self { endpoint })
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

fn map_quic_io(error: impl std::fmt::Display) -> DcError {
    DcError::Io(std::io::Error::other(error.to_string()))
}

#[derive(Debug)]
struct AcceptAnyCertificate;

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
