//! This device's end-to-end encryption state: the Olm account plus the Olm sessions with
//! every peer device, cached in memory and persisted (as encrypted pickles) after every
//! state change. Ratchet state must never be lost or reused, hence write-through persistence.
//!
//! Several sessions per device are kept: if two devices open sessions to each other at the
//! same time, each side keeps both and encrypts with the one it last received on, so they
//! converge (same approach as Matrix clients).

use std::collections::HashMap;

use anyhow::{Context, Result};
use iroh::EndpointId;
use vodozemac::olm::Session;

use crate::{
    clock::now_ms,
    crypto::{self, OlmDevice, OlmIdentityKey, OneTimeKey, PickleKey, Sealed},
    identity::DeviceEntry,
    store::Store,
};

const MAX_SESSIONS_PER_DEVICE: usize = 4;

struct ActiveSession {
    id: String,
    session: Session,
    last_used_ms: u64,
}

pub struct DeviceCrypto {
    olm: OlmDevice,
    pickle_key: PickleKey,
    /// Per device, most recently used first. Loaded lazily from the store.
    sessions: HashMap<EndpointId, Vec<ActiveSession>>,
}

impl DeviceCrypto {
    /// New account plus its first fallback key (to publish in our device entry).
    pub fn generate(device_secret: &[u8; 32]) -> (Self, OneTimeKey) {
        let mut crypto = Self::new(OlmDevice::generate(), device_secret);
        let fallback_key = crypto.olm.rotate_fallback_key();
        (crypto, fallback_key)
    }

    pub fn load(account_pickle: &str, device_secret: &[u8; 32]) -> Result<Self> {
        let pickle_key = crypto::pickle_key(device_secret);
        let olm = OlmDevice::from_pickle(account_pickle, &pickle_key)
            .context("olm account unreadable")?;
        Ok(Self::new(olm, device_secret))
    }

    fn new(olm: OlmDevice, device_secret: &[u8; 32]) -> Self {
        Self {
            olm,
            pickle_key: crypto::pickle_key(device_secret),
            sessions: HashMap::new(),
        }
    }

    pub fn identity_key(&self) -> OlmIdentityKey {
        self.olm.identity_key()
    }

    pub fn account_pickle(&self) -> String {
        self.olm.pickle(&self.pickle_key)
    }

    pub fn new_one_time_key(&mut self, store: &Store) -> Result<OneTimeKey> {
        let key = self.olm.new_one_time_key();
        store.save_olm_account(&self.account_pickle())?;
        Ok(key)
    }

    /// One-time key without persisting the account (for a device that has no store yet).
    pub fn new_one_time_key_unsaved(&mut self) -> OneTimeKey {
        self.olm.new_one_time_key()
    }

    /// Starts a session towards a peer (not yet persisted: call [`Self::adopt`] once accepted).
    pub fn start_outbound(
        &self,
        their_identity: &OlmIdentityKey,
        their_key: &OneTimeKey,
    ) -> Result<Session> {
        Ok(self.olm.outbound_session(their_identity, their_key)?)
    }

    /// Accepts a pre-key message (consuming a one-time key, if one was used). Returns the new
    /// session, the plaintext and the sender's Olm identity key. Persist the account afterwards
    /// via [`Self::save_account`] when no store existed yet.
    pub fn accept_inbound_unsaved(
        &mut self,
        sealed: &Sealed,
    ) -> Result<(Session, Vec<u8>, OlmIdentityKey)> {
        let sender = sealed
            .pre_key_sender()
            .context("expected a session-opening message")?;
        let (session, plaintext) = self.olm.inbound_session(&sender, sealed)?;
        Ok((session, plaintext, sender))
    }

    pub fn accept_inbound(
        &mut self,
        store: &Store,
        sealed: &Sealed,
    ) -> Result<(Session, Vec<u8>, OlmIdentityKey)> {
        let accepted = self.accept_inbound_unsaved(sealed)?;
        self.save_account(store)?;
        Ok(accepted)
    }

    pub fn save_account(&self, store: &Store) -> Result<()> {
        store.save_olm_account(&self.account_pickle())
    }

    /// Persists and caches a session for `device` as its most recent one.
    pub fn adopt(&mut self, store: &Store, device: EndpointId, session: Session) -> Result<()> {
        self.load_sessions(store, &device)?;
        let active = ActiveSession {
            id: crypto::session_id(&session),
            session,
            last_used_ms: now_ms(),
        };
        self.persist(store, &device, &active)?;
        let sessions = self.sessions.entry(device).or_default();
        sessions.retain(|existing| existing.id != active.id);
        sessions.insert(0, active);
        for dropped in sessions.split_off(sessions.len().min(MAX_SESSIONS_PER_DEVICE)) {
            store.delete_session(&device, &dropped.id)?;
        }
        Ok(())
    }

    /// Encrypts for `peer`, opening a session from its fallback key if none exists yet.
    pub fn seal(&mut self, store: &Store, peer: &DeviceEntry, plaintext: &[u8]) -> Result<Sealed> {
        let device = peer.device_id();
        self.load_sessions(store, &device)?;
        if self.sessions.get(&device).is_none_or(Vec::is_empty) {
            let session = self.start_outbound(&peer.cert.olm_identity_key, &peer.fallback_key)?;
            self.adopt(store, device, session)?;
        }
        let pickle_key = self.pickle_key;
        let active = self
            .sessions
            .get_mut(&device)
            .and_then(|sessions| sessions.first_mut())
            .expect("session ensured above");
        let sealed = crypto::seal(&mut active.session, plaintext)?;
        store.save_session(
            &device,
            &active.id,
            &crypto::pickle_session(&active.session, &pickle_key),
            active.last_used_ms,
        )?;
        Ok(sealed)
    }

    /// Decrypts a message from `device`, trying every known session. A pre-key message from the
    /// device's *certified* identity key that no session can decrypt opens a new session.
    /// `Ok(None)`: nothing here can decrypt it (a final answer, unlike a storage error).
    pub fn open(
        &mut self,
        store: &Store,
        device: &EndpointId,
        certified_identity: &OlmIdentityKey,
        sealed: &Sealed,
    ) -> Result<Option<Vec<u8>>> {
        self.load_sessions(store, device)?;
        if let Some(plaintext) = self.try_existing(store, device, sealed)? {
            return Ok(Some(plaintext));
        }
        if sealed.pre_key_sender().as_ref() != Some(certified_identity) {
            return Ok(None);
        }
        let Ok((session, plaintext, _)) = self.accept_inbound_unsaved(sealed) else {
            return Ok(None);
        };
        self.save_account(store)?;
        self.adopt(store, *device, session)?;
        Ok(Some(plaintext))
    }

    /// Opens a fresh session from the peer's fallback key and makes it the one `seal` uses.
    /// For a peer that reported it cannot decrypt what the current session produces: its first
    /// messages are pre-key messages, which the peer can always open via its fallback key.
    pub fn restart_session(&mut self, store: &Store, peer: &DeviceEntry) -> Result<()> {
        let session = self.start_outbound(&peer.cert.olm_identity_key, &peer.fallback_key)?;
        self.adopt(store, peer.device_id(), session)
    }

    fn try_existing(
        &mut self,
        store: &Store,
        device: &EndpointId,
        sealed: &Sealed,
    ) -> Result<Option<Vec<u8>>> {
        let pickle_key = self.pickle_key;
        let Some(sessions) = self.sessions.get_mut(device) else {
            return Ok(None);
        };
        let Some((index, plaintext)) =
            sessions.iter_mut().enumerate().find_map(|(index, active)| {
                crypto::open(&mut active.session, sealed)
                    .ok()
                    .map(|plaintext| (index, plaintext))
            })
        else {
            return Ok(None);
        };
        let mut active = sessions.remove(index);
        active.last_used_ms = now_ms();
        store.save_session(
            device,
            &active.id,
            &crypto::pickle_session(&active.session, &pickle_key),
            active.last_used_ms,
        )?;
        sessions.insert(0, active);
        Ok(Some(plaintext))
    }

    pub fn forget_device(&mut self, store: &Store, device: &EndpointId) -> Result<()> {
        self.sessions.remove(device);
        store.delete_sessions_of(device)
    }

    fn persist(&self, store: &Store, device: &EndpointId, active: &ActiveSession) -> Result<()> {
        store.save_session(
            device,
            &active.id,
            &crypto::pickle_session(&active.session, &self.pickle_key),
            active.last_used_ms,
        )
    }

    fn load_sessions(&mut self, store: &Store, device: &EndpointId) -> Result<()> {
        if self.sessions.contains_key(device) {
            return Ok(());
        }
        let mut loaded = Vec::new();
        for (id, pickle, last_used_ms) in store.sessions_of(device)? {
            let session = crypto::unpickle_session(&pickle, &self.pickle_key)
                .context("olm session unreadable")?;
            loaded.push(ActiveSession {
                id,
                session,
                last_used_ms,
            });
        }
        loaded.sort_by_key(|active| std::cmp::Reverse(active.last_used_ms));
        self.sessions.insert(*device, loaded);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::UserIdentity;

    struct Peer {
        store: Store,
        crypto: DeviceCrypto,
        entry: DeviceEntry,
    }

    fn peer() -> Peer {
        let secret = iroh::SecretKey::generate();
        let (crypto, fallback_key) = DeviceCrypto::generate(&secret.to_bytes());
        let user = UserIdentity::generate();
        let entry = DeviceEntry {
            cert: user.certify_device(secret.public(), crypto.identity_key(), 0),
            fallback_key,
            mailbox: crate::mailbox::address(&secret.to_bytes(), crate::mailbox::default_relays()),
            label: "test".into(),
        };
        let store = Store::open_in_memory().unwrap();
        crypto.save_account(&store).unwrap();
        Peer {
            store,
            crypto,
            entry,
        }
    }

    fn deliver(from: &mut Peer, to: &mut Peer, text: &[u8]) -> Result<Vec<u8>> {
        let sealed = from.crypto.seal(&from.store, &to.entry, text)?;
        let sender = from.entry.device_id();
        let identity = from.entry.cert.olm_identity_key;
        to.crypto
            .open(&to.store, &sender, &identity, &sealed)?
            .context("undecryptable")
    }

    #[test]
    fn simultaneous_session_creation_converges() {
        let (mut alice, mut bob) = (peer(), peer());
        // Both open a session from the other's fallback key before hearing anything back.
        let from_alice = alice.crypto.seal(&alice.store, &bob.entry, b"a1").unwrap();
        let from_bob = bob.crypto.seal(&bob.store, &alice.entry, b"b1").unwrap();
        let (alice_id, bob_id) = (alice.entry.device_id(), bob.entry.device_id());
        let alice_identity = alice.entry.cert.olm_identity_key;
        let bob_identity = bob.entry.cert.olm_identity_key;
        assert_eq!(
            bob.crypto
                .open(&bob.store, &alice_id, &alice_identity, &from_alice)
                .unwrap()
                .unwrap(),
            b"a1"
        );
        assert_eq!(
            alice
                .crypto
                .open(&alice.store, &bob_id, &bob_identity, &from_bob)
                .unwrap()
                .unwrap(),
            b"b1"
        );

        for round in 0..5u8 {
            assert_eq!(deliver(&mut alice, &mut bob, &[round]).unwrap(), [round]);
            assert_eq!(
                deliver(&mut bob, &mut alice, &[round, 1]).unwrap(),
                [round, 1]
            );
        }
    }

    #[test]
    fn sessions_survive_reload_from_store() {
        let (mut alice, mut bob) = (peer(), peer());
        assert_eq!(deliver(&mut alice, &mut bob, b"hello").unwrap(), b"hello");
        assert_eq!(deliver(&mut bob, &mut alice, b"hi").unwrap(), b"hi");

        bob.crypto.sessions.clear();
        alice.crypto.sessions.clear();
        assert_eq!(
            deliver(&mut alice, &mut bob, b"after reload").unwrap(),
            b"after reload"
        );
    }

    #[test]
    fn foreign_identity_cannot_open_session() {
        let (mut alice, mut bob, mut mallory) = (peer(), peer(), peer());
        // Mallory seals with her own identity but claims to be Alice's device.
        let sealed = mallory
            .crypto
            .seal(&mallory.store, &bob.entry, b"evil")
            .unwrap();
        let alice_identity = alice.entry.cert.olm_identity_key;
        let opened = bob.crypto.open(
            &bob.store,
            &alice.entry.device_id(),
            &alice_identity,
            &sealed,
        );
        assert!(matches!(opened, Ok(None)));
        assert_eq!(deliver(&mut alice, &mut bob, b"real").unwrap(), b"real");
    }

    /// Bob dropped the session Alice still encrypts with (evicted, or a restored backup):
    /// Alice's next message is undecryptable until she restarts the session, then it converges.
    #[test]
    fn a_lost_session_is_reported_and_recovered_by_a_restart() {
        let (mut alice, mut bob) = (peer(), peer());
        assert_eq!(deliver(&mut alice, &mut bob, b"one").unwrap(), b"one");
        // Bob replies, so Alice switches from pre-key to normal messages.
        assert_eq!(deliver(&mut bob, &mut alice, b"two").unwrap(), b"two");
        let alice_id = alice.entry.device_id();
        bob.crypto.forget_device(&bob.store, &alice_id).unwrap();

        let stale = alice
            .crypto
            .seal(&alice.store, &bob.entry, b"lost")
            .unwrap();
        let identity = alice.entry.cert.olm_identity_key;
        let opened = bob.crypto.open(&bob.store, &alice_id, &identity, &stale);
        assert!(matches!(opened, Ok(None)), "a final answer, not an error");

        alice
            .crypto
            .restart_session(&alice.store, &bob.entry)
            .unwrap();
        let resealed = alice
            .crypto
            .seal(&alice.store, &bob.entry, b"lost")
            .unwrap();
        assert_eq!(
            bob.crypto
                .open(&bob.store, &alice_id, &identity, &resealed)
                .unwrap()
                .unwrap(),
            b"lost"
        );
        for round in 0..3u8 {
            assert_eq!(deliver(&mut bob, &mut alice, &[round]).unwrap(), [round]);
            assert_eq!(
                deliver(&mut alice, &mut bob, &[round, 1]).unwrap(),
                [round, 1]
            );
        }
    }

    /// Pre-key messages of a session the receiver evicted still open via the fallback key.
    #[test]
    fn a_delayed_pre_key_message_opens_after_its_session_was_evicted() {
        let (mut alice, mut bob) = (peer(), peer());
        let first = alice
            .crypto
            .seal(&alice.store, &bob.entry, b"delayed")
            .unwrap();
        let (alice_id, identity) = (alice.entry.device_id(), alice.entry.cert.olm_identity_key);
        assert!(
            bob.crypto
                .open(&bob.store, &alice_id, &identity, &first)
                .unwrap()
                .is_some()
        );
        bob.crypto.forget_device(&bob.store, &alice_id).unwrap();
        let second = alice
            .crypto
            .seal(&alice.store, &bob.entry, b"later")
            .unwrap();
        assert_eq!(
            bob.crypto
                .open(&bob.store, &alice_id, &identity, &second)
                .unwrap()
                .unwrap(),
            b"later"
        );
    }
}
