//! Shared session lifecycle types and the stage 2 authentication handshake.

use dc_auth::{PasswordVerifier, Permissions};
use dc_common::{DcError, Result};
use dc_protocol::{validate_version, Capabilities, Hello, WireMessage, PROTOCOL_VERSION};
use dc_transport::FramedStream;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    Created,
    Negotiating,
    Active,
    Closing,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedSession {
    pub permissions: Permissions,
    /// Capabilities advertised by the authenticated peer. Keeping this on
    /// the session lets applications select compatible media lanes instead
    /// of silently black-screening when an older client connects.
    pub peer_capabilities: Capabilities,
}

pub async fn authenticate_server(
    stream: &mut FramedStream,
    verifier: &PasswordVerifier,
    permissions: Permissions,
    capabilities: Capabilities,
) -> Result<AuthenticatedSession> {
    let WireMessage::Hello(hello) = stream.receive().await? else {
        return Err(DcError::Codec("expected client hello".into()));
    };
    validate_version(hello.protocol_version)?;
    let (nonce, challenge) = verifier.challenge();
    stream.send(&challenge).await?;
    let WireMessage::Authenticate { proof } = stream.receive().await? else {
        return Err(DcError::Codec("expected authentication proof".into()));
    };
    if !verifier.verify(&nonce, &proof) {
        stream
            .send(&WireMessage::Rejected {
                reason: "authentication failed".into(),
            })
            .await?;
        return Err(DcError::InvalidInput("authentication failed".into()));
    }
    let mut advertised_permissions = permissions.to_capabilities();
    // Mirror transport features in the authenticated response so the Viewer
    // can select only media lanes implemented by both peers.
    advertised_permissions.hybrid_video = capabilities.hybrid_video;
    advertised_permissions.desktop_optimizations = capabilities.desktop_optimizations;
    advertised_permissions.h264_nal_datagrams = capabilities.h264_nal_datagrams;
    stream
        .send(&WireMessage::Authenticated {
            permissions: advertised_permissions,
        })
        .await?;
    Ok(AuthenticatedSession {
        permissions,
        peer_capabilities: hello.capabilities,
    })
}

pub async fn authenticate_client(
    stream: &mut FramedStream,
    password: &str,
    capabilities: Capabilities,
) -> Result<AuthenticatedSession> {
    stream
        .send(&WireMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            capabilities,
        }))
        .await?;
    let WireMessage::Challenge { nonce, salt } = stream.receive().await? else {
        return Err(DcError::Codec("expected server challenge".into()));
    };
    let proof = dc_auth::proof_for_password(password, &salt, &nonce)?;
    stream.send(&WireMessage::Authenticate { proof }).await?;
    match stream.receive().await? {
        WireMessage::Authenticated { permissions } => Ok(AuthenticatedSession {
            permissions: Permissions {
                view_desktop: permissions.desktop,
                control_input: permissions.control_input,
                open_terminal: permissions.terminal,
                execute_command: permissions.command_execution,
                transfer_files: permissions.file_transfer,
                ssh_access: permissions.ssh_compatibility,
            },
            peer_capabilities: permissions,
        }),
        WireMessage::Rejected { reason } => Err(DcError::InvalidInput(reason)),
        _ => Err(DcError::Codec("expected authentication result".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_are_mapped_to_permissions() {
        let permissions = Permissions {
            view_desktop: true,
            control_input: false,
            open_terminal: true,
            execute_command: false,
            transfer_files: true,
            ssh_access: false,
        };
        let caps = permissions.to_capabilities();
        assert!(caps.desktop && caps.terminal && caps.file_transfer);
        assert!(!caps.command_execution);
    }
}
