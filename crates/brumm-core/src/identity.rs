//! Identity layer: a user key derived from a BIP-39 seed phrase, per-device iroh keys,
//! device certificates and the signed, versioned device list of a user (SPEC.md §3).
//!
//! The user key is the stable identity contacts know; device keys are what iroh dials.
//! A user's devices are exactly those in the device list with the highest `seq`.

use bip39::Mnemonic;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::{
    crypto::{OlmIdentityKey, OneTimeKey},
    mailbox::MailboxAddress,
};

const USER_KEY_CONTEXT: &str = "brumm 2026 user identity key v1";
const DEVICE_CERT_DOMAIN: &[u8] = b"brumm/device-cert/v2";
const DEVICE_LIST_DOMAIN: &[u8] = b"brumm/device-list/v1";
pub const MAX_DEVICES: usize = 8;
pub const MAX_DEVICE_LABEL_CHARS: usize = 32;
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

    pub fn certify_device(
        &self,
        device_id: EndpointId,
        olm_identity_key: OlmIdentityKey,
        issued_at_ms: u64,
    ) -> DeviceCert {
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

    pub fn sign_device_list(&self, seq: u64, entries: Vec<DeviceEntry>) -> DeviceList {
        let mut list = DeviceList {
            user_key: self.public(),
            seq,
            entries,
            signature: Signature::from_bytes(&[0; Signature::BYTE_SIZE]),
        };
        list.signature = self.signing_key.sign(&list.signed_bytes());
        list
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
        key.verify_strict(&self.signed_bytes(), &self.signature)
            .is_ok()
    }
}

/// One device in a [`DeviceList`]: its cert, the Olm fallback key others use to open a
/// session without a one-time key, its Nostr mailbox and a human label ("Linux", "Phone").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub cert: DeviceCert,
    pub fallback_key: OneTimeKey,
    pub mailbox: MailboxAddress,
    pub label: String,
}

impl DeviceEntry {
    pub fn device_id(&self) -> EndpointId {
        self.cert.device_id
    }
}

/// All devices of a user, signed by the user key. Higher `seq` supersedes lower.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceList {
    pub user_key: UserKey,
    pub seq: u64,
    pub entries: Vec<DeviceEntry>,
    pub signature: Signature,
}

impl DeviceList {
    fn signed_bytes(&self) -> Vec<u8> {
        [
            DEVICE_LIST_DOMAIN,
            &self.user_key,
            &self.seq.to_le_bytes(),
            &crate::wire::encode(&self.entries),
        ]
        .concat()
    }

    pub fn verify(&self) -> bool {
        let Ok(key) = VerifyingKey::from_bytes(&self.user_key) else {
            return false;
        };
        let mut seen = std::collections::HashSet::new();
        let entries_valid = self.entries.iter().all(|entry| {
            entry.cert.user_key == self.user_key
                && entry.cert.verify()
                && entry.label.chars().count() <= MAX_DEVICE_LABEL_CHARS
                && entry.mailbox.is_valid()
                && seen.insert(entry.device_id())
        });
        !self.entries.is_empty()
            && self.entries.len() <= MAX_DEVICES
            && entries_valid
            && key
                .verify_strict(&self.signed_bytes(), &self.signature)
                .is_ok()
    }

    pub fn entry(&self, device_id: &EndpointId) -> Option<&DeviceEntry> {
        self.entries
            .iter()
            .find(|entry| &entry.device_id() == device_id)
    }
}

/// What a device presents about its user to a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub display_name: String,
    pub devices: DeviceList,
}

impl Profile {
    pub fn user_key(&self) -> UserKey {
        self.devices.user_key
    }

    /// Valid iff the device list is correctly signed and contains the device we talk to.
    pub fn is_valid_for(&self, remote_device: EndpointId) -> bool {
        is_valid_display_name(&self.display_name)
            && self.devices.verify()
            && self.devices.entry(&remote_device).is_some()
    }
}

/// Default label for this device, e.g. "Linux".
/// Shown for a device whose platform has no OS name (`std::env::consts::OS` is empty in wasm).
const BROWSER_DEVICE_LABEL: &str = "Browser";

pub fn default_device_label() -> String {
    let os = std::env::consts::OS;
    if cfg!(target_family = "wasm") || os.is_empty() {
        return BROWSER_DEVICE_LABEL.to_owned();
    }
    let mut label: String = os.chars().take(MAX_DEVICE_LABEL_CHARS).collect();
    if let Some(first) = label.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    match label.as_str() {
        "Macos" => "macOS".to_owned(),
        _ => label,
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
pub(crate) mod tests {
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

    pub(crate) fn entry(identity: &UserIdentity, device: EndpointId) -> DeviceEntry {
        DeviceEntry {
            cert: identity.certify_device(device, [3; 32], 1),
            fallback_key: [4; 32],
            mailbox: crate::mailbox::address(&[5; 32], crate::mailbox::default_relays()),
            label: "Linux".into(),
        }
    }

    #[test]
    fn profile_rejects_foreign_device() {
        let identity = UserIdentity::generate();
        let device = iroh::SecretKey::generate().public();
        let other_device = iroh::SecretKey::generate().public();
        let profile = Profile {
            display_name: "alice".into(),
            devices: identity.sign_device_list(1, vec![entry(&identity, device)]),
        };
        assert!(profile.is_valid_for(device));
        assert!(!profile.is_valid_for(other_device));
    }

    #[test]
    fn device_list_rejects_tampering_and_foreign_certs() {
        let alice = UserIdentity::generate();
        let mallory = UserIdentity::generate();
        let device = iroh::SecretKey::generate().public();
        let mut list = alice.sign_device_list(3, vec![entry(&alice, device)]);
        assert!(list.verify());
        list.seq = 4;
        assert!(!list.verify());

        let foreign = mallory.sign_device_list(1, vec![entry(&alice, device)]);
        assert!(!foreign.verify());

        let duplicate =
            alice.sign_device_list(1, vec![entry(&alice, device), entry(&alice, device)]);
        assert!(!duplicate.verify());
    }

    #[test]
    fn fingerprint_is_grouped() {
        let print = fingerprint(&[7u8; 32]);
        assert_eq!(print.len(), 16 + 3);
        assert_eq!(print.matches('-').count(), 3);
    }
}
