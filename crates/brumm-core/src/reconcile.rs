//! Own-device consistency check (anti-entropy), the pure part: which messages two devices of
//! the same user disagree on. Delivery is reliable, but a lost envelope (a bug, a wiped session,
//! a relay that dropped a letter) would otherwise leave a permanent gap. This makes every pair
//! of own devices converge on the same set of messages, whatever the cause.
//!
//! Three steps, each one envelope (see `node::reconcile` for the transport):
//! 1. A → B [`SyncDigest`]: per (conversation, week) a hash over the sorted message ids.
//! 2. B → A [`SyncHave`]: for every bucket whose hash differs, all ids B holds there.
//! 3. A → B the messages B lacks ([`Repair::send`]). Ids only B holds ([`Repair::they_have`]) are
//!    either deleted on A (A tells B to delete them) or arrive with B's own round.
//!
//! Only ids are compared, never bodies: message ids are random and a message never changes.

use std::collections::BTreeMap;

use crate::{
    identity::UserKey,
    store::IndexedMessage,
    wire::{BucketHash, MessageId, SyncBucket, SyncBucketIds, SyncDigest, SyncHave},
};

/// Bucket width. Recent weeks change, older ones match, so a mismatch stays small.
pub const BUCKET_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const BUCKET_HASH_CONTEXT: &str = "brumm 2026 sync bucket v1";

type BucketKey = (UserKey, u32);

/// Sorted ids per bucket.
fn buckets(index: &[IndexedMessage]) -> BTreeMap<BucketKey, Vec<MessageId>> {
    let mut buckets: BTreeMap<BucketKey, Vec<MessageId>> = BTreeMap::new();
    for message in index {
        let week = u32::try_from(message.sent_at_ms / BUCKET_MS).unwrap_or(u32::MAX);
        buckets
            .entry((message.contact, week))
            .or_default()
            .push(message.id);
    }
    for ids in buckets.values_mut() {
        ids.sort_unstable();
        ids.dedup();
    }
    buckets
}

fn bucket_hash(ids: &[MessageId]) -> BucketHash {
    let mut hasher = blake3::Hasher::new_derive_key(BUCKET_HASH_CONTEXT);
    for id in ids {
        hasher.update(id);
    }
    let mut hash = BucketHash::default();
    let width = hash.len();
    hash.copy_from_slice(&hasher.finalize().as_bytes()[..width]);
    hash
}

/// Step 1. `index` must hold exactly the messages of the window that `since_ms` describes.
pub fn digest(since_ms: u64, index: &[IndexedMessage]) -> SyncDigest {
    SyncDigest {
        since_ms,
        buckets: buckets(index)
            .into_iter()
            .map(|((contact, week), ids)| SyncBucket {
                contact,
                week,
                hash: bucket_hash(&ids),
            })
            .collect(),
    }
}

/// Step 2: our ids for every bucket that differs from `theirs` (also buckets only one side
/// has). `None` when both hold the same messages. `index` must use `theirs.since_ms`.
pub fn answer(theirs: &SyncDigest, index: &[IndexedMessage]) -> Option<SyncHave> {
    let mut ours = buckets(index);
    let mut differing = Vec::new();
    for bucket in &theirs.buckets {
        let key = (bucket.contact, bucket.week);
        let ids = ours.remove(&key).unwrap_or_default();
        if bucket_hash(&ids) != bucket.hash {
            differing.push(bucket_ids(key, ids));
        }
    }
    // Buckets they do not have at all.
    differing.extend(ours.into_iter().map(|(key, ids)| bucket_ids(key, ids)));
    (!differing.is_empty()).then_some(SyncHave {
        since_ms: theirs.since_ms,
        buckets: differing,
    })
}

fn bucket_ids((contact, week): BucketKey, ids: Vec<MessageId>) -> SyncBucketIds {
    SyncBucketIds { contact, week, ids }
}

/// Step 3: what to do about the buckets the other device reported.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Repair {
    /// Ours, missing there: send them.
    pub send: Vec<(UserKey, Vec<MessageId>)>,
    /// Theirs, missing here: deleted here (they must delete too), or new to us.
    pub they_have: Vec<(UserKey, Vec<MessageId>)>,
}

/// `index` must use `theirs.since_ms`.
pub fn repair(theirs: &SyncHave, index: &[IndexedMessage]) -> Repair {
    let ours = buckets(index);
    let mut send: BTreeMap<UserKey, Vec<MessageId>> = BTreeMap::new();
    let mut they_have: BTreeMap<UserKey, Vec<MessageId>> = BTreeMap::new();
    for bucket in &theirs.buckets {
        let mine = ours
            .get(&(bucket.contact, bucket.week))
            .map_or(&[][..], Vec::as_slice);
        let missing_there = mine.iter().filter(|id| !bucket.ids.contains(id));
        send.entry(bucket.contact)
            .or_default()
            .extend(missing_there);
        let missing_here = bucket
            .ids
            .iter()
            .filter(|id| mine.binary_search(id).is_err());
        they_have
            .entry(bucket.contact)
            .or_default()
            .extend(missing_here);
    }
    let non_empty = |map: BTreeMap<UserKey, Vec<MessageId>>| {
        map.into_iter().filter(|(_, ids)| !ids.is_empty()).collect()
    };
    Repair {
        send: non_empty(send),
        they_have: non_empty(they_have),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTES: UserKey = [1; 32];
    const BOB: UserKey = [2; 32];

    fn at(contact: UserKey, id: u8, sent_at_ms: u64) -> IndexedMessage {
        IndexedMessage {
            contact,
            id: [id; 16],
            sent_at_ms,
        }
    }

    /// Runs steps 1–3 from `a` to `b`; returns what `a` sends and what only `b` has.
    fn round(a: &[IndexedMessage], b: &[IndexedMessage]) -> Repair {
        match answer(&digest(0, a), b) {
            Some(have) => repair(&have, a),
            None => Repair::default(),
        }
    }

    #[test]
    fn equal_sets_need_no_answer_whatever_the_order() {
        let a = vec![at(NOTES, 1, 5), at(NOTES, 2, 6), at(BOB, 3, BUCKET_MS * 3)];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(answer(&digest(0, &a), &b), None);
    }

    #[test]
    fn a_missing_message_is_found_in_its_bucket_only() {
        let old = (0..50).map(|id| at(NOTES, id, u64::from(id)));
        let a: Vec<_> = old.clone().chain([at(NOTES, 99, BUCKET_MS * 10)]).collect();
        let b: Vec<_> = old.collect();
        let have = answer(&digest(0, &a), &b).expect("they differ");
        assert_eq!(
            have.buckets,
            vec![SyncBucketIds {
                contact: NOTES,
                week: 10,
                ids: vec![],
            }],
            "the old, matching week is not listed"
        );
        assert_eq!(
            repair(&have, &a),
            Repair {
                send: vec![(NOTES, vec![[99; 16]])],
                they_have: vec![],
            }
        );
    }

    #[test]
    fn both_sides_missing_something_in_the_same_bucket() {
        let a = vec![at(BOB, 1, 10), at(BOB, 2, 20)];
        let b = vec![at(BOB, 2, 20), at(BOB, 3, 30)];
        assert_eq!(
            round(&a, &b),
            Repair {
                send: vec![(BOB, vec![[1; 16]])],
                they_have: vec![(BOB, vec![[3; 16]])],
            }
        );
        // The reverse round finds the mirror image.
        assert_eq!(
            round(&b, &a),
            Repair {
                send: vec![(BOB, vec![[3; 16]])],
                they_have: vec![(BOB, vec![[1; 16]])],
            }
        );
    }

    #[test]
    fn a_bucket_only_the_answering_side_has_is_reported() {
        let a = vec![at(NOTES, 1, 10)];
        let b = vec![at(NOTES, 1, 10), at(BOB, 7, BUCKET_MS * 2)];
        assert_eq!(
            round(&a, &b),
            Repair {
                send: vec![],
                they_have: vec![(BOB, vec![[7; 16]])],
            }
        );
    }

    #[test]
    fn same_ids_in_different_chats_are_different_messages() {
        let a = vec![at(NOTES, 1, 10)];
        let b = vec![at(BOB, 1, 10)];
        assert_eq!(
            round(&a, &b),
            Repair {
                send: vec![(NOTES, vec![[1; 16]])],
                they_have: vec![(BOB, vec![[1; 16]])],
            }
        );
    }

    /// Repeated rounds in both directions, applying each repair, always end in equal sets.
    #[test]
    fn rounds_in_both_directions_converge() {
        let mut a: Vec<IndexedMessage> = (0..40u8)
            .filter(|id| id % 3 != 0)
            .map(|id| {
                at(
                    if id % 2 == 0 { NOTES } else { BOB },
                    id,
                    u64::from(id) * BUCKET_MS / 4,
                )
            })
            .collect();
        let mut b: Vec<IndexedMessage> = (0..40u8)
            .filter(|id| id % 5 != 0)
            .map(|id| {
                at(
                    if id % 2 == 0 { NOTES } else { BOB },
                    id,
                    u64::from(id) * BUCKET_MS / 4,
                )
            })
            .collect();
        let lookup = |all: &[IndexedMessage], contact: UserKey, id: MessageId| {
            *all.iter()
                .find(|m| m.contact == contact && m.id == id)
                .expect("sender has it")
        };
        for from_a in [true, false] {
            let (src, dst) = if from_a { (&a, &b) } else { (&b, &a) };
            let fix = round(src, dst);
            let arriving: Vec<IndexedMessage> = fix
                .send
                .iter()
                .flat_map(|(contact, ids)| ids.iter().map(|id| lookup(src, *contact, *id)))
                .collect();
            if from_a {
                b.extend(arriving)
            } else {
                a.extend(arriving)
            }
        }
        assert_eq!(answer(&digest(0, &a), &b), None);
        assert_eq!(answer(&digest(0, &b), &a), None);
    }
}
