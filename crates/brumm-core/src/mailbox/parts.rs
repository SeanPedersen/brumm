//! Letters larger than one relay event travel as numbered parts (SPEC.md §8): each part is a
//! letter of its own under the same `p` tag, and the receiver puts them back together in any
//! order. A part names the whole by its blake3 digest, which is checked once all parts are in.
//! Pure: no I/O, no clocks; the caller passes the post time of each part.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

/// The most parts one letter may have: 64 × 24 KiB = 1.5 MiB, a burst public relays accept.
pub const MAX_PARTS: usize = 64;
/// Parts of unfinished letters kept in memory; the oldest letter is dropped past this.
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LetterPart {
    pub digest: [u8; 32],
    pub index: u16,
    pub count: u16,
    pub bytes: Vec<u8>,
}

/// `whole` cut into parts of at most `part_bytes`, or `None` when it needs more than
/// [`MAX_PARTS`].
pub fn split(whole: &[u8], part_bytes: usize) -> Option<Vec<LetterPart>> {
    let part_bytes = part_bytes.max(1);
    let count = whole.len().div_ceil(part_bytes).max(1);
    if count > MAX_PARTS {
        return None;
    }
    let digest = *blake3::hash(whole).as_bytes();
    let count = u16::try_from(count).ok()?;
    Some(
        whole
            .chunks(part_bytes)
            .enumerate()
            .map(|(index, bytes)| LetterPart {
                digest,
                index: index as u16,
                count,
                bytes: bytes.to_vec(),
            })
            .collect(),
    )
}

struct Pending {
    parts: Vec<Option<Vec<u8>>>,
    received: usize,
    bytes: usize,
    /// Earliest post time among the parts, so a restart re-fetches all of them.
    posted_at_secs: u64,
}

/// Collects parts until a letter is complete. Bounded by [`MAX_PENDING_BYTES`].
#[derive(Default)]
pub struct Assembler {
    pending: HashMap<[u8; 32], Pending>,
    /// Digests in arrival order of their first part, for eviction.
    order: VecDeque<[u8; 32]>,
    bytes: usize,
}

impl Assembler {
    /// The whole letter and its earliest post time once `part` completes it. Malformed parts,
    /// duplicates and a whole that does not match its digest are dropped.
    pub fn insert(
        &mut self,
        part: LetterPart,
        part_bytes: usize,
        posted_at_secs: u64,
    ) -> Option<(Vec<u8>, u64)> {
        let count = usize::from(part.count);
        let index = usize::from(part.index);
        let well_formed = (1..=MAX_PARTS).contains(&count)
            && index < count
            && !part.bytes.is_empty()
            && part.bytes.len() <= part_bytes;
        if !well_formed {
            return None;
        }
        let pending = self.pending.entry(part.digest).or_insert_with(|| {
            self.order.push_back(part.digest);
            Pending {
                parts: vec![None; count],
                received: 0,
                bytes: 0,
                posted_at_secs,
            }
        });
        if pending.parts.len() != count || pending.parts[index].is_some() {
            return None;
        }
        pending.posted_at_secs = pending.posted_at_secs.min(posted_at_secs);
        pending.bytes += part.bytes.len();
        self.bytes += part.bytes.len();
        pending.parts[index] = Some(part.bytes);
        pending.received += 1;
        if pending.received < count {
            self.evict_over_budget(&part.digest);
            return None;
        }
        let pending = self.remove(&part.digest)?;
        let whole: Vec<u8> = pending.parts.into_iter().flatten().flatten().collect();
        (blake3::hash(&whole).as_bytes() == &part.digest).then_some((whole, pending.posted_at_secs))
    }

    fn remove(&mut self, digest: &[u8; 32]) -> Option<Pending> {
        let pending = self.pending.remove(digest)?;
        self.bytes -= pending.bytes;
        self.order.retain(|queued| queued != digest);
        Some(pending)
    }

    /// Drops the oldest unfinished letters (never `keep`, the one just added to) over budget.
    fn evict_over_budget(&mut self, keep: &[u8; 32]) {
        while self.bytes > MAX_PENDING_BYTES {
            let Some(oldest) = self.order.iter().find(|digest| *digest != keep).copied() else {
                return;
            };
            self.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PART: usize = 1000;

    fn whole(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    #[test]
    fn parts_reassemble_in_any_order() {
        let data = whole(3500);
        let mut parts = split(&data, PART).expect("fits");
        assert_eq!(parts.len(), 4);
        parts.reverse();
        let mut assembler = Assembler::default();
        let mut done = None;
        for (time, part) in parts.into_iter().enumerate() {
            assert!(done.is_none(), "complete before the last part");
            done = assembler.insert(part, PART, 100 - time as u64);
        }
        assert_eq!(done, Some((data, 97)));
        assert_eq!(assembler.bytes, 0);
    }

    #[test]
    fn duplicates_and_tampering_are_dropped() {
        let mut parts = split(&whole(2500), PART).expect("fits");
        let mut assembler = Assembler::default();
        assert!(assembler.insert(parts[0].clone(), PART, 1).is_none());
        assert!(assembler.insert(parts[0].clone(), PART, 1).is_none());
        parts[1].bytes[0] ^= 1;
        assert!(assembler.insert(parts[1].clone(), PART, 1).is_none());
        assert!(
            assembler.insert(parts[2].clone(), PART, 1).is_none(),
            "digest mismatch"
        );
        assert!(assembler.pending.is_empty());
    }

    #[test]
    fn malformed_parts_and_oversized_letters_are_refused() {
        assert!(split(&whole(PART * MAX_PARTS + 1), PART).is_none());
        let mut part = split(&whole(10), PART).expect("fits").remove(0);
        part.index = 1;
        assert!(Assembler::default().insert(part, PART, 0).is_none());
    }

    #[test]
    fn unfinished_letters_are_evicted_oldest_first() {
        let mut assembler = Assembler::default();
        let part_bytes = 1024 * 1024;
        for seed in 0..20u8 {
            let data = vec![seed; part_bytes * 2];
            let first = split(&data, part_bytes).expect("fits").remove(0);
            assembler.insert(first, part_bytes, 0);
        }
        assert!(assembler.bytes <= MAX_PENDING_BYTES);
        assert_eq!(assembler.pending.len(), assembler.order.len());
    }
}
