//! Which epoch secret to send with, and when the group needs a rekey (SPEC.md §14.3). Pure.
//!
//! Epochs form a tree. A [`super::keys::Rekey`] starts a root, bound to the op-log heads it
//! was made for. An `Add` op travels inside a message of some epoch `E`, and everyone who reads
//! it derives the child `E' = ratchet_add(E, op)` with the heads advanced by that op; the
//! newcomer gets only `E'`. An epoch is **complete** when its heads' ancestry contains every
//! membership op the device has folded (adds, kicks, leaves): only complete epochs are used for
//! sending, so a kicked member never gets a key used after the kick, and a newcomer is never
//! left out. Several complete epochs (concurrent rekeys) → the lowest id. None complete (a
//! kick, or two concurrent adds that forked the tree) → a rekey is due.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{
    GroupOp, OpHash,
    checkpoint::Checkpoint,
    keys::{EpochId, Secret, epoch_id, ratchet_add},
    oplog::{self, Change, FoldedOp},
};

/// Everything needed to use (and hand on) one epoch secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Epoch {
    pub id: EpochId,
    pub secret: Secret,
    /// The membership it was made for (sorted).
    pub heads: Vec<OpHash>,
    /// The epoch an `Add` ratcheted it from; `None` for a rekey's root.
    pub parent: Option<EpochId>,
    pub created_at_ms: u64,
}

impl Epoch {
    pub fn root(secret: Secret, heads: Vec<OpHash>, now_ms: u64) -> Self {
        Self {
            id: epoch_id(&secret),
            secret,
            heads: sorted(heads),
            parent: None,
            created_at_ms: now_ms,
        }
    }

    /// The epoch after `add` was posted in `self`.
    pub fn after_add(&self, add: &GroupOp, now_ms: u64) -> Self {
        let secret = ratchet_add(&self.secret, &add.hash());
        Self {
            id: epoch_id(&secret),
            secret,
            heads: advance_heads(&self.heads, add),
            parent: Some(self.id),
            created_at_ms: now_ms,
        }
    }
}

fn sorted(mut heads: Vec<OpHash>) -> Vec<OpHash> {
    heads.sort_unstable();
    heads.dedup();
    heads
}

/// The heads after `op`: its parents are no longer heads, the op is.
pub fn advance_heads(heads: &[OpHash], op: &GroupOp) -> Vec<OpHash> {
    let mut next: Vec<OpHash> = heads
        .iter()
        .filter(|head| !op.parents.contains(head))
        .copied()
        .collect();
    next.push(op.hash());
    sorted(next)
}

/// Ops that changed who is in the group (in fold order after the checkpoint).
pub fn membership_ops(order: &[FoldedOp]) -> Vec<OpHash> {
    order
        .iter()
        .filter(|folded| {
            matches!(
                folded.change,
                Some(Change::Added(_) | Change::Removed(_) | Change::Left { .. } | Change::Created)
            )
        })
        .map(|folded| folded.hash)
        .collect()
}

/// Whether `heads` cover every membership op: they reach past the checkpoint (if any) and their
/// ancestry contains each of `membership` (the membership ops after it).
pub fn is_complete(
    heads: &[OpHash],
    membership: &[OpHash],
    ops: &[GroupOp],
    checkpoint: Option<&Checkpoint>,
) -> bool {
    let ancestry: HashSet<OpHash> = oplog::ancestry(heads, ops);
    let past_checkpoint = checkpoint.is_none_or(|checkpoint| {
        checkpoint.heads.iter().all(|head| ancestry.contains(head))
    });
    past_checkpoint && membership.iter().all(|op| ancestry.contains(op))
}

/// The epoch to send with: the lowest id among the complete ones.
pub fn choose<'a>(complete: impl IntoIterator<Item = &'a Epoch>) -> Option<&'a Epoch> {
    complete.into_iter().min_by_key(|epoch| epoch.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group::{
        OpKind,
        keys::random_secret,
        oplog::fold,
        tests::TestUser,
    };

    struct Setup {
        owner: TestUser,
        ops: Vec<GroupOp>,
    }

    impl Setup {
        fn new() -> Self {
            let owner = TestUser::new("Owner");
            let ops = vec![owner.create("Keys")];
            Self { owner, ops }
        }

        fn group(&self) -> [u8; 32] {
            self.ops[0].group
        }

        fn heads(&self) -> Vec<OpHash> {
            fold(self.group(), None, &self.ops).state.heads
        }

        fn push(&mut self, op: GroupOp) -> GroupOp {
            self.ops.push(op.clone());
            op
        }

        fn complete(&self, epoch: &Epoch) -> bool {
            let folded = fold(self.group(), None, &self.ops);
            is_complete(&epoch.heads, &membership_ops(&folded.order), &self.ops, None)
        }
    }

    #[test]
    fn an_add_ratchets_and_a_kick_needs_a_rekey() {
        let mut setup = Setup::new();
        let root = Epoch::root(random_secret(), setup.heads(), 1);
        assert!(setup.complete(&root));

        let bob = TestUser::new("Bob");
        let add = setup.owner.op(setup.group(), setup.heads(), bob.add_kind(&setup.group(), 0));
        let add = setup.push(add);
        assert!(!setup.complete(&root), "the root does not include Bob");
        let child = root.after_add(&add, 2);
        assert!(setup.complete(&child));
        assert_eq!(child.heads, setup.heads());
        assert_eq!(child.parent, Some(root.id));
        // Everyone derives the same child.
        assert_eq!(root.after_add(&add, 9).secret, child.secret);

        // A rename does not touch membership.
        let rename = setup.owner.op(setup.group(), setup.heads(), OpKind::Rename { name: "K2".into() });
        setup.push(rename);
        assert!(setup.complete(&child));

        // A kick: no epoch is complete until a rekey for the new heads.
        let kick = setup.owner.op(setup.group(), setup.heads(), OpKind::Remove { user: bob.key() });
        setup.push(kick);
        assert!(!setup.complete(&child));
        let rekeyed = Epoch::root(random_secret(), setup.heads(), 3);
        assert!(setup.complete(&rekeyed));
    }

    #[test]
    fn concurrent_adds_fork_and_concurrent_rekeys_pick_the_lower_id() {
        let mut setup = Setup::new();
        let root = Epoch::root(random_secret(), setup.heads(), 1);
        let (bob, carol) = (TestUser::new("Bob"), TestUser::new("Carol"));
        let base = setup.heads();
        let add_bob = setup.push(setup.owner.op(setup.group(), base.clone(), bob.add_kind(&setup.group(), 0)));
        let add_carol = setup.push(setup.owner.op(setup.group(), base, carol.add_kind(&setup.group(), 0)));
        let (with_bob, with_carol) = (root.after_add(&add_bob, 2), root.after_add(&add_carol, 2));
        assert!(!setup.complete(&with_bob) && !setup.complete(&with_carol), "forked");

        let (first, second) = (
            Epoch::root(random_secret(), setup.heads(), 3),
            Epoch::root(random_secret(), setup.heads(), 3),
        );
        let candidates = [&first, &second, &with_bob];
        let complete: Vec<&Epoch> = candidates.into_iter().filter(|epoch| setup.complete(epoch)).collect();
        assert_eq!(complete.len(), 2);
        assert_eq!(choose(complete).unwrap().id, first.id.min(second.id));
    }
}
