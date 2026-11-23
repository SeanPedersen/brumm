//! Groups (SPEC.md §14): types shared by the pure group modules and the node.
//!
//! - [`oplog`]: the signed op log and its deterministic fold into a [`GroupState`]
//! - [`checkpoint`]: signed snapshots that let a newcomer skip replaying the whole log
//! - [`keys`]: device group keys, epochs, rekey blobs and per-sender hash ratchets
//! - [`message`]: sealed, device-signed group messages and their payloads
//! - [`sync`]: range-based set reconciliation for catching up on a group's log
//!
//! Everything here is pure (no I/O). Signatures and other static checks run once when an op
//! arrives ([`GroupOp::verify`]); the fold itself only applies the rules, so refolding a
//! 10k-member log needs no cryptography.

#[cfg(test)]
mod bench;
pub mod checkpoint;
pub mod epochs;
pub mod join;
pub mod keys;
pub mod message;
pub mod oplog;
pub mod sync;

use ed25519_dalek::{Signature, SigningKey, ed25519::signature::Signer};
use serde::{Deserialize, Serialize};

pub use crate::conversation::GroupId;
use crate::{
    identity::{Profile, UserIdentity, UserKey, is_valid_display_name, verify_signature},
    wire,
};

/// blake3 of an encoded [`GroupOp`].
pub type OpHash = [u8; 32];
/// blake3 of a blob's bytes (rekey blob, file, group photo).
pub type BlobHash = [u8; 32];
pub type DeviceKey = [u8; 32];

pub const MAX_GROUP_MEMBERS: usize = 10_000;
pub const MAX_GROUP_NAME_CHARS: usize = 64;
/// An op names the heads it saw; more than this many concurrent branches is not plausible.
pub const MAX_OP_PARENTS: usize = 64;

const OP_DOMAIN: &[u8] = b"brumm/group-op/v1";
const CONSENT_DOMAIN: &[u8] = b"brumm/group-consent/v1";
const DEVICE_KEYS_DOMAIN: &[u8] = b"brumm/group-device-keys/v1";
const GROUP_ID_CONTEXT: &str = "brumm 2026 group id v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Role {
    Owner,
    Admin,
    Member,
}

/// One change to a group, signed by its author's user key (SPEC.md §14.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupOp {
    pub group: GroupId,
    /// The heads the author saw. Empty only for `Create`.
    pub parents: Vec<OpHash>,
    pub at_ms: u64,
    pub author: UserKey,
    pub kind: OpKind,
    pub signature: Signature,
}

/// Append-only (wire compatibility).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpKind {
    /// The group id is derived from the creator's key and `nonce` ([`group_id`]), so nobody
    /// else can write a `Create` for it.
    Create {
        name: String,
        announcement: bool,
        nonce: [u8; 16],
        creator: Profile,
        device_keys: Vec<DeviceKeys>,
    },
    /// `consent` is the newcomer's own signature over the group and `rejoin` (how often the
    /// user left or was removed before), so nobody is added without accepting, and an old
    /// consent cannot add someone again after they left.
    Add {
        member: Profile,
        device_keys: Vec<DeviceKeys>,
        rejoin: u32,
        consent: Signature,
    },
    Remove {
        user: UserKey,
    },
    Leave,
    SetRole {
        user: UserKey,
        admin: bool,
    },
    Rename {
        name: String,
    },
    SetAnnouncement(bool),
    SetAvatar {
        blob: Option<BlobHash>,
    },
    SetHistorySharing(bool),
}

/// A device's X25519 key for group key delivery, signed by the device's own (iroh) key, which
/// the user's device list certifies (SPEC.md §14.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceKeys {
    pub device_id: DeviceKey,
    pub hpke_pk: [u8; 32],
    pub issued_at_ms: u64,
    pub signature: Signature,
}

impl DeviceKeys {
    pub fn sign(device_secret: &[u8; 32], hpke_pk: [u8; 32], issued_at_ms: u64) -> Self {
        let signing = SigningKey::from_bytes(device_secret);
        let device_id = signing.verifying_key().to_bytes();
        let signature = signing.sign(&device_keys_bytes(&device_id, &hpke_pk, issued_at_ms));
        Self {
            device_id,
            hpke_pk,
            issued_at_ms,
            signature,
        }
    }

    pub fn verify(&self) -> bool {
        verify_signature(
            &self.device_id,
            &device_keys_bytes(&self.device_id, &self.hpke_pk, self.issued_at_ms),
            &self.signature,
        )
    }
}

fn device_keys_bytes(device_id: &DeviceKey, hpke_pk: &[u8; 32], issued_at_ms: u64) -> Vec<u8> {
    [
        DEVICE_KEYS_DOMAIN,
        device_id,
        hpke_pk,
        &issued_at_ms.to_le_bytes(),
    ]
    .concat()
}

/// The id of the group `creator` makes with `nonce`.
pub fn group_id(creator: &UserKey, nonce: &[u8; 16]) -> GroupId {
    blake3::derive_key(GROUP_ID_CONTEXT, &[creator.as_slice(), nonce].concat())
}

/// The bytes a newcomer signs to accept joining `group` (see [`OpKind::Add`]).
pub fn consent_bytes(group: &GroupId, rejoin: u32) -> Vec<u8> {
    [CONSENT_DOMAIN, group, &rejoin.to_le_bytes()].concat()
}

pub fn sign_consent(user: &UserIdentity, group: &GroupId, rejoin: u32) -> Signature {
    user.sign(&consent_bytes(group, rejoin))
}

pub fn is_valid_group_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.chars().count() <= MAX_GROUP_NAME_CHARS
        && !name.chars().any(char::is_control)
}

impl GroupOp {
    /// Signs a new op with `author`'s user key.
    pub fn new(
        author: &UserIdentity,
        group: GroupId,
        parents: Vec<OpHash>,
        at_ms: u64,
        kind: OpKind,
    ) -> Self {
        let mut op = Self {
            group,
            parents,
            at_ms,
            author: author.public(),
            kind,
            signature: Signature::from_bytes(&[0; Signature::BYTE_SIZE]),
        };
        op.signature = author.sign(&op.signed_bytes());
        op
    }

    fn signed_bytes(&self) -> Vec<u8> {
        [
            OP_DOMAIN,
            &self.group,
            &wire::encode(&self.parents),
            &self.at_ms.to_le_bytes(),
            &self.author,
            &wire::encode(&self.kind),
        ]
        .concat()
    }

    pub fn hash(&self) -> OpHash {
        *blake3::hash(&wire::encode(self)).as_bytes()
    }

    /// Everything that does not depend on the group's state: the author's signature, the
    /// creator binding of `Create`, consent, device lists and device keys. Run once on receipt;
    /// [`oplog::fold`] trusts it.
    pub fn verify(&self) -> bool {
        if self.parents.len() > MAX_OP_PARENTS
            || !verify_signature(&self.author, &self.signed_bytes(), &self.signature)
        {
            return false;
        }
        match &self.kind {
            OpKind::Create {
                name,
                nonce,
                creator,
                device_keys,
                ..
            } => {
                self.parents.is_empty()
                    && group_id(&self.author, nonce) == self.group
                    && is_valid_group_name(name)
                    && creator.user_key() == self.author
                    && valid_profile(creator, device_keys)
            }
            OpKind::Add {
                member,
                device_keys,
                rejoin,
                consent,
            } => {
                !self.parents.is_empty()
                    && valid_profile(member, device_keys)
                    && verify_signature(
                        &member.user_key(),
                        &consent_bytes(&self.group, *rejoin),
                        consent,
                    )
            }
            OpKind::Rename { name } => !self.parents.is_empty() && is_valid_group_name(name),
            _ => !self.parents.is_empty(),
        }
    }
}

/// A member's profile with a device key record for some of its listed devices.
fn valid_profile(profile: &Profile, device_keys: &[DeviceKeys]) -> bool {
    is_valid_display_name(&profile.display_name)
        && profile.devices.verify()
        && device_keys.iter().all(|keys| {
            keys.verify()
                && profile
                    .devices
                    .entries
                    .iter()
                    .any(|entry| entry.device_id().as_bytes() == &keys.device_id)
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use iroh::SecretKey;

    use super::*;
    use crate::identity::tests::entry;

    /// A test user with one device; returns its identity, profile and device secret.
    pub(crate) struct TestUser {
        pub identity: UserIdentity,
        pub profile: Profile,
        pub device_secret: [u8; 32],
    }

    impl TestUser {
        pub(crate) fn new(name: &str) -> Self {
            let identity = UserIdentity::generate();
            let device = SecretKey::generate();
            let profile = Profile {
                display_name: name.into(),
                devices: identity.sign_device_list(1, vec![entry(&identity, device.public())]),
            };
            Self {
                identity,
                profile,
                device_secret: device.to_bytes(),
            }
        }

        pub(crate) fn key(&self) -> UserKey {
            self.identity.public()
        }

        pub(crate) fn device_keys(&self) -> Vec<DeviceKeys> {
            vec![DeviceKeys::sign(&self.device_secret, [7; 32], 1)]
        }

        pub(crate) fn create(&self, name: &str) -> GroupOp {
            let nonce = [9; 16];
            GroupOp::new(
                &self.identity,
                group_id(&self.key(), &nonce),
                Vec::new(),
                1,
                OpKind::Create {
                    name: name.into(),
                    announcement: false,
                    nonce,
                    creator: self.profile.clone(),
                    device_keys: self.device_keys(),
                },
            )
        }

        pub(crate) fn op(&self, group: GroupId, parents: Vec<OpHash>, kind: OpKind) -> GroupOp {
            GroupOp::new(&self.identity, group, parents, 2, kind)
        }

        /// The `Add` kind for this user, consenting with `rejoin`.
        pub(crate) fn add_kind(&self, group: &GroupId, rejoin: u32) -> OpKind {
            OpKind::Add {
                member: self.profile.clone(),
                device_keys: self.device_keys(),
                rejoin,
                consent: sign_consent(&self.identity, group, rejoin),
            }
        }
    }

    #[test]
    fn static_checks_catch_forgeries() {
        let ada = TestUser::new("Ada");
        let bob = TestUser::new("Bob");
        let create = ada.create("Chess");
        assert!(create.verify());
        let group = create.group;

        // Bob cannot write a Create for Ada's group id.
        let forged = GroupOp::new(
            &bob.identity,
            group,
            Vec::new(),
            1,
            match create.kind.clone() {
                OpKind::Create {
                    name,
                    announcement,
                    nonce,
                    device_keys,
                    ..
                } => OpKind::Create {
                    name,
                    announcement,
                    nonce,
                    creator: bob.profile.clone(),
                    device_keys,
                },
                _ => unreachable!(),
            },
        );
        assert!(!forged.verify());

        let add = ada.op(group, vec![create.hash()], bob.add_kind(&group, 0));
        assert!(add.verify());
        // A consent for another group (or rejoin count) does not count.
        let mut stale = add.clone();
        if let OpKind::Add { rejoin, .. } = &mut stale.kind {
            *rejoin = 1;
        }
        let resigned = ada.op(group, vec![create.hash()], stale.kind);
        assert!(!resigned.verify());

        let mut tampered = add;
        tampered.at_ms += 1;
        assert!(!tampered.verify());
    }
}
