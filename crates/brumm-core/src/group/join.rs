//! Joining a group and syncing groups between own devices (SPEC.md §14.6). These records
//! travel inside 1:1 Olm envelopes (`wire::Payload::Group*`), never on the group topic.
//!
//! 1. An owner/admin sends a contact a [`GroupInvitation`].
//! 2. The contact answers with an [`InvitationReply`]; accepting carries its signed consent,
//!    current profile and the group keys of the devices it knows.
//! 3. The admin signs the `Add`, posts it in the group, and sends the newcomer a
//!    [`GroupWelcome`]: the op log (checkpoint + ops), the epoch ratcheted past the `Add` (never
//!    an earlier one) and, if the group shares history, recent messages.
//! 4. The newcomer hands everything on to its own devices as a [`GroupSync`].
//!
//! The op log of a big group (≈ 15 MiB at 10k members) does not fit a Nostr letter, so a
//! welcome or sync carries it inline only when small ([`LogSource::Inline`]). Otherwise it names
//! where to get it ([`LogPointer`]): the checkpoint by hash and the ops after it from member
//! devices (a big group always has some online), and for a log up to
//! [`MAX_NOSTR_LOG_BYTES`] also as [`LogChunk`] letters on the relays, so a small group can be
//! joined while nobody is online.

use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};

use super::{
    BlobHash, DeviceKeys, GroupId, GroupOp, MAX_GROUP_NAME_CHARS, checkpoint::Checkpoint, epochs::Epoch,
};
use crate::{
    identity::{Profile, UserKey},
    wire::{Body, MessageId},
};

/// Days of history an admin shares with a newcomer when the group shares history.
pub const SHARED_HISTORY_DAYS: u64 = 7;
pub const MAX_SHARED_MESSAGES: usize = 200;
/// Encoded budget for shared history in one welcome, so that the welcome stays one Nostr
/// letter: about the last 60 short messages; bigger ones (files) are left out.
pub const MAX_SHARED_HISTORY_BYTES: usize = 12 * 1024;
/// An encoded op log up to this size travels inside the welcome (it then fits a letter).
pub const INLINE_LOG_BYTES: usize = 12 * 1024;
/// Largest op log also posted as Nostr chunks (≈ 35 letters, a group of a few hundred).
pub const MAX_NOSTR_LOG_BYTES: usize = 1024 * 1024;
/// Data per [`LogChunk`] letter (the letter limit is 32 KiB, minus the chunk header).
pub const LOG_CHUNK_BYTES: usize = 30 * 1024;
/// Member devices a pointer names (the sender's own device first).
pub const MAX_LOG_SOURCES: usize = 8;
/// Largest op log a device fetches or assembles.
pub const MAX_FETCHED_LOG_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInvitation {
    pub group: GroupId,
    pub name: String,
    pub member_count: u32,
    pub at_ms: u64,
}

impl GroupInvitation {
    pub fn is_valid(&self) -> bool {
        self.name.chars().count() <= MAX_GROUP_NAME_CHARS
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acceptance {
    pub profile: Profile,
    pub device_keys: Vec<DeviceKeys>,
    /// How often we left or were removed before (the admin's op log decides if it matches).
    pub rejoin: u32,
    pub consent: Signature,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvitationReply {
    pub group: GroupId,
    /// `None` declines.
    pub accept: Option<Acceptance>,
}

/// A message shared with a newcomer, under the admin's authority (not the original sender's).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedMessage {
    pub id: MessageId,
    pub sender: UserKey,
    pub sent_at_ms: u64,
    pub body: Body,
}

/// The op log as a newcomer or own device needs it: the latest checkpoint and the ops after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpLog {
    pub checkpoint: Option<Checkpoint>,
    pub ops: Vec<GroupOp>,
}

/// A blob by hash and length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub blob: BlobHash,
    pub len: u64,
}

/// The op log posted as Nostr letters under a tag only the recipient learns (from this record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkSet {
    /// Seed of the letters' tag ([`crate::mailbox::group_tag_keys`]).
    pub tag_seed: [u8; 32],
    /// Hash and length of the encoded [`OpLog`].
    pub log: BlobRef,
    pub count: u32,
    pub posted_at_secs: u64,
}

/// Where a newcomer (or own device) gets an op log too big to be sent along.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogPointer {
    /// The checkpoint (blob hash = [`Checkpoint::hash`]); members serve their current one.
    pub checkpoint: Option<BlobRef>,
    /// Member devices to ask, best first.
    pub sources: Vec<[u8; 32]>,
    pub chunks: Option<ChunkSet>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogSource {
    Inline(Box<OpLog>),
    Pointer(LogPointer),
}

/// One Nostr letter of a [`ChunkSet`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogChunk {
    /// The whole log's hash: which set this belongs to.
    pub log: BlobHash,
    pub index: u32,
    pub data: Vec<u8>,
}

/// Splits an encoded op log into the letters of a [`ChunkSet`].
pub fn split_log(log: &[u8], chunk_bytes: usize) -> Vec<LogChunk> {
    let hash = *blake3::hash(log).as_bytes();
    log.chunks(chunk_bytes.max(1))
        .enumerate()
        .map(|(index, data)| LogChunk {
            log: hash,
            index: index as u32,
            data: data.to_vec(),
        })
        .collect()
}

/// Collects the letters of one [`ChunkSet`] in any order.
#[derive(Debug, Clone)]
pub struct ChunkAssembly {
    log: BlobRef,
    parts: Vec<Option<Vec<u8>>>,
}

impl ChunkAssembly {
    pub fn new(set: &ChunkSet) -> Option<Self> {
        // Every part carries at least one byte.
        let sane = set.log.len <= MAX_FETCHED_LOG_BYTES && set.count > 0 && u64::from(set.count) <= set.log.len;
        sane.then(|| Self {
            log: set.log,
            parts: vec![None; set.count as usize],
        })
    }

    /// Adds a letter; returns the whole log once every part is there and it matches the hash.
    pub fn insert(&mut self, chunk: LogChunk) -> Option<Vec<u8>> {
        if chunk.log != self.log.blob {
            return None;
        }
        let slot = self.parts.get_mut(chunk.index as usize)?;
        *slot = Some(chunk.data);
        if self.parts.iter().any(Option::is_none) {
            return None;
        }
        let whole: Vec<u8> = self.parts.iter().flatten().flatten().copied().collect();
        let complete = whole.len() as u64 == self.log.len && *blake3::hash(&whole).as_bytes() == self.log.blob;
        if !complete {
            // A forged or stale part: start over, the relays still hold the real ones.
            self.parts.iter_mut().for_each(|part| *part = None);
            return None;
        }
        Some(whole)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupWelcome {
    pub group: GroupId,
    pub log: LogSource,
    pub epoch: Epoch,
    pub history: Vec<SharedMessage>,
}

/// Own-device sync of one group: its log, every epoch secret we hold, and whether we are
/// still in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSync {
    pub group: GroupId,
    pub log: LogSource,
    pub epochs: Vec<Epoch>,
    pub active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_for(log: &[u8], count: usize) -> ChunkSet {
        ChunkSet {
            tag_seed: [7; 32],
            log: BlobRef {
                blob: *blake3::hash(log).as_bytes(),
                len: log.len() as u64,
            },
            count: count as u32,
            posted_at_secs: 0,
        }
    }

    #[test]
    fn chunks_reassemble_in_any_order() {
        let log: Vec<u8> = (0..10_000u32).flat_map(u32::to_le_bytes).collect();
        let mut chunks = split_log(&log, 4096);
        assert_eq!(chunks.len(), log.len().div_ceil(4096));
        let mut assembly = ChunkAssembly::new(&set_for(&log, chunks.len())).unwrap();
        chunks.reverse();
        let last = chunks.pop().unwrap();
        for chunk in chunks {
            assert_eq!(assembly.insert(chunk), None);
        }
        assert_eq!(assembly.insert(last), Some(log));
    }

    #[test]
    fn a_forged_part_is_rejected_and_the_real_one_still_completes() {
        let log = vec![1u8; 9000];
        let chunks = split_log(&log, 4096);
        let mut assembly = ChunkAssembly::new(&set_for(&log, chunks.len())).unwrap();
        let mut forged = chunks[0].clone();
        forged.data[0] ^= 1;
        assert_eq!(assembly.insert(forged), None);
        assert_eq!(assembly.insert(chunks[1].clone()), None);
        assert_eq!(assembly.insert(chunks[2].clone()), None);
        for chunk in chunks.clone() {
            if let Some(whole) = assembly.insert(chunk) {
                assert_eq!(whole, log);
                return;
            }
        }
        panic!("the real parts never completed the log");
    }

    #[test]
    fn a_set_for_another_log_is_ignored() {
        let log = vec![2u8; 100];
        let mut assembly = ChunkAssembly::new(&set_for(&log, 1)).unwrap();
        let other = split_log(&[3u8; 100], 4096).remove(0);
        assert_eq!(assembly.insert(other), None);
    }
}
