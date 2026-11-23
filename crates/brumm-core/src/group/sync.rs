//! Range-based set reconciliation (as in Nostr's negentropy) over a group's log, keyed by
//! `(sent_at_ms, id)` (SPEC.md §14.4). Pure.
//!
//! Both sides run the same [`respond`] on what the other sent. A range whose fingerprint
//! matches is done; a differing one is split into [`BRANCHES`] sub-ranges, or listed as ids
//! once it is small. The bytes exchanged grow with what is missing, not with the log: two
//! devices that differ by a handful of messages settle in a few round trips of short frames.
//!
//! Chosen over the `negentropy` crate: the protocol is a few dozen lines, has no extra
//! dependency, builds for wasm as is, and the frames are our own postcard types.

use serde::{Deserialize, Serialize};

pub type ItemId = [u8; 32];

/// One log entry as seen by reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Item {
    pub sent_at_ms: u64,
    pub id: ItemId,
}

/// Ranges are `[lower, upper)` in `(sent_at_ms, id)` order.
pub const MIN_BOUND: Item = Item {
    sent_at_ms: 0,
    id: [0; 32],
};
pub const MAX_BOUND: Item = Item {
    sent_at_ms: u64::MAX,
    id: [0xff; 32],
};
/// Sub-ranges per differing range.
pub const BRANCHES: usize = 16;
/// A range with at most this many items is sent as an id list instead of split further.
pub const ID_LIST_THRESHOLD: usize = 32;

pub type Fingerprint = [u8; 16];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    /// Settled (or nothing to say) for this range.
    Skip,
    Fingerprint(Fingerprint),
    Ids(Vec<ItemId>),
}

/// The ranges of one message tile the whole order: each starts where the previous one ended
/// (the first at [`MIN_BOUND`]), so only upper bounds travel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub upper: Item,
    pub mode: Mode,
}

/// What one side learned from a message, plus its reply (empty: reconciliation is over).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub reply: Vec<Range>,
    /// Ids the other side has and we lack: fetch them.
    pub we_lack: Vec<ItemId>,
    /// Ids we have and the other side lacks: it fetches them (or we push them).
    pub they_lack: Vec<ItemId>,
}

/// Sum of the ids as 256-bit little-endian integers (order-independent), hashed with the count.
fn fingerprint(items: &[Item]) -> Fingerprint {
    let mut sum = [0u8; 32];
    for item in items {
        let mut carry = 0u16;
        for (byte, add) in sum.iter_mut().zip(item.id) {
            let total = u16::from(*byte) + u16::from(add) + carry;
            *byte = total as u8;
            carry = total >> 8;
        }
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&sum);
    hasher.update(&(items.len() as u64).to_le_bytes());
    let mut fingerprint = Fingerprint::default();
    let width = fingerprint.len();
    fingerprint.copy_from_slice(&hasher.finalize().as_bytes()[..width]);
    fingerprint
}

/// Our items in `[lower, upper)`; `items` must be sorted.
fn in_range<'a>(items: &'a [Item], lower: &Item, upper: &Item) -> &'a [Item] {
    let start = items.partition_point(|item| item < lower);
    let end = items.partition_point(|item| item < upper);
    &items[start..end.max(start)]
}

fn describe(items: &[Item], upper: Item) -> Range {
    let mode = if items.len() <= ID_LIST_THRESHOLD {
        Mode::Ids(items.iter().map(|item| item.id).collect())
    } else {
        Mode::Fingerprint(fingerprint(items))
    };
    Range { upper, mode }
}

/// The opening message: everything we have, as one range. `items` must be sorted.
pub fn initiate(items: &[Item]) -> Vec<Range> {
    vec![describe(items, MAX_BOUND)]
}

/// Appends a range, merging neighbouring skips.
fn push(reply: &mut Vec<Range>, range: Range) {
    if range.mode == Mode::Skip
        && let Some(last) = reply.last_mut()
        && last.mode == Mode::Skip
    {
        last.upper = range.upper;
        return;
    }
    reply.push(range);
}

/// Answers the other side's ranges. `items` must be sorted.
pub fn respond(items: &[Item], incoming: &[Range]) -> Step {
    let mut step = Step::default();
    let mut lower = MIN_BOUND;
    for range in incoming {
        let ours = in_range(items, &lower, &range.upper);
        let skip = Range {
            upper: range.upper,
            mode: Mode::Skip,
        };
        match &range.mode {
            Mode::Skip => push(&mut step.reply, skip),
            Mode::Ids(theirs) => {
                let mut theirs_sorted = theirs.clone();
                theirs_sorted.sort_unstable();
                let ours_sorted: Vec<ItemId> = {
                    let mut ids: Vec<ItemId> = ours.iter().map(|item| item.id).collect();
                    ids.sort_unstable();
                    ids
                };
                step.we_lack.extend(
                    theirs_sorted
                        .iter()
                        .filter(|id| ours_sorted.binary_search(id).is_err()),
                );
                step.they_lack.extend(
                    ours_sorted
                        .iter()
                        .filter(|id| theirs_sorted.binary_search(id).is_err()),
                );
                push(&mut step.reply, skip);
            }
            Mode::Fingerprint(theirs) if fingerprint(ours) == *theirs => {
                push(&mut step.reply, skip)
            }
            Mode::Fingerprint(_) if ours.len() <= ID_LIST_THRESHOLD => {
                push(&mut step.reply, describe(ours, range.upper));
            }
            Mode::Fingerprint(_) => {
                let per_branch = ours.len().div_ceil(BRANCHES);
                for (index, chunk) in ours.chunks(per_branch).enumerate() {
                    let upper = ours
                        .get((index + 1) * per_branch)
                        .copied()
                        .unwrap_or(range.upper);
                    push(
                        &mut step.reply,
                        Range {
                            upper,
                            mode: Mode::Fingerprint(fingerprint(chunk)),
                        },
                    );
                }
            }
        }
        lower = range.upper;
    }
    while step.reply.last().is_some_and(|range| range.mode == Mode::Skip) {
        step.reply.pop();
    }
    step.we_lack.sort_unstable();
    step.we_lack.dedup();
    step.they_lack.sort_unstable();
    step.they_lack.dedup();
    step
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn item(n: u32) -> Item {
        Item {
            sent_at_ms: u64::from(n / 3),
            id: *blake3::hash(&n.to_le_bytes()).as_bytes(),
        }
    }

    /// Runs both sides to completion; returns (what A lacks, what B lacks, frames, bytes).
    fn reconcile(a: &[Item], b: &[Item]) -> (BTreeSet<ItemId>, BTreeSet<ItemId>, usize, usize) {
        let (mut a_lacks, mut b_lacks) = (BTreeSet::new(), BTreeSet::new());
        let mut message = initiate(a);
        let mut bytes = crate::wire::encode(&message).len();
        let mut frames = 1;
        let mut b_turn = true;
        while !message.is_empty() {
            let step = if b_turn { respond(b, &message) } else { respond(a, &message) };
            let (lacks_here, lacks_there) = if b_turn {
                (&mut b_lacks, &mut a_lacks)
            } else {
                (&mut a_lacks, &mut b_lacks)
            };
            lacks_here.extend(step.we_lack);
            lacks_there.extend(step.they_lack);
            message = step.reply;
            bytes += crate::wire::encode(&message).len();
            frames += 1;
            b_turn = !b_turn;
            assert!(frames < 50, "reconciliation must terminate");
        }
        (a_lacks, b_lacks, frames, bytes)
    }

    fn sorted(range: impl Iterator<Item = u32>) -> Vec<Item> {
        let mut items: Vec<Item> = range.map(item).collect();
        items.sort();
        items
    }

    #[test]
    fn finds_exactly_the_differences_both_ways() {
        let a = sorted((0..5000).filter(|n| n % 997 != 0));
        let b = sorted((0..5000).filter(|n| n % 1009 != 0));
        let (a_lacks, b_lacks, _, bytes) = reconcile(&a, &b);
        let expected_a: BTreeSet<ItemId> =
            (0..5000).filter(|n| n % 997 == 0 && n % 1009 != 0).map(|n| item(n).id).collect();
        let expected_b: BTreeSet<ItemId> =
            (0..5000).filter(|n| n % 1009 == 0 && n % 997 != 0).map(|n| item(n).id).collect();
        assert_eq!(a_lacks, expected_a);
        assert_eq!(b_lacks, expected_b);
        // Far less than shipping 5000 ids (160 KB).
        assert!(bytes < 16_000, "{bytes} bytes");
    }

    #[test]
    fn equal_sets_settle_in_one_round_trip_and_empty_sides_list_everything() {
        let a = sorted(0..3000);
        let (a_lacks, b_lacks, frames, _) = reconcile(&a, &a);
        assert!(a_lacks.is_empty() && b_lacks.is_empty());
        assert_eq!(frames, 2);

        let (a_lacks, b_lacks, _, _) = reconcile(&[], &a);
        assert_eq!(a_lacks.len(), 3000);
        assert!(b_lacks.is_empty());
        let (a_lacks, b_lacks, _, _) = reconcile(&a, &[]);
        assert!(a_lacks.is_empty());
        assert_eq!(b_lacks.len(), 3000);
    }
}
