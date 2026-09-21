//! Authentication and authorization primitives.

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, SaltString},
    Argon2,
};
use dc_common::{DcError, Result};
use dc_protocol::{Capabilities, WireMessage};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Permissions {
    pub view_desktop: bool,
    pub control_input: bool,
    pub open_terminal: bool,
    pub execute_command: bool,
    pub transfer_files: bool,
    pub ssh_access: bool,
}

impl Permissions {
    pub const fn desktop_viewer() -> Self {
        Self {
            view_desktop: true,
            ..Self::empty()
        }
    }

    pub const fn empty() -> Self {
        Self {
            view_desktop: false,
            control_input: false,
            open_terminal: false,
            execute_command: false,
            transfer_files: false,
            ssh_access: false,
        }
    }

    pub const fn to_capabilities(self) -> Capabilities {
        Capabilities {
            desktop: self.view_desktop,
            control_input: self.control_input,
            terminal: self.open_terminal,
            command_execution: self.execute_command,
            file_transfer: self.transfer_files,
            clipboard: false,
            ssh_compatibility: self.ssh_access,
        }
    }
}

/// The stored value is an Argon2id password verifier.  The verifier itself is never sent over
/// the wire; the challenge proof binds it to a fresh nonce for every connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PasswordVerifier {
    encoded: String,
    salt: String,
}

impl PasswordVerifier {
    pub fn from_password(password: &str) -> Result<Self> {
        if password.is_empty() {
            return Err(DcError::InvalidInput("password must not be empty".into()));
        }
        let salt = SaltString::generate(&mut OsRng);
        let encoded = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|error| DcError::Codec(format!("derive password verifier: {error}")))?
            .to_string();
        Ok(Self {
            encoded,
            salt: salt.as_str().to_owned(),
        })
    }

    pub fn from_encoded(encoded: impl Into<String>, salt: impl Into<String>) -> Result<Self> {
        let encoded = encoded.into();
        PasswordHash::new(&encoded)
            .map_err(|error| DcError::InvalidInput(format!("invalid Argon2 verifier: {error}")))?;
        Ok(Self {
            encoded,
            salt: salt.into(),
        })
    }

    pub fn salt(&self) -> &str {
        &self.salt
    }

    pub fn challenge(&self) -> ([u8; 32], WireMessage) {
        let mut nonce = [0; 32];
        OsRng.fill_bytes(&mut nonce);
        (
            nonce,
            WireMessage::Challenge {
                nonce,
                salt: self.salt.clone(),
            },
        )
    }

    pub fn verify(&self, nonce: &[u8; 32], proof: &[u8; 32]) -> bool {
        constant_time_equal(&proof_for(&self.encoded, nonce), proof)
    }
}

pub fn proof_for_password(password: &str, salt: &str, nonce: &[u8; 32]) -> Result<[u8; 32]> {
    let salt = SaltString::from_b64(salt)
        .map_err(|error| DcError::InvalidInput(format!("invalid challenge salt: {error}")))?;
    let encoded = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| DcError::Codec(format!("derive challenge verifier: {error}")))?
        .to_string();
    Ok(proof_for(&encoded, nonce))
}

fn proof_for(encoded: &str, nonce: &[u8; 32]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(encoded.as_bytes());
    digest.update(nonce);
    digest.finalize().into()
}

fn constant_time_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_proof_authenticates_the_password() {
        let verifier = PasswordVerifier::from_password("correct horse").unwrap();
        let (nonce, _) = verifier.challenge();
        let proof = proof_for_password("correct horse", verifier.salt(), &nonce).unwrap();
        assert!(verifier.verify(&nonce, &proof));
        let wrong = proof_for_password("wrong", verifier.salt(), &nonce).unwrap();
        assert!(!verifier.verify(&nonce, &wrong));
    }
}
