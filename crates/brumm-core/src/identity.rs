//! Identity layer: a user key derived from a BIP-39 seed phrase, per-device iroh keys and
//! device certificates that bind a device (`EndpointId`) to its user.
//!
//! The user key is the stable identity contacts know; device keys are what iroh dials.

use bip39::Mnemonic;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::crypto::OlmIdentityKey;

const USER_KEY_CONTEXT: &str = "brumm 2026 user identity key v1";
const DEVICE_CERT_DOMAIN: &[u8] = b"brumm/device-cert/v2";
const SEED_ENTROPY_BYTES: usize = 32;
const FINGERPRINT_BYTES: usize = 10;
const FINGERPRINT_GROUP_CHARS: usize = 4;
pub const MAX_DISPLAY_NAME_CHARS: usize = 64;

/// Public user key bytes (Ed25519).
pub type UserKey = [u8; 32];

/// The secret side of a user identity. Lives on every linked device.
pub struct UserIdentity {
    signing_key: SigningKey,
    mnemonic: Mnemonic,
}

impl UserIdentity {
    pub fn generate() -> Self {
        let entropy: [u8; SEED_ENTROPY_BYTES] = rand::random();
        let mnemonic = Mnemonic::from_entropy(&entropy).expect("32 bytes is valid BIP-39 entropy");
        Self::from_mnemonic(mnemonic)
    }

    pub fn from_phrase(phrase: &str) -> Result<Self, bip39::Error> {
        Ok(Self::from_mnemonic(Mnemonic::parse(phrase.trim())?))
    }

    fn from_mnemonic(mnemonic: Mnemonic) -> Self {
        let seed = mnemonic.to_seed("");
        let secret = blake3::derive_key(USER_KEY_CONTEXT, &seed);
        Self {
            signing_key: SigningKey::from_bytes(&secret),
            mnemonic,
        }
    }

    pub fn public(&self) -> UserKey {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn phrase(&self) -> String {
        self.mnemonic.to_string()
    }

    pub fn certify_device(&self, device_id: EndpointId, olm_identity_key: OlmIdentityKey, issued_at_ms: u64) -> DeviceCert {
        let mut cert = DeviceCert {
            user_key: self.public(),
            device_id,
            olm_identity_key,
            issued_at_ms,
            signature: Signature::from_bytes(&[0; Signature::BYTE_SIZE]),
        };
        cert.signature = self.signing_key.sign(&cert.signed_bytes());
        cert
    }
}

/// Statement "device `device_id` with Olm identity `olm_identity_key` belongs to user
/// `user_key`", signed by the user key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCert {
    pub user_key: UserKey,
    pub device_id: EndpointId,
    pub olm_identity_key: OlmIdentityKey,
    pub issued_at_ms: u64,
    pub signature: Signature,
}

impl DeviceCert {
    fn signed_bytes(&self) -> Vec<u8> {
        [
            DEVICE_CERT_DOMAIN,
            &self.user_key,
            self.device_id.as_bytes(),
            &self.olm_identity_key,
            &self.issued_at_ms.to_le_bytes(),
        ]
        .concat()
    }

    pub fn verify(&self) -> bool {
        let Ok(key) = VerifyingKey::from_bytes(&self.user_key) else {
            return false;
        };
        key.verify_strict(&self.signed_bytes(), &self.signature).is_ok()
    }
}

/// What a device presents about its user to a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub display_name: String,
    pub device_cert: DeviceCert,
}

impl Profile {
    pub fn user_key(&self) -> UserKey {
        self.device_cert.user_key
    }

    /// Valid iff the cert is correctly signed and names the device we are actually talking to.
    pub fn is_valid_for(&self, remote_device: EndpointId) -> bool {
        self.device_cert.device_id == remote_device
            && self.device_cert.verify()
            && is_valid_display_name(&self.display_name)
    }
}

pub fn normalize_display_name(raw: &str) -> Option<String> {
    let name: String = raw.trim().chars().take(MAX_DISPLAY_NAME_CHARS).collect();
    is_valid_display_name(&name).then_some(name)
}

fn is_valid_display_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_DISPLAY_NAME_CHARS
        && !name.chars().any(char::is_control)
}

/// Human-comparable short hash of a user key, e.g. `ABCD-EFGH-IJKL-MNOP`.
pub fn fingerprint(user_key: &UserKey) -> String {
    let hash = blake3::hash(user_key);
    let encoded = data_encoding::BASE32_NOPAD.encode(&hash.as_bytes()[..FINGERPRINT_BYTES]);
    encoded
        .as_bytes()
        .chunks(FINGERPRINT_GROUP_CHARS)
        .map(|group| std::str::from_utf8(group).expect("base32 is ascii"))
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrase_roundtrip_restores_same_user_key() {
        let identity = UserIdentity::generate();
        let restored = UserIdentity::from_phrase(&identity.phrase()).unwrap();
        assert_eq!(identity.public(), restored.public());
    }

    #[test]
    fn device_cert_verifies_and_detects_tampering() {
        let identity = UserIdentity::generate();
        let device = iroh::SecretKey::generate().public();
        let mut cert = identity.certify_device(device, [3; 32], 42);
        assert!(cert.verify());
        cert.issued_at_ms += 1;
        assert!(!cert.verify());
        cert.issued_at_ms -= 1;
        cert.olm_identity_key = [4; 32];
        assert!(!cert.verify());
    }

    #[test]
    fn profile_rejects_foreign_device() {
        let identity = UserIdentity::generate();
        let device = iroh::SecretKey::generate().public();
        let other_device = iroh::SecretKey::generate().public();
        let profile = Profile {
            display_name: "alice".into(),
            device_cert: identity.certify_device(device, [3; 32], 1),
        };
        assert!(profile.is_valid_for(device));
        assert!(!profile.is_valid_for(other_device));
    }

    #[test]
    fn fingerprint_is_grouped() {
        let print = fingerprint(&[7u8; 32]);
        assert_eq!(print.len(), 16 + 3);
        assert_eq!(print.matches('-').count(), 3);
    }
}
