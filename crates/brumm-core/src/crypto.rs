//! End-to-end encryption: one vodozemac Olm (Double Ratchet) session per device pair.
//!
//! - A device's long-term Olm identity key is bound to the user via [`crate::identity::DeviceCert`].
//! - Sessions are bootstrapped from a one-time key carried in the invite (SPEC.md §5.1, §7).
//! - Accounts and sessions are persisted as pickles encrypted with a per-device pickle key
//!   (defense in depth on top of the SQLCipher database).

use serde::{Deserialize, Serialize};
use vodozemac::{
    Curve25519PublicKey,
    olm::{Account, AccountPickle, OlmMessage, Session, SessionConfig, SessionPickle},
};

const PICKLE_KEY_CONTEXT: &str = "brumm 2026 olm pickle key v1";
const ONE_TIME_KEYS_PER_INVITE: usize = 1;

pub type OlmIdentityKey = [u8; 32];
pub type OneTimeKey = [u8; 32];
pub type PickleKey = [u8; 32];

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("cannot create session: {0}")]
    Session(#[from] vodozemac::olm::SessionCreationError),
    #[error("cannot decrypt: {0}")]
    Decrypt(#[from] vodozemac::olm::DecryptionError),
    #[error("cannot encrypt: {0}")]
    Encrypt(#[from] vodozemac::olm::EncryptionError),
    #[error("malformed ciphertext: {0}")]
    Decode(#[from] vodozemac::DecodeError),
    #[error("corrupt pickle: {0}")]
    Pickle(#[from] vodozemac::PickleError),
    #[error("expected a pre-key message to start a session")]
    NotPreKey,
}

/// Olm ciphertext as it travels on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sealed {
    pub message_type: u8,
    pub ciphertext: Vec<u8>,
}

impl Sealed {
    fn from_olm(message: &OlmMessage) -> Self {
        let (message_type, ciphertext) = message.to_parts();
        Self {
            message_type: message_type as u8,
            ciphertext,
        }
    }

    fn to_olm(&self) -> Result<OlmMessage, CryptoError> {
        Ok(OlmMessage::from_parts(self.message_type.into(), &self.ciphertext)?)
    }

    /// Identity key of the sender, if this message can start a new session.
    pub fn pre_key_sender(&self) -> Option<OlmIdentityKey> {
        match self.to_olm().ok()? {
            OlmMessage::PreKey(message) => Some(message.identity_key().to_bytes()),
            OlmMessage::Normal(_) => None,
        }
    }
}

/// Derives the key used to encrypt Olm pickles from the device's secret key bytes.
pub fn pickle_key(device_secret: &[u8; 32]) -> PickleKey {
    blake3::derive_key(PICKLE_KEY_CONTEXT, device_secret)
}

/// This device's Olm account (identity key + one-time keys).
pub struct OlmDevice {
    account: Account,
}

impl OlmDevice {
    pub fn generate() -> Self {
        Self { account: Account::new() }
    }

    pub fn from_pickle(pickle: &str, key: &PickleKey) -> Result<Self, CryptoError> {
        let pickle = AccountPickle::from_encrypted(pickle, key)?;
        Ok(Self { account: Account::from_pickle(pickle) })
    }

    pub fn pickle(&self, key: &PickleKey) -> String {
        self.account.pickle().encrypt(key)
    }

    pub fn identity_key(&self) -> OlmIdentityKey {
        self.account.curve25519_key().to_bytes()
    }

    /// Generates a one-time key to hand out (e.g. in an invite). Persist the account afterwards.
    pub fn new_one_time_key(&mut self) -> OneTimeKey {
        let generated = self.account.generate_one_time_keys(ONE_TIME_KEYS_PER_INVITE);
        self.account.mark_keys_as_published();
        generated
            .created
            .first()
            .expect("generating one key yields one key")
            .to_bytes()
    }

    pub fn outbound_session(&self, their_identity: &OlmIdentityKey, their_one_time_key: &OneTimeKey) -> Result<Session, CryptoError> {
        Ok(self.account.create_outbound_session(
            SessionConfig::version_1(),
            Curve25519PublicKey::from_bytes(*their_identity),
            Curve25519PublicKey::from_bytes(*their_one_time_key),
        )?)
    }

    /// Creates the receiving side of a session from a pre-key message and returns its plaintext.
    /// Consumes the referenced one-time key: persist the account afterwards.
    pub fn inbound_session(&mut self, their_identity: &OlmIdentityKey, sealed: &Sealed) -> Result<(Session, Vec<u8>), CryptoError> {
        let OlmMessage::PreKey(message) = sealed.to_olm()? else {
            return Err(CryptoError::NotPreKey);
        };
        let created = self.account.create_inbound_session(
            SessionConfig::version_1(),
            Curve25519PublicKey::from_bytes(*their_identity),
            &message,
        )?;
        Ok((created.session, created.plaintext))
    }
}

pub fn seal(session: &mut Session, plaintext: &[u8]) -> Result<Sealed, CryptoError> {
    Ok(Sealed::from_olm(&session.encrypt(plaintext)?))
}

pub fn open(session: &mut Session, sealed: &Sealed) -> Result<Vec<u8>, CryptoError> {
    Ok(session.decrypt(&sealed.to_olm()?)?)
}

pub fn pickle_session(session: &Session, key: &PickleKey) -> String {
    session.pickle().encrypt(key)
}

pub fn unpickle_session(pickle: &str, key: &PickleKey) -> Result<Session, CryptoError> {
    Ok(Session::from_pickle(SessionPickle::from_encrypted(pickle, key)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_style_session_roundtrip() {
        let mut alice = OlmDevice::generate();
        let bob = OlmDevice::generate();
        let one_time_key = alice.new_one_time_key();

        let mut bob_session = bob.outbound_session(&alice.identity_key(), &one_time_key).unwrap();
        let hello = seal(&mut bob_session, b"hello").unwrap();
        assert_eq!(hello.pre_key_sender(), Some(bob.identity_key()));
        assert!(!hello.ciphertext.windows(5).any(|window| window == b"hello"));

        let (mut alice_session, plaintext) = alice.inbound_session(&bob.identity_key(), &hello).unwrap();
        assert_eq!(plaintext, b"hello");

        let reply = seal(&mut alice_session, b"hi bob").unwrap();
        assert_eq!(reply.pre_key_sender(), None);
        assert_eq!(open(&mut bob_session, &reply).unwrap(), b"hi bob");

        let second = seal(&mut bob_session, b"normal now").unwrap();
        assert_eq!(second.pre_key_sender(), None);
        assert_eq!(open(&mut alice_session, &second).unwrap(), b"normal now");
    }

    #[test]
    fn one_time_key_is_single_use() {
        let mut alice = OlmDevice::generate();
        let bob = OlmDevice::generate();
        let one_time_key = alice.new_one_time_key();
        let hello = seal(&mut bob.outbound_session(&alice.identity_key(), &one_time_key).unwrap(), b"1").unwrap();
        let replay = seal(&mut bob.outbound_session(&alice.identity_key(), &one_time_key).unwrap(), b"2").unwrap();
        alice.inbound_session(&bob.identity_key(), &hello).unwrap();
        assert!(alice.inbound_session(&bob.identity_key(), &replay).is_err());
    }

    #[test]
    fn pickles_roundtrip() {
        let key = pickle_key(&[5; 32]);
        let mut alice = OlmDevice::generate();
        let bob = OlmDevice::generate();
        let one_time_key = alice.new_one_time_key();
        let session = bob.outbound_session(&alice.identity_key(), &one_time_key).unwrap();

        let restored = OlmDevice::from_pickle(&alice.pickle(&key), &key).unwrap();
        assert_eq!(restored.identity_key(), alice.identity_key());

        let mut restored_session = unpickle_session(&pickle_session(&session, &key), &key).unwrap();
        assert!(seal(&mut restored_session, b"y").is_ok());
        assert!(OlmDevice::from_pickle(&alice.pickle(&key), &pickle_key(&[6; 32])).is_err());
    }
}
