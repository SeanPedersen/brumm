//! Checkpoints (SPEC.md §14.2): every [`CHECKPOINT_OPS`] ops the owner or an admin signs the
//! folded state at some heads. A newcomer starts from the latest checkpoint plus the ops after
//! it instead of replaying 10k `Add`s, and members may prune the ops it covers. Pure.
//!
//! Every device folds "checkpoint snapshot + ops it does not cover", so a late op that was
//! concurrent with covered ones lands after the snapshot on every device alike. A device that
//! still has the covered ops recomputes the snapshot ([`Checkpoint::matches_log`]) and rejects
//! one that differs; a newcomer trusts it as much as the admin who invited it.

use std::collections::HashSet;

use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};

use super::{
    GroupId, GroupOp, OpHash,
    oplog::{self, GroupState},
};
use crate::{
    identity::{UserIdentity, UserKey, verify_signature},
    wire,
};

/// Ops between two checkpoints.
pub const CHECKPOINT_OPS: u64 = 1000;
const CHECKPOINT_DOMAIN: &[u8] = b"brumm/group-checkpoint/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub group: GroupId,
    pub heads: Vec<OpHash>,
    /// Every op in the ancestry of `heads`, sorted. Lets a device that never saw them tell a
    /// late op's pruned parents from missing ones.
    pub covered: Vec<OpHash>,
    pub snapshot: GroupState,
    pub author: UserKey,
    pub signature: Signature,
}

impl Checkpoint {
    /// Signs the current state. `ops` must be every op folded since `previous` (so all of
    /// them are ancestors of `state.heads`).
    pub fn new(
        author: &UserIdentity,
        state: &GroupState,
        previous: Option<&Checkpoint>,
        ops: &[GroupOp],
    ) -> Self {
        let mut covered: Vec<OpHash> = previous
            .map(|checkpoint| checkpoint.covered.clone())
            .unwrap_or_default();
        let known: HashSet<OpHash> = ops.iter().map(GroupOp::hash).collect();
        covered.extend(
            oplog::ancestry(&state.heads, ops)
                .into_iter()
                .filter(|hash| known.contains(hash)),
        );
        covered.sort_unstable();
        covered.dedup();
        let mut checkpoint = Self {
            group: state.group,
            heads: state.heads.clone(),
            covered,
            snapshot: state.clone(),
            author: author.public(),
            signature: Signature::from_bytes(&[0; Signature::BYTE_SIZE]),
        };
        checkpoint.signature = author.sign(&checkpoint.signed_bytes());
        checkpoint
    }

    fn signed_bytes(&self) -> Vec<u8> {
        [
            CHECKPOINT_DOMAIN,
            &self.group,
            &wire::encode(&self.heads),
            &wire::encode(&self.covered),
            &wire::encode(&self.snapshot),
            &self.author,
        ]
        .concat()
    }

    pub fn hash(&self) -> [u8; 32] {
        *blake3::hash(&wire::encode(self)).as_bytes()
    }

    /// Signature and internal consistency: signed by an owner or admin of the snapshot,
    /// heads among the covered ops.
    pub fn verify(&self) -> bool {
        let covered_sorted = self.covered.windows(2).all(|pair| pair[0] < pair[1]);
        covered_sorted
            && self.snapshot.group == self.group
            && self.snapshot.heads == self.heads
            && self.snapshot.is_admin(&self.author)
            && self
                .heads
                .iter()
                .all(|head| self.covered.binary_search(head).is_ok())
            && verify_signature(&self.author, &self.signed_bytes(), &self.signature)
    }

    /// For a device that still has the covered ops: refolds them from `previous` and checks
    /// that the snapshot and the covered set are exactly what they give.
    pub fn matches_log(&self, previous: Option<&Checkpoint>, ops: &[GroupOp]) -> bool {
        let previous_covered: HashSet<OpHash> = previous
            .map(|checkpoint| checkpoint.covered.iter().copied().collect())
            .unwrap_or_default();
        let ancestry = oplog::ancestry(&self.heads, ops);
        let in_range: Vec<GroupOp> = ops
            .iter()
            .filter(|op| {
                let hash = op.hash();
                ancestry.contains(&hash) && !previous_covered.contains(&hash)
            })
            .cloned()
            .collect();
        let mut covered: Vec<OpHash> = previous_covered
            .iter()
            .copied()
            .chain(in_range.iter().map(GroupOp::hash))
            .collect();
        covered.sort_unstable();
        covered.dedup();
        let folded = oplog::fold(self.group, previous, &in_range);
        folded.pending.is_empty() && covered == self.covered && folded.state == self.snapshot
    }

    /// Whether `self` should replace `current`: it must extend it (cover everything it
    /// covers) and cover more; equal coverage picks the lower hash.
    pub fn supersedes(&self, current: &Checkpoint) -> bool {
        if self.group != current.group || self.covered.len() < current.covered.len() {
            return false;
        }
        let extends = current
            .covered
            .iter()
            .all(|hash| self.covered.binary_search(hash).is_ok());
        extends
            && (self.covered.len() > current.covered.len() || self.hash() < current.hash())
    }

    pub fn covers(&self, hash: &OpHash) -> bool {
        self.covered.binary_search(hash).is_ok()
    }
}

/// Whether a device holding `state` (folded on `checkpoint`, if any) is due to write the next
/// checkpoint, one every `interval` ops ([`CHECKPOINT_OPS`] outside tests).
pub fn is_due(state: &GroupState, checkpoint: Option<&Checkpoint>, interval: u64) -> bool {
    let since = state.seq - checkpoint.map_or(0, |checkpoint| checkpoint.snapshot.seq);
    since >= interval
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group::{OpKind, oplog::fold, tests::TestUser};

    fn build_log(owner: &TestUser, members: usize) -> (GroupId, Vec<GroupOp>, Vec<TestUser>) {
        let create = owner.create("Big");
        let group = create.group;
        let mut ops = vec![create];
        let mut users = Vec::new();
        for index in 0..members {
            let user = TestUser::new(&format!("U{index}"));
            let heads = fold(group, None, &ops).state.heads;
            ops.push(owner.op(group, heads, user.add_kind(&group, 0)));
            users.push(user);
        }
        (group, ops, users)
    }

    #[test]
    fn checkpoint_plus_tail_folds_like_the_full_log() {
        let owner = TestUser::new("Owner");
        let (group, mut ops, users) = build_log(&owner, 8);
        let state = fold(group, None, &ops).state;
        let checkpoint = Checkpoint::new(&owner.identity, &state, None, &ops);
        assert!(checkpoint.verify());
        assert!(checkpoint.matches_log(None, &ops));
        assert_eq!(checkpoint.covered.len(), 9);

        // Ops after the checkpoint: a member leaves, another renames (not allowed).
        let tail_leave = users[0].op(group, state.heads.clone(), OpKind::Leave);
        ops.push(tail_leave.clone());
        let heads = fold(group, None, &ops).state.heads;
        let tail_rename = owner.op(group, heads, OpKind::Rename { name: "Bigger".into() });
        ops.push(tail_rename.clone());

        let full = fold(group, None, &ops).state;
        let tail = vec![tail_leave, tail_rename];
        let from_checkpoint = fold(group, Some(&checkpoint), &tail);
        assert!(from_checkpoint.pending.is_empty());
        assert_eq!(from_checkpoint.state, full);
        // Covered ops given again are ignored.
        assert_eq!(fold(group, Some(&checkpoint), &ops).state, full);
    }

    #[test]
    fn a_forged_snapshot_is_rejected() {
        let owner = TestUser::new("Owner");
        let (group, ops, users) = build_log(&owner, 3);
        let state = fold(group, None, &ops).state;

        // An admin who lies about the membership: signature fine, log disagrees.
        let mut forged_state = state.clone();
        forged_state.members.remove(&users[0].key());
        let forged = Checkpoint::new(&owner.identity, &forged_state, None, &ops);
        assert!(forged.verify());
        assert!(!forged.matches_log(None, &ops));

        // Not signed by an admin of the snapshot.
        let by_member = Checkpoint::new(&users[1].identity, &state, None, &ops);
        assert!(!by_member.verify());

        // Tampering after signing.
        let mut tampered = Checkpoint::new(&owner.identity, &state, None, &ops);
        tampered.snapshot.name = "Evil".into();
        assert!(!tampered.verify());
    }

    #[test]
    fn a_later_checkpoint_supersedes_and_chains() {
        let owner = TestUser::new("Owner");
        let (group, mut ops, _) = build_log(&owner, 2);
        let first_state = fold(group, None, &ops).state;
        let first = Checkpoint::new(&owner.identity, &first_state, None, &ops);
        let extra = TestUser::new("Extra");
        ops.push(owner.op(group, first_state.heads.clone(), extra.add_kind(&group, 0)));
        let tail: Vec<GroupOp> = ops.iter().filter(|op| !first.covers(&op.hash())).cloned().collect();
        let second_state = fold(group, Some(&first), &tail).state;
        let second = Checkpoint::new(&owner.identity, &second_state, Some(&first), &tail);
        assert!(second.verify());
        assert!(second.matches_log(Some(&first), &tail));
        assert!(second.supersedes(&first));
        assert!(!first.supersedes(&second));
        assert_eq!(fold(group, Some(&second), &[]).state, fold(group, None, &ops).state);
        assert!(!is_due(&second_state, Some(&second), CHECKPOINT_OPS));
    }
}
