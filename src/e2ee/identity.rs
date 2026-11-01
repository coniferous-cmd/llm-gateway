//! Long-term X25519 gateway identities.

use std::io;

use base64::Engine as _;
use x25519_dalek::{PublicKey, StaticSecret};

pub const IDENTITY_KEY_SIZE: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    private: [u8; IDENTITY_KEY_SIZE],
    public: [u8; IDENTITY_KEY_SIZE],
}

impl Identity {
    pub fn generate() -> io::Result<Self> {
        let mut private = [0_u8; IDENTITY_KEY_SIZE];
        getrandom::fill(&mut private).map_err(io::Error::other)?;
        Ok(Self::from_private(private))
    }

    pub fn from_private(private: [u8; IDENTITY_KEY_SIZE]) -> Self {
        let public = PublicKey::from(&StaticSecret::from(private)).to_bytes();
        Self { private, public }
    }

    /// Decode a base64 X25519 private key and derive its public half.
    pub fn parse_private(encoded: &str) -> Result<Self, String> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("decode identity key: {error}"))?;
        let bytes: [u8; IDENTITY_KEY_SIZE] = raw.try_into().map_err(|_| {
            "identity key must be a valid 32-byte X25519 key: invalid key length".to_string()
        })?;
        Ok(Self::from_private(bytes))
    }

    /// Decode a pinned peer public key from config.
    pub fn parse_public(encoded: &str) -> Result<[u8; IDENTITY_KEY_SIZE], String> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("decode identity public key: {error}"))?;
        raw.try_into().map_err(|_| {
            "identity public key must be a valid 32-byte X25519 key: invalid key length".to_string()
        })
    }

    pub fn private(&self) -> [u8; IDENTITY_KEY_SIZE] {
        self.private
    }

    pub fn public(&self) -> [u8; IDENTITY_KEY_SIZE] {
        self.public
    }

    pub fn encode_private(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.private)
    }

    pub fn encode_public(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.public)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_identity_round_trips() {
        let identity = Identity::generate().unwrap();
        let parsed = Identity::parse_private(&identity.encode_private()).unwrap();
        assert_eq!(parsed.private(), identity.private());
        assert_eq!(parsed.public(), identity.public());
        assert_ne!(identity.private(), [0_u8; 32]);
        assert_ne!(identity.private(), identity.public());
        assert_eq!(
            Identity::parse_public(&identity.encode_public()).unwrap(),
            identity.public()
        );
    }

    #[test]
    fn malformed_keys_are_rejected() {
        assert!(Identity::parse_private("not base64!").is_err());
        assert!(Identity::parse_private("AAAA").is_err());
        assert!(Identity::parse_private("").is_err());
        assert!(Identity::parse_public("not base64!").is_err());
        let short = base64::engine::general_purpose::STANDARD.encode([0_u8; 31]);
        assert!(Identity::parse_public(&short).is_err());
    }

    #[test]
    fn encoding_is_single_line_standard_base64() {
        let identity = Identity::generate().unwrap();
        let private = identity.encode_private();
        let public = identity.encode_public();
        for value in [&private, &public] {
            assert!(!value.contains(char::is_whitespace));
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .unwrap()
                    .len(),
                32
            );
        }
    }
}
