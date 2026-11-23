//! Group keys (SPEC.md §14.3). Pure.
//!
//! - Every device has an X25519 key pair for groups, derived from its device secret and
//!   published as signed [`super::DeviceKeys`].
//! - An epoch is a random 32-byte secret. A [`Rekey`] delivers a new one: one HPKE slot per
//!   recipient device, laid out by sorted device id, so each device finds its own slot.
//! - An `Add` does not rekey: everyone ratchets the secret with the op's hash ([`ratchet_add`]),
//!   and the newcomer only gets the result, so it cannot read earlier messages.
//! - Messages use a hash ratchet per sender device ([`ChainState`]); used keys are forgotten.

use std::collections::BTreeMap;

use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};
use ed25519_dalek::{Signature, SigningKey, ed25519::signature::Signer};
use hpke::{
    Deserializable, Kem as _, OpModeR, OpModeS, Serializable, aead::ChaCha20Poly1305 as HpkeAead,
    kdf::HkdfSha256, kem::X25519HkdfSha256,
};
use serde::{Deserialize, Serialize};

use super::{BlobHash, DeviceKey, DeviceKeys, GroupId, OpHash};
use crate::{identity::verify_signature, wire};

pub type EpochId = [u8; 16];
pub type Secret = [u8; 32];

/// HPKE encapsulated key (32) + sealed secret (32) + tag (16).
pub const SLOT_BYTES: usize = 32 + 32 + 16;
/// Removals within this window share one rekey.
pub const REKEY_BATCH_WINDOW_MS: u64 = 30_000;
/// Old epochs stay readable this long after a newer one replaced them.
pub const EPOCH_GRACE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// Per device, a manual "heal" rekey is allowed at most this often.
pub const MIN_REKEY_INTERVAL_MS: u64 = 10 * 60 * 1000;
/// Message keys kept for messages that have not arrived yet (per sender chain).
pub const MAX_SKIPPED_KEYS: u32 = 2000;
/// Rekey blobs up to this size travel inside the record; larger ones are fetched as a blob.
pub const INLINE_REKEY_BYTES: usize = 16 * 1024;

const DEVICE_KEY_CONTEXT: &str = "brumm 2026 group device key v1";
const EPOCH_ID_CONTEXT: &str = "brumm 2026 group epoch id v1";
const ADD_RATCHET_CONTEXT: &str = "brumm 2026 group add ratchet v1";
const SENDER_CHAIN_CONTEXT: &str = "brumm 2026 group sender chain v1";
const MESSAGE_KEY_CONTEXT: &str = "brumm 2026 group message key v1";
const NEXT_CHAIN_CONTEXT: &str = "brumm 2026 group next chain v1";
const SLOT_INFO: &[u8] = b"brumm group epoch v1";
const REKEY_DOMAIN: &[u8] = b"brumm/group-rekey/v1";
const SECRET_BOX_NONCE_BYTES: usize = 12;

type Kem = X25519HkdfSha256;

/// This device's group key pair: `(secret, public)` bytes.
pub fn device_keypair(device_secret: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let ikm = blake3::derive_key(DEVICE_KEY_CONTEXT, device_secret);
    let (secret, public) = Kem::derive_keypair(&ikm);
    let mut secret_bytes = [0; 32];
    secret_bytes.copy_from_slice(&secret.to_bytes());
    let mut public_bytes = [0; 32];
    public_bytes.copy_from_slice(&public.to_bytes());
    (secret_bytes, public_bytes)
}

/// Public identifier of an epoch secret (it changes with every ratchet step).
pub fn epoch_id(secret: &Secret) -> EpochId {
    let hash = blake3::derive_key(EPOCH_ID_CONTEXT, secret);
    let mut id = EpochId::default();
    let width = id.len();
    id.copy_from_slice(&hash[..width]);
    id
}

/// The secret after an `Add` op (SPEC.md §14.3).
pub fn ratchet_add(secret: &Secret, op: &OpHash) -> Secret {
    blake3::derive_key(ADD_RATCHET_CONTEXT, &[secret.as_slice(), op].concat())
}

pub fn random_secret() -> Secret {
    rand::random()
}

fn slot_info(group: &GroupId, epoch: &EpochId) -> Vec<u8> {
    [SLOT_INFO, group, epoch].concat()
}

/// Recipients in slot order: sorted by device id, each once.
pub fn slot_order(recipients: &[DeviceKeys]) -> Vec<DeviceKeys> {
    let mut sorted: BTreeMap<DeviceKey, DeviceKeys> = BTreeMap::new();
    for keys in recipients {
        let newer = sorted
            .get(&keys.device_id)
            .is_none_or(|known| known.issued_at_ms < keys.issued_at_ms);
        if newer {
            sorted.insert(keys.device_id, keys.clone());
        }
    }
    sorted.into_values().collect()
}

/// `slot[0] ‖ slot[1] ‖ …` for `recipients` (already in [`slot_order`]). A recipient whose
/// public key is malformed gets an all-zero slot it cannot open (it asks for the key instead).
/// Native builds seal on all cores: a 30k-device rekey is seconds of HPKE on one.
pub fn build_slots(
    group: &GroupId,
    epoch: &EpochId,
    secret: &Secret,
    recipients: &[DeviceKeys],
) -> Vec<u8> {
    let info = slot_info(group, epoch);
    let seal_one = |keys: &DeviceKeys| -> [u8; SLOT_BYTES] {
        let mut slot = [0; SLOT_BYTES];
        let Ok(public) = <Kem as hpke::Kem>::PublicKey::from_bytes(&keys.hpke_pk) else {
            return slot;
        };
        let Ok((encapped, sealed)) = hpke::single_shot_seal::<HpkeAead, HkdfSha256, Kem>(
            &OpModeS::Base,
            &public,
            &info,
            secret,
            &keys.device_id,
        ) else {
            return slot;
        };
        slot[..32].copy_from_slice(&encapped.to_bytes());
        slot[32..].copy_from_slice(&sealed);
        slot
    };
    let mut blob = vec![0; recipients.len() * SLOT_BYTES];
    #[cfg(not(target_family = "wasm"))]
    {
        let threads = std::thread::available_parallelism().map_or(1, usize::from);
        let per_thread = recipients.len().div_ceil(threads).max(1);
        std::thread::scope(|scope| {
            for (chunk, out) in recipients
                .chunks(per_thread)
                .zip(blob.chunks_mut(per_thread * SLOT_BYTES))
            {
                scope.spawn(move || {
                    for (keys, slot) in chunk.iter().zip(out.chunks_mut(SLOT_BYTES)) {
                        slot.copy_from_slice(&seal_one(keys));
                    }
                });
            }
        });
    }
    #[cfg(target_family = "wasm")]
    for (keys, slot) in recipients.iter().zip(blob.chunks_mut(SLOT_BYTES)) {
        slot.copy_from_slice(&seal_one(keys));
    }
    blob
}

/// Opens this device's slot. `None` if it was not meant for us or was tampered with.
pub fn open_slot(
    group: &GroupId,
    epoch: &EpochId,
    slot: &[u8],
    device_id: &DeviceKey,
    device_secret: &[u8; 32],
) -> Option<Secret> {
    if slot.len() != SLOT_BYTES {
        return None;
    }
    let (secret_key, _) = device_keypair(device_secret);
    let secret_key = <Kem as hpke::Kem>::PrivateKey::from_bytes(&secret_key).ok()?;
    let encapped = <Kem as hpke::Kem>::EncappedKey::from_bytes(&slot[..32]).ok()?;
    let opened = hpke::single_shot_open::<HpkeAead, HkdfSha256, Kem>(
        &OpModeR::Base,
        &secret_key,
        &encapped,
        &slot_info(group, epoch),
        &slot[32..],
        device_id,
    )
    .ok()?;
    let secret: Secret = opened.try_into().ok()?;
    (epoch_id(&secret) == *epoch).then_some(secret)
}

/// Where `device` is in a blob laid out for `recipients` ([`slot_order`]), as a byte range.
pub fn slot_range(recipients: &[DeviceKeys], device: &DeviceKey) -> Option<std::ops::Range<usize>> {
    let index = recipients
        .binary_search_by(|keys| keys.device_id.cmp(device))
        .ok()?;
    Some(index * SLOT_BYTES..(index + 1) * SLOT_BYTES)
}

/// Slot order for the op state's `members_keys` without `excluded`, plus `extra`.
pub fn recipients(
    members_keys: &[DeviceKeys],
    excluded: &[DeviceKey],
    extra: &[DeviceKeys],
) -> Vec<DeviceKeys> {
    let mut all: Vec<DeviceKeys> = members_keys
        .iter()
        .filter(|keys| !excluded.contains(&keys.device_id))
        .cloned()
        .collect();
    all.extend(extra.iter().cloned());
    slot_order(&all)
}

/// A new epoch secret for the group, signed by the device that made it (SPEC.md §14.3).
/// Recipients are the device keys of the members at `membership` (in the op-log state there)
/// minus `excluded` (revoked devices) plus `extra` (devices linked since their `Add`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rekey {
    pub group: GroupId,
    pub epoch: EpochId,
    pub parent: Option<EpochId>,
    pub membership: Vec<OpHash>,
    pub excluded: Vec<DeviceKey>,
    pub extra: Vec<DeviceKeys>,
    pub blob: BlobHash,
    pub blob_len: u64,
    /// The slots themselves when small; otherwise fetched by `blob` from members.
    pub inline: Option<Vec<u8>>,
    pub author_device: DeviceKey,
    pub at_ms: u64,
    pub signature: Signature,
}

impl Rekey {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_secret: &[u8; 32],
        group: GroupId,
        secret: &Secret,
        parent: Option<EpochId>,
        membership: Vec<OpHash>,
        excluded: Vec<DeviceKey>,
        extra: Vec<DeviceKeys>,
        recipients: &[DeviceKeys],
        at_ms: u64,
    ) -> (Self, Vec<u8>) {
        let epoch = epoch_id(secret);
        let slots = build_slots(&group, &epoch, secret, recipients);
        let signing = SigningKey::from_bytes(device_secret);
        let mut rekey = Self {
            group,
            epoch,
            parent,
            membership,
            excluded,
            extra,
            blob: *blake3::hash(&slots).as_bytes(),
            blob_len: slots.len() as u64,
            inline: (slots.len() <= INLINE_REKEY_BYTES).then(|| slots.clone()),
            author_device: signing.verifying_key().to_bytes(),
            at_ms,
            signature: Signature::from_bytes(&[0; Signature::BYTE_SIZE]),
        };
        rekey.signature = signing.sign(&rekey.signed_bytes());
        (rekey, slots)
    }

    fn signed_bytes(&self) -> Vec<u8> {
        [
            REKEY_DOMAIN,
            &self.group,
            &self.epoch,
            &wire::encode(&self.parent),
            &wire::encode(&self.membership),
            &wire::encode(&self.excluded),
            &wire::encode(&self.extra),
            &self.blob,
            &self.blob_len.to_le_bytes(),
            &self.author_device,
            &self.at_ms.to_le_bytes(),
        ]
        .concat()
    }

    /// Signature by `author_device`, inline slots matching the blob hash, valid extra keys.
    /// Whether the author is a member device is the caller's check.
    pub fn verify(&self) -> bool {
        let inline_ok = self.inline.as_ref().is_none_or(|slots| {
            slots.len() as u64 == self.blob_len && *blake3::hash(slots).as_bytes() == self.blob
        });
        inline_ok
            && self.blob_len.is_multiple_of(SLOT_BYTES as u64)
            && self.extra.iter().all(DeviceKeys::verify)
            && verify_signature(&self.author_device, &self.signed_bytes(), &self.signature)
    }

    /// The recipients in slot order, given the device keys of the members at `membership`.
    pub fn recipients(&self, members_keys: &[DeviceKeys]) -> Vec<DeviceKeys> {
        recipients(members_keys, &self.excluded, &self.extra)
    }
}

/// One sender device's hash ratchet within an epoch: `chain₀ = KDF(secret, device)`,
/// `key_n = KDF(chain_n, "msg")`, `chain_{n+1} = KDF(chain_n, "chain")`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainState {
    chain: Secret,
    next: u32,
    /// Keys of skipped counters (messages still on the way), consumed when they arrive.
    skipped: BTreeMap<u32, Secret>,
}

impl ChainState {
    pub fn new(epoch_secret: &Secret, device: &DeviceKey) -> Self {
        Self {
            chain: blake3::derive_key(
                SENDER_CHAIN_CONTEXT,
                &[epoch_secret.as_slice(), device].concat(),
            ),
            next: 0,
            skipped: BTreeMap::new(),
        }
    }

    fn step(&mut self) -> Secret {
        let key = blake3::derive_key(MESSAGE_KEY_CONTEXT, &self.chain);
        self.chain = blake3::derive_key(NEXT_CHAIN_CONTEXT, &self.chain);
        self.next += 1;
        key
    }

    /// Sender: the next counter and its key.
    pub fn next_key(&mut self) -> (u32, Secret) {
        let counter = self.next;
        (counter, self.step())
    }

    /// Receiver: the key for `counter`, once. Keys of skipped counters are kept (up to
    /// [`MAX_SKIPPED_KEYS`]); a counter seen before or too far ahead gives `None`.
    pub fn key_for(&mut self, counter: u32) -> Option<Secret> {
        if counter < self.next {
            return self.skipped.remove(&counter);
        }
        if counter - self.next > MAX_SKIPPED_KEYS {
            return None;
        }
        while self.next < counter {
            let skipped_counter = self.next;
            let key = self.step();
            self.skipped.insert(skipped_counter, key);
        }
        while self.skipped.len() > MAX_SKIPPED_KEYS as usize {
            self.skipped.pop_first();
        }
        Some(self.step())
    }
}

/// AEAD with a single-use key (every message key is used once), so a fixed nonce is safe.
pub fn seal_with_key(key: &Secret, aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(&(*key).into());
    cipher
        .encrypt(&[0; SECRET_BOX_NONCE_BYTES].into(), Payload { msg: plaintext, aad })
        .expect("in-memory encryption cannot fail")
}

pub fn open_with_key(key: &Secret, aad: &[u8], ciphertext: &[u8]) -> Option<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(&(*key).into());
    cipher
        .decrypt(&[0; SECRET_BOX_NONCE_BYTES].into(), Payload { msg: ciphertext, aad })
        .ok()
}

/// Encrypts state at rest (epoch secrets, chains) with a long-lived key: random nonce first.
pub fn seal_at_rest(key: &Secret, plaintext: &[u8]) -> Vec<u8> {
    let nonce: [u8; SECRET_BOX_NONCE_BYTES] = rand::random();
    let cipher = ChaCha20Poly1305::new(&(*key).into());
    let mut sealed = nonce.to_vec();
    sealed.extend(
        cipher
            .encrypt(&nonce.into(), plaintext)
            .expect("in-memory encryption cannot fail"),
    );
    sealed
}

pub fn open_at_rest(key: &Secret, sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < SECRET_BOX_NONCE_BYTES {
        return None;
    }
    let (nonce, ciphertext) = sealed.split_at(SECRET_BOX_NONCE_BYTES);
    let nonce: [u8; SECRET_BOX_NONCE_BYTES] = nonce.try_into().ok()?;
    ChaCha20Poly1305::new(&(*key).into())
        .decrypt(&nonce.into(), ciphertext)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Device {
        secret: [u8; 32],
        keys: DeviceKeys,
    }

    fn device() -> Device {
        let secret: [u8; 32] = iroh::SecretKey::generate().to_bytes();
        let (_, public) = device_keypair(&secret);
        Device {
            keys: DeviceKeys::sign(&secret, public, 1),
            secret,
        }
    }

    #[test]
    fn every_recipient_opens_only_its_own_slot() {
        let devices: Vec<Device> = (0..5).map(|_| device()).collect();
        let outsider = device();
        let group = [1; 32];
        let secret = random_secret();
        let recipients = slot_order(&devices.iter().map(|d| d.keys.clone()).collect::<Vec<_>>());
        let (rekey, blob) = Rekey::new(
            &devices[0].secret,
            group,
            &secret,
            None,
            vec![[2; 32]],
            Vec::new(),
            Vec::new(),
            &recipients,
            7,
        );
        assert!(rekey.verify());
        assert_eq!(blob.len(), 5 * SLOT_BYTES);
        assert_eq!(rekey.inline.as_deref(), Some(blob.as_slice()));
        for device in &devices {
            let range = slot_range(&recipients, &device.keys.device_id).unwrap();
            let opened = open_slot(&group, &rekey.epoch, &blob[range.clone()], &device.keys.device_id, &device.secret);
            assert_eq!(opened, Some(secret));
            // Someone else's slot does not open.
            let other = if range.start == 0 { SLOT_BYTES..2 * SLOT_BYTES } else { 0..SLOT_BYTES };
            assert_eq!(open_slot(&group, &rekey.epoch, &blob[other], &device.keys.device_id, &device.secret), None);
        }
        assert!(slot_range(&recipients, &outsider.keys.device_id).is_none());
        for slot in blob.chunks(SLOT_BYTES) {
            assert_eq!(open_slot(&group, &rekey.epoch, slot, &outsider.keys.device_id, &outsider.secret), None);
        }
        let mut tampered = rekey.clone();
        tampered.membership.push([3; 32]);
        assert!(!tampered.verify());
    }

    #[test]
    fn recipients_drop_excluded_and_add_extra_devices() {
        let (a, b, c) = (device(), device(), device());
        let group = [1; 32];
        let (rekey, _) = Rekey::new(
            &a.secret,
            group,
            &random_secret(),
            None,
            Vec::new(),
            vec![b.keys.device_id],
            vec![c.keys.clone()],
            &[],
            1,
        );
        let recipients = rekey.recipients(&[a.keys.clone(), b.keys.clone()]);
        let ids: Vec<DeviceKey> = recipients.iter().map(|keys| keys.device_id).collect();
        let mut expected = vec![a.keys.device_id, c.keys.device_id];
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[test]
    fn the_add_ratchet_moves_forward_only() {
        let secret = random_secret();
        let after = ratchet_add(&secret, &[5; 32]);
        assert_ne!(epoch_id(&after), epoch_id(&secret));
        assert_eq!(after, ratchet_add(&secret, &[5; 32]));
        assert_ne!(after, ratchet_add(&secret, &[6; 32]));
    }

    #[test]
    fn skipped_and_out_of_order_messages_decrypt_once() {
        let secret = random_secret();
        let sender_id = [4; 32];
        let mut sender = ChainState::new(&secret, &sender_id);
        let keys: Vec<(u32, Secret)> = (0..5).map(|_| sender.next_key()).collect();
        let mut receiver = ChainState::new(&secret, &sender_id);
        // 3 arrives first, then 0, then 3 again (replay), then 1, 2, 4.
        assert_eq!(receiver.key_for(3), Some(keys[3].1));
        assert_eq!(receiver.key_for(0), Some(keys[0].1));
        assert_eq!(receiver.key_for(3), None);
        assert_eq!(receiver.key_for(0), None);
        assert_eq!(receiver.key_for(1), Some(keys[1].1));
        assert_eq!(receiver.key_for(2), Some(keys[2].1));
        assert_eq!(receiver.key_for(4), Some(keys[4].1));
        assert_eq!(receiver.key_for(4 + MAX_SKIPPED_KEYS + 2), None);
        // Another sender's chain is unrelated.
        assert_ne!(ChainState::new(&secret, &[5; 32]).next_key().1, keys[0].1);
    }

    #[test]
    fn sealing_round_trips() {
        let key = random_secret();
        let sealed = seal_with_key(&key, b"header", b"hello");
        assert_eq!(open_with_key(&key, b"header", &sealed).as_deref(), Some(&b"hello"[..]));
        assert_eq!(open_with_key(&key, b"other", &sealed), None);
        let at_rest = seal_at_rest(&key, b"secret");
        assert_eq!(open_at_rest(&key, &at_rest).as_deref(), Some(&b"secret"[..]));
        assert_eq!(open_at_rest(&random_secret(), &at_rest), None);
    }
}
