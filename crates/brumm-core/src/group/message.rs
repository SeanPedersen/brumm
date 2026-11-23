//! Group messages (SPEC.md §14.3): one ciphertext for the whole group, encrypted with the
//! sender device's hash-ratchet key of the current epoch and signed by that device, so every
//! member (and every forwarder) can tell which member device sent it. Pure.
//!
//! [`GroupWire`] is the unit that travels on gossip, in the member-served log and in Nostr
//! letters: a sealed [`GroupMessage`], a [`Rekey`], or a [`KeyRequest`] / [`KeyGrant`] pair
//! with which a device that missed an epoch gets it from any member (SPEC.md §14.3). All of
//! them are signed by a device; none carries a secret in the clear.

use ed25519_dalek::{Signature, SigningKey, ed25519::signature::Signer};
use serde::{Deserialize, Serialize};

use super::{
    BlobHash, DeviceKey, DeviceKeys, GroupId, GroupOp, OpHash,
    checkpoint::Checkpoint,
    epochs::Epoch,
    keys::{self, EpochId, Rekey, SLOT_BYTES, Secret, open_with_key, seal_with_key},
};
use crate::{
    identity::{DeviceList, verify_signature},
    wire::{self, ChatMessage, MessageId, Reaction},
};

const MESSAGE_DOMAIN: &[u8] = b"brumm/group-message/v1";
const KEY_REQUEST_DOMAIN: &[u8] = b"brumm/group-key-request/v1";
const KEY_GRANT_DOMAIN: &[u8] = b"brumm/group-key-grant/v1";
/// Largest encoded [`GroupWire`] on gossip and in the log (texts, ops, small rekeys).
pub const MAX_WIRE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMessage {
    pub group: GroupId,
    pub epoch: EpochId,
    pub sender_device: DeviceKey,
    pub counter: u32,
    /// Sender's clock; orders the log for catch-up (SPEC.md §14.4).
    pub sent_at_ms: u64,
    pub ciphertext: Vec<u8>,
    pub signature: Signature,
}

/// Append-only (wire compatibility).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupPayload {
    Chat(ChatMessage),
    /// The sender deletes messages it wrote.
    Delete(Vec<MessageId>),
    Reaction(Reaction),
    /// A file encrypted once with `key` (single-use, as for message keys); members fetch the
    /// ciphertext by `blob` from whoever has it.
    File {
        id: MessageId,
        sent_at_ms: u64,
        name: String,
        size: u64,
        blob: BlobHash,
        key: Secret,
    },
    /// A change to the group (also delivered to newcomers inside their welcome).
    Op(Box<GroupOp>),
    /// Inline checkpoint. Senders use [`GroupPayload::CheckpointBlob`]: a checkpoint of a group
    /// with more than a few hundred members does not fit a unit ([`MAX_WIRE_BYTES`]).
    Checkpoint(Box<Checkpoint>),
    /// The sender's user published a new device list.
    DeviceList(DeviceList),
    /// The sender device's (new) group key record.
    DeviceKeys(DeviceKeys),
    /// A checkpoint stored as a blob (the encoded [`Checkpoint`], `len` bytes); members fetch
    /// it from whoever has it.
    CheckpointBlob { blob: BlobHash, len: u64 },
}

/// Largest checkpoint a device fetches: a 10k-member group with 8 devices each, with room.
pub const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;

/// Append-only (wire compatibility).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupWire {
    Message(GroupMessage),
    Rekey(Rekey),
    KeyRequest(KeyRequest),
    KeyGrant(KeyGrant),
}

/// "I cannot read epoch `epoch`": any member may answer with a [`KeyGrant`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRequest {
    pub group: GroupId,
    pub epoch: EpochId,
    pub device: DeviceKey,
    pub at_ms: u64,
    pub signature: Signature,
}

impl KeyRequest {
    pub fn new(device_secret: &[u8; 32], group: GroupId, epoch: EpochId, at_ms: u64) -> Self {
        let signing = SigningKey::from_bytes(device_secret);
        let mut request = Self {
            group,
            epoch,
            device: signing.verifying_key().to_bytes(),
            at_ms,
            signature: Signature::from_bytes(&[0; Signature::BYTE_SIZE]),
        };
        request.signature = signing.sign(&request.signed_bytes());
        request
    }

    fn signed_bytes(&self) -> Vec<u8> {
        [
            KEY_REQUEST_DOMAIN,
            &self.group,
            &self.epoch,
            &self.device,
            &self.at_ms.to_le_bytes(),
        ]
        .concat()
    }

    pub fn verify(&self) -> bool {
        verify_signature(&self.device, &self.signed_bytes(), &self.signature)
    }
}

/// One epoch secret HPKE-sealed to one device (its slot, as in a rekey), plus what the device
/// needs to use it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyGrant {
    pub group: GroupId,
    pub epoch: EpochId,
    pub heads: Vec<OpHash>,
    pub parent: Option<EpochId>,
    pub to_device: DeviceKey,
    pub slot: Vec<u8>,
    pub author_device: DeviceKey,
    pub at_ms: u64,
    pub signature: Signature,
}

impl KeyGrant {
    pub fn new(
        device_secret: &[u8; 32],
        group: GroupId,
        epoch: &Epoch,
        to: &DeviceKeys,
        at_ms: u64,
    ) -> Self {
        let signing = SigningKey::from_bytes(device_secret);
        let slot = keys::build_slots(&group, &epoch.id, &epoch.secret, std::slice::from_ref(to));
        let mut grant = Self {
            group,
            epoch: epoch.id,
            heads: epoch.heads.clone(),
            parent: epoch.parent,
            to_device: to.device_id,
            slot,
            author_device: signing.verifying_key().to_bytes(),
            at_ms,
            signature: Signature::from_bytes(&[0; Signature::BYTE_SIZE]),
        };
        grant.signature = signing.sign(&grant.signed_bytes());
        grant
    }

    fn signed_bytes(&self) -> Vec<u8> {
        [
            KEY_GRANT_DOMAIN,
            &self.group,
            &self.epoch,
            &wire::encode(&self.heads),
            &wire::encode(&self.parent),
            &self.to_device,
            &self.slot,
            &self.author_device,
            &self.at_ms.to_le_bytes(),
        ]
        .concat()
    }

    pub fn verify(&self) -> bool {
        self.slot.len() == SLOT_BYTES
            && verify_signature(&self.author_device, &self.signed_bytes(), &self.signature)
    }

    /// The epoch, if this grant is for `device` and opens with its secret.
    pub fn open(&self, device: &DeviceKey, device_secret: &[u8; 32], now_ms: u64) -> Option<Epoch> {
        if self.to_device != *device {
            return None;
        }
        let secret = keys::open_slot(&self.group, &self.epoch, &self.slot, device, device_secret)?;
        Some(Epoch {
            id: self.epoch,
            secret,
            heads: self.heads.clone(),
            parent: self.parent,
            created_at_ms: now_ms,
        })
    }
}

impl GroupWire {
    pub fn group(&self) -> GroupId {
        match self {
            Self::Message(message) => message.group,
            Self::Rekey(rekey) => rekey.group,
            Self::KeyRequest(request) => request.group,
            Self::KeyGrant(grant) => grant.group,
        }
    }

    /// The device that signed it.
    pub fn author_device(&self) -> DeviceKey {
        match self {
            Self::Message(message) => message.sender_device,
            Self::Rekey(rekey) => rekey.author_device,
            Self::KeyRequest(request) => request.device,
            Self::KeyGrant(grant) => grant.author_device,
        }
    }

    pub fn sent_at_ms(&self) -> u64 {
        match self {
            Self::Message(message) => message.sent_at_ms,
            Self::Rekey(rekey) => rekey.at_ms,
            Self::KeyRequest(request) => request.at_ms,
            Self::KeyGrant(grant) => grant.at_ms,
        }
    }

    pub fn verify(&self) -> bool {
        match self {
            Self::Message(message) => message.verify(),
            Self::Rekey(rekey) => rekey.verify(),
            Self::KeyRequest(request) => request.verify(),
            Self::KeyGrant(grant) => grant.verify(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        wire::encode(self)
    }

    /// Log id: hash of the encoded unit (also the catch-up item id).
    pub fn id(encoded: &[u8]) -> [u8; 32] {
        *blake3::hash(encoded).as_bytes()
    }
}

impl GroupMessage {
    /// Encrypts `payload` with `key` (the sender chain's key for `counter`) and signs it.
    pub fn seal(
        device_secret: &[u8; 32],
        group: GroupId,
        epoch: EpochId,
        counter: u32,
        key: &Secret,
        sent_at_ms: u64,
        payload: &GroupPayload,
    ) -> Self {
        let signing = SigningKey::from_bytes(device_secret);
        let mut message = Self {
            group,
            epoch,
            sender_device: signing.verifying_key().to_bytes(),
            counter,
            sent_at_ms,
            ciphertext: Vec::new(),
            signature: Signature::from_bytes(&[0; Signature::BYTE_SIZE]),
        };
        message.ciphertext = seal_with_key(key, &message.header(), &wire::encode(payload));
        message.signature = signing.sign(&message.signed_bytes());
        message
    }

    /// Associated data: everything but the ciphertext and signature.
    fn header(&self) -> Vec<u8> {
        [
            MESSAGE_DOMAIN,
            &self.group,
            &self.epoch,
            &self.sender_device,
            &self.counter.to_le_bytes(),
            &self.sent_at_ms.to_le_bytes(),
        ]
        .concat()
    }

    fn signed_bytes(&self) -> Vec<u8> {
        [self.header(), self.ciphertext.clone()].concat()
    }

    /// The sender device's signature. Whether that device belongs to a member is the caller's check.
    pub fn verify(&self) -> bool {
        verify_signature(&self.sender_device, &self.signed_bytes(), &self.signature)
    }

    pub fn open(&self, key: &Secret) -> Option<GroupPayload> {
        let plaintext = open_with_key(key, &self.header(), &self.ciphertext)?;
        wire::decode(&plaintext).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        group::keys::{ChainState, random_secret},
        wire::Body,
    };

    #[test]
    fn a_message_opens_with_the_right_key_and_detects_tampering() {
        let device_secret = iroh::SecretKey::generate().to_bytes();
        let device = SigningKey::from_bytes(&device_secret).verifying_key().to_bytes();
        let epoch_secret = random_secret();
        let mut sender = ChainState::new(&epoch_secret, &device);
        let (counter, key) = sender.next_key();
        let payload = GroupPayload::Chat(ChatMessage {
            id: [1; 16],
            sent_at_ms: 5,
            body: Body::Text("hello group".into()),
        });
        let message = GroupMessage::seal(&device_secret, [2; 32], [3; 16], counter, &key, 5, &payload);
        assert!(message.verify());

        let mut receiver = ChainState::new(&epoch_secret, &device);
        let key = receiver.key_for(message.counter).unwrap();
        assert_eq!(message.open(&key), Some(payload));

        // A kicked device holding only an older epoch secret derives other keys.
        let mut outsider = ChainState::new(&random_secret(), &device);
        assert_eq!(message.open(&outsider.key_for(0).unwrap()), None);

        let mut forged = message.clone();
        forged.counter = 9;
        assert!(!forged.verify());
        assert_eq!(forged.open(&key), None, "the header is authenticated");
        let wire = GroupWire::Message(message);
        assert!(wire.verify());
        assert!(wire.encode().len() < MAX_WIRE_BYTES);
    }

    #[test]
    fn a_key_grant_opens_only_for_its_device() {
        use crate::group::keys::device_keypair;
        let granter = iroh::SecretKey::generate().to_bytes();
        let asker = iroh::SecretKey::generate().to_bytes();
        let asker_id = SigningKey::from_bytes(&asker).verifying_key().to_bytes();
        let asker_keys = DeviceKeys::sign(&asker, device_keypair(&asker).1, 1);
        let epoch = Epoch::root(random_secret(), vec![[1; 32]], 1);
        let request = KeyRequest::new(&asker, [2; 32], epoch.id, 3);
        assert!(request.verify() && request.device == asker_id);

        let grant = KeyGrant::new(&granter, [2; 32], &epoch, &asker_keys, 4);
        assert!(grant.verify());
        let opened = grant.open(&asker_id, &asker, 5).unwrap();
        assert_eq!((opened.secret, opened.heads.clone()), (epoch.secret, epoch.heads.clone()));
        assert!(grant.open(&asker_id, &granter, 5).is_none());
        let mut forged = grant.clone();
        forged.heads.push([9; 32]);
        assert!(!forged.verify());
    }
}
