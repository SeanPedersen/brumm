//! This device's end-to-end encryption state: the Olm account plus one session per peer
//! device, cached in memory and persisted (as encrypted pickles) after every state change.
//! Ratchet state must never be lost or reused, hence write-through persistence.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use iroh::EndpointId;
use vodozemac::olm::Session;

use crate::{
    crypto::{self, OlmDevice, OlmIdentityKey, OneTimeKey, PickleKey, Sealed},
    store::Store,
};

pub struct DeviceCrypto {
    olm: OlmDevice,
    pickle_key: PickleKey,
    sessions: HashMap<EndpointId, Session>,
}

impl DeviceCrypto {
    pub fn generate(device_secret: &[u8; 32]) -> Self {
        Self::new(OlmDevice::generate(), device_secret)
    }

    pub fn load(account_pickle: &str, device_secret: &[u8; 32]) -> Result<Self> {
        let pickle_key = crypto::pickle_key(device_secret);
        let olm = OlmDevice::from_pickle(account_pickle, &pickle_key).context("olm account unreadable")?;
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

    /// Starts a session towards a peer from its identity key + one-time key (not yet persisted:
    /// call [`Self::adopt`] once the peer is accepted as a contact).
    pub fn start_outbound(&self, their_identity: &OlmIdentityKey, their_one_time_key: &OneTimeKey) -> Result<Session> {
        Ok(self.olm.outbound_session(their_identity, their_one_time_key)?)
    }

    /// Accepts a pre-key message, consuming our one-time key. Returns the new session, the
    /// plaintext and the sender's Olm identity key.
    pub fn accept_inbound(&mut self, store: &Store, sealed: &Sealed) -> Result<(Session, Vec<u8>, OlmIdentityKey)> {
        let sender = sealed.pre_key_sender().context("expected a session-opening message")?;
        let (session, plaintext) = self.olm.inbound_session(&sender, sealed)?;
        store.save_olm_account(&self.account_pickle())?;
        Ok((session, plaintext, sender))
    }

    /// Persists and caches a session for `device`. The device must already be stored.
    pub fn adopt(&mut self, store: &Store, device: EndpointId, session: Session) -> Result<()> {
        store.save_session(&device, &crypto::pickle_session(&session, &self.pickle_key))?;
        self.sessions.insert(device, session);
        Ok(())
    }

    pub fn seal(&mut self, store: &Store, device: &EndpointId, plaintext: &[u8]) -> Result<Sealed> {
        let pickle_key = self.pickle_key;
        let session = self.session(store, device)?.context("no encrypted session with this device")?;
        let sealed = crypto::seal(session, plaintext)?;
        store.save_session(device, &crypto::pickle_session(session, &pickle_key))?;
        Ok(sealed)
    }

    /// Decrypts a message from `device`. If the peer started a fresh session (pre-key message
    /// from its certified identity key) and the existing one cannot decrypt, the new one replaces it.
    pub fn open(&mut self, store: &Store, device: &EndpointId, certified_identity: &OlmIdentityKey, sealed: &Sealed) -> Result<Vec<u8>> {
        let pickle_key = self.pickle_key;
        if let Some(session) = self.session(store, device)? {
            match crypto::open(session, sealed) {
                Ok(plaintext) => {
                    store.save_session(device, &crypto::pickle_session(session, &pickle_key))?;
                    return Ok(plaintext);
                }
                Err(err) if sealed.pre_key_sender().is_none() => return Err(err.into()),
                Err(err) => tracing::info!(%device, ?err, "existing session failed, trying new pre-key session"),
            }
        }
        if sealed.pre_key_sender().as_ref() != Some(certified_identity) {
            bail!("message is not from the device's certified identity key");
        }
        let (session, plaintext, _) = self.accept_inbound(store, sealed)?;
        self.adopt(store, *device, session)?;
        Ok(plaintext)
    }

    fn session(&mut self, store: &Store, device: &EndpointId) -> Result<Option<&mut Session>> {
        if !self.sessions.contains_key(device) {
            let Some(pickle) = store.session_pickle(device)? else {
                return Ok(None);
            };
            let session = crypto::unpickle_session(&pickle, &self.pickle_key).context("olm session unreadable")?;
            self.sessions.insert(*device, session);
        }
        Ok(self.sessions.get_mut(device))
    }
}
