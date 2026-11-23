//! The group's state as a deterministic fold over its op DAG (SPEC.md §14.2). Pure.
//!
//! Ops are applied in a topological order. Among ops that are ready at the same time, the one
//! whose author ranks highest goes first (owner, then admins by promotion, then members by
//! joining, then non-members), then the lower hash. Each op is checked against the state at
//! its position, so e.g. an admin's kick that races the owner's demotion of that admin is
//! dropped on every device alike. The input order never matters.
//!
//! Ops must have passed [`GroupOp::verify`]; the fold itself does no cryptography.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{
    BlobHash, DeviceKeys, GroupId, GroupOp, MAX_GROUP_MEMBERS, OpHash, OpKind, Role,
    checkpoint::Checkpoint,
};
use crate::identity::{Profile, UserKey};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub role: Role,
    /// Fold position of the op that added the member (succession and ranking).
    pub joined_seq: u64,
    /// Fold position of the promotion, for admins.
    pub promoted_seq: Option<u64>,
    /// As of the `Add`. Newer device lists travel outside the op log.
    pub profile: Profile,
    pub device_keys: Vec<DeviceKeys>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupState {
    pub group: GroupId,
    pub created: bool,
    pub name: String,
    pub announcement: bool,
    pub history_sharing: bool,
    pub avatar: Option<BlobHash>,
    /// `None` once the last member left.
    pub owner: Option<UserKey>,
    pub members: BTreeMap<UserKey, Member>,
    /// How often each user left or was removed; a new `Add` needs consent for this count.
    pub departures: BTreeMap<UserKey, u32>,
    /// The DAG frontier: what a new op names as its parents. Sorted.
    pub heads: Vec<OpHash>,
    /// Ops folded so far (valid or not): the next op's position.
    pub seq: u64,
}

/// What a valid op did, for system lines and key handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Created,
    Added(UserKey),
    Removed(UserKey),
    /// `new_owner` is set when the owner left and someone else took over.
    Left {
        user: UserKey,
        new_owner: Option<UserKey>,
    },
    RoleSet {
        user: UserKey,
        admin: bool,
    },
    Renamed(String),
    Announcement(bool),
    Avatar(Option<BlobHash>),
    HistorySharing(bool),
}

impl Change {
    /// Membership shrank: the next epoch must exclude someone (SPEC.md §14.3).
    pub fn needs_rekey(&self) -> bool {
        matches!(self, Self::Removed(_) | Self::Left { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldedOp {
    pub hash: OpHash,
    pub author: UserKey,
    pub at_ms: u64,
    /// `None` if the op was not allowed at its position.
    pub change: Option<Change>,
}

#[derive(Debug, Clone, Default)]
pub struct Folded {
    pub state: GroupState,
    /// Applied ops in fold order (after the checkpoint, if one was used).
    pub order: Vec<FoldedOp>,
    /// Ops waiting for parents we do not have.
    pub pending: Vec<OpHash>,
}

impl GroupState {
    pub fn empty(group: GroupId) -> Self {
        Self {
            group,
            ..Self::default()
        }
    }

    pub fn role(&self, user: &UserKey) -> Option<Role> {
        self.members.get(user).map(|member| member.role)
    }

    pub fn is_member(&self, user: &UserKey) -> bool {
        self.members.contains_key(user)
    }

    pub fn is_admin(&self, user: &UserKey) -> bool {
        matches!(self.role(user), Some(Role::Owner | Role::Admin))
    }

    /// Lower ranks fold first among concurrent ops.
    fn rank(&self, user: &UserKey) -> (u8, u64) {
        match self.members.get(user) {
            Some(Member {
                role: Role::Owner, ..
            }) => (0, 0),
            Some(Member {
                role: Role::Admin,
                promoted_seq,
                ..
            }) => (1, promoted_seq.unwrap_or(0)),
            Some(Member { joined_seq, .. }) => (2, *joined_seq),
            None => (3, 0),
        }
    }

    /// Whether `user` may post in this group (announcement mode: owner and admins only).
    pub fn may_post(&self, user: &UserKey) -> bool {
        self.is_member(user) && (!self.announcement || self.is_admin(user))
    }
}

/// Folds `ops` on top of `base` (or from scratch). Ops covered by `base`, of another group,
/// or with missing parents are not applied.
pub fn fold(group: GroupId, base: Option<&Checkpoint>, ops: &[GroupOp]) -> Folded {
    let covered: HashSet<OpHash> = base
        .map(|checkpoint| checkpoint.covered.iter().copied().collect())
        .unwrap_or_default();
    let mut state = base.map_or_else(
        || GroupState::empty(group),
        |checkpoint| checkpoint.snapshot.clone(),
    );
    let mut by_hash: HashMap<OpHash, &GroupOp> = HashMap::new();
    for op in ops.iter().filter(|op| op.group == group) {
        let hash = op.hash();
        if !covered.contains(&hash) {
            by_hash.insert(hash, op);
        }
    }

    let mut children: HashMap<OpHash, Vec<OpHash>> = HashMap::new();
    let mut waiting: HashMap<OpHash, usize> = HashMap::new();
    let mut ready: Vec<OpHash> = Vec::new();
    for (hash, op) in &by_hash {
        let mut parents: Vec<&OpHash> = op.parents.iter().collect();
        parents.sort_unstable();
        parents.dedup();
        let mut unmet = 0;
        for parent in parents {
            if by_hash.contains_key(parent) {
                children.entry(*parent).or_default().push(*hash);
                unmet += 1;
            } else if !covered.contains(parent) {
                // A parent nobody gave us: this op (and what builds on it) waits.
                unmet += 1;
            }
        }
        if unmet == 0 {
            ready.push(*hash);
        } else {
            waiting.insert(*hash, unmet);
        }
    }

    let mut order = Vec::with_capacity(by_hash.len());
    let mut folded_parents: HashSet<OpHash> = HashSet::new();
    while !ready.is_empty() {
        let next = (0..ready.len())
            .min_by_key(|&index| (state.rank(&by_hash[&ready[index]].author), ready[index]))
            .expect("ready is not empty");
        let hash = ready.swap_remove(next);
        let op = by_hash[&hash];
        folded_parents.extend(op.parents.iter().copied());
        order.push(apply(&mut state, op, hash));
        for child in children.get(&hash).into_iter().flatten() {
            let unmet = waiting.get_mut(child).expect("child is waiting");
            *unmet -= 1;
            if *unmet == 0 {
                waiting.remove(child);
                ready.push(*child);
            }
        }
    }

    let mut heads: Vec<OpHash> = state
        .heads
        .iter()
        .copied()
        .chain(order.iter().map(|folded| folded.hash))
        .filter(|hash| !folded_parents.contains(hash))
        .collect();
    heads.sort_unstable();
    heads.dedup();
    state.heads = heads;
    let mut pending: Vec<OpHash> = waiting.into_keys().collect();
    pending.sort_unstable();
    Folded {
        state,
        order,
        pending,
    }
}

/// Applies an op that names exactly the current heads as its parents. Every known op is its
/// ancestor, so it is last in any fold and the result equals a full [`fold`]. `None` if the
/// op does not extend the heads (the caller refolds instead).
pub fn extend(state: &mut GroupState, op: &GroupOp) -> Option<FoldedOp> {
    let mut parents = op.parents.clone();
    parents.sort_unstable();
    parents.dedup();
    if op.group != state.group || parents != state.heads || parents.is_empty() {
        return None;
    }
    let hash = op.hash();
    let folded = apply(state, op, hash);
    state.heads = vec![hash];
    Some(folded)
}

/// Checks `op` against `state` and applies it. Advances `state.seq` either way.
fn apply(state: &mut GroupState, op: &GroupOp, hash: OpHash) -> FoldedOp {
    let seq = state.seq;
    state.seq += 1;
    let change = change_of(state, op, seq);
    FoldedOp {
        hash,
        author: op.author,
        at_ms: op.at_ms,
        change,
    }
}

fn change_of(state: &mut GroupState, op: &GroupOp, seq: u64) -> Option<Change> {
    let author = op.author;
    if let OpKind::Create {
        name,
        announcement,
        creator,
        device_keys,
        ..
    } = &op.kind
    {
        if state.created {
            return None;
        }
        state.created = true;
        state.name = name.clone();
        state.announcement = *announcement;
        state.owner = Some(author);
        state.members.insert(
            author,
            Member {
                role: Role::Owner,
                joined_seq: seq,
                promoted_seq: None,
                profile: creator.clone(),
                device_keys: device_keys.clone(),
            },
        );
        return Some(Change::Created);
    }
    if !state.created {
        return None;
    }
    let author_role = state.role(&author)?;
    let is_admin = matches!(author_role, Role::Owner | Role::Admin);
    match &op.kind {
        OpKind::Create { .. } => unreachable!("handled above"),
        OpKind::Add {
            member,
            device_keys,
            rejoin,
            ..
        } => {
            let user = member.user_key();
            let departures = state.departures.get(&user).copied().unwrap_or(0);
            if !is_admin
                || state.is_member(&user)
                || state.members.len() >= MAX_GROUP_MEMBERS
                || *rejoin != departures
            {
                return None;
            }
            state.members.insert(
                user,
                Member {
                    role: Role::Member,
                    joined_seq: seq,
                    promoted_seq: None,
                    profile: member.clone(),
                    device_keys: device_keys.clone(),
                },
            );
            Some(Change::Added(user))
        }
        OpKind::Remove { user } => {
            let allowed = match state.role(user)? {
                Role::Owner => false,
                Role::Admin => author_role == Role::Owner,
                Role::Member => is_admin,
            };
            if !allowed {
                return None;
            }
            depart(state, user);
            Some(Change::Removed(*user))
        }
        OpKind::Leave => {
            depart(state, &author);
            let new_owner = (author_role == Role::Owner)
                .then(|| succeed(state))
                .flatten();
            Some(Change::Left {
                user: author,
                new_owner,
            })
        }
        OpKind::SetRole { user, admin } => {
            if author_role != Role::Owner {
                return None;
            }
            let target = state.members.get_mut(user)?;
            match (target.role, admin) {
                (Role::Member, true) => {
                    target.role = Role::Admin;
                    target.promoted_seq = Some(seq);
                }
                (Role::Admin, false) => {
                    target.role = Role::Member;
                    target.promoted_seq = None;
                }
                _ => return None,
            }
            Some(Change::RoleSet {
                user: *user,
                admin: *admin,
            })
        }
        OpKind::Rename { name } if is_admin => {
            state.name = name.clone();
            Some(Change::Renamed(name.clone()))
        }
        OpKind::SetAnnouncement(on) if author_role == Role::Owner => {
            state.announcement = *on;
            Some(Change::Announcement(*on))
        }
        OpKind::SetAvatar { blob } if is_admin => {
            state.avatar = *blob;
            Some(Change::Avatar(*blob))
        }
        OpKind::SetHistorySharing(on) if is_admin => {
            state.history_sharing = *on;
            Some(Change::HistorySharing(*on))
        }
        _ => None,
    }
}

fn depart(state: &mut GroupState, user: &UserKey) {
    state.members.remove(user);
    *state.departures.entry(*user).or_default() += 1;
}

/// The owner left: the admin promoted earliest takes over, else the member who joined earliest.
fn succeed(state: &mut GroupState) -> Option<UserKey> {
    let admin = state
        .members
        .iter()
        .filter(|(_, member)| member.role == Role::Admin)
        .min_by_key(|(_, member)| member.promoted_seq)
        .map(|(user, _)| *user);
    let heir = admin.or_else(|| {
        state
            .members
            .iter()
            .min_by_key(|(_, member)| member.joined_seq)
            .map(|(user, _)| *user)
    });
    state.owner = heir;
    if let Some(heir) = heir {
        let member = state.members.get_mut(&heir).expect("heir is a member");
        member.role = Role::Owner;
        member.promoted_seq = None;
    }
    heir
}

/// Every op reachable from `heads` through parents, among `ops` (ancestors pruned before a
/// checkpoint are simply not found).
pub fn ancestry(heads: &[OpHash], ops: &[GroupOp]) -> HashSet<OpHash> {
    let by_hash: HashMap<OpHash, &GroupOp> = ops.iter().map(|op| (op.hash(), op)).collect();
    let mut seen = HashSet::new();
    let mut stack: Vec<OpHash> = heads.to_vec();
    while let Some(hash) = stack.pop() {
        if !seen.insert(hash) {
            continue;
        }
        if let Some(op) = by_hash.get(&hash) {
            stack.extend(op.parents.iter().copied());
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use rand::seq::SliceRandom;

    use super::*;
    use crate::group::tests::TestUser;

    /// A group's op list, grown by appending ops on the current heads.
    struct Log {
        group: GroupId,
        ops: Vec<GroupOp>,
    }

    impl Log {
        fn new(owner: &TestUser) -> Self {
            let create = owner.create("Chess club");
            Self {
                group: create.group,
                ops: vec![create],
            }
        }

        fn state(&self) -> GroupState {
            fold(self.group, None, &self.ops).state
        }

        fn push(&mut self, author: &TestUser, kind: OpKind) -> GroupOp {
            let op = author.op(self.group, self.state().heads, kind);
            assert!(op.verify());
            self.ops.push(op.clone());
            op
        }

        /// `by` adds `user` with a consent for the user's current departure count.
        fn add(&mut self, by: &TestUser, user: &TestUser) {
            let rejoin = self
                .state()
                .departures
                .get(&user.key())
                .copied()
                .unwrap_or(0);
            self.push(by, user.add_kind(&self.group, rejoin));
        }

        fn set_admin(&mut self, by: &TestUser, user: &TestUser, admin: bool) {
            self.push(
                by,
                OpKind::SetRole {
                    user: user.key(),
                    admin,
                },
            );
        }
    }

    #[test]
    fn create_add_promote_kick() {
        let (owner, bob, cara) = (TestUser::new("Owner"), TestUser::new("Bob"), TestUser::new("Cara"));
        let mut log = Log::new(&owner);
        log.add(&owner, &bob);
        log.add(&owner, &cara);
        log.set_admin(&owner, &bob, true);
        log.push(&bob, OpKind::Remove { user: cara.key() });
        let state = log.state();
        assert_eq!(state.name, "Chess club");
        assert_eq!(state.role(&bob.key()), Some(Role::Admin));
        assert!(!state.is_member(&cara.key()));
        assert_eq!(state.departures[&cara.key()], 1);
        assert_eq!(state.heads.len(), 1);
        assert_eq!(state.seq, 5);

        // An admin cannot remove the owner; a member cannot rename.
        log.push(&bob, OpKind::Remove { user: owner.key() });
        assert!(log.state().is_member(&owner.key()));
        log.add(&bob, &cara);
        log.push(&cara, OpKind::Rename { name: "Mine".into() });
        assert_eq!(log.state().name, "Chess club");
        // Only the owner switches announcement mode.
        log.push(&bob, OpKind::SetAnnouncement(true));
        assert!(!log.state().announcement);
        log.push(&owner, OpKind::SetAnnouncement(true));
        let state = log.state();
        assert!(state.may_post(&bob.key()) && !state.may_post(&cara.key()));
    }

    #[test]
    fn delivery_order_never_changes_the_state() {
        let owner = TestUser::new("Owner");
        let mut log = Log::new(&owner);
        let users: Vec<TestUser> = (0..6).map(|i| TestUser::new(&format!("U{i}"))).collect();
        for user in &users {
            log.add(&owner, user);
        }
        // Concurrent ops on the same heads by several members.
        let heads = log.state().heads;
        for user in &users[..3] {
            log.ops.push(user.op(log.group, heads.clone(), OpKind::Leave));
        }
        log.ops.push(users[3].op(log.group, heads, OpKind::Rename { name: "x".into() }));
        let expected = log.state();
        let mut rng = rand::rng();
        for _ in 0..20 {
            let mut shuffled = log.ops.clone();
            shuffled.shuffle(&mut rng);
            assert_eq!(fold(log.group, None, &shuffled).state, expected);
        }
        assert_eq!(expected.members.len(), 4);
        assert_eq!(expected.heads.len(), 4);
        assert_eq!(expected.name, "Chess club", "a member cannot rename");
    }

    #[test]
    fn a_demotion_beats_the_racing_kick_by_that_admin() {
        let (owner, admin, member) = (TestUser::new("Owner"), TestUser::new("Admin"), TestUser::new("Member"));
        let mut log = Log::new(&owner);
        log.add(&owner, &admin);
        log.add(&owner, &member);
        log.set_admin(&owner, &admin, true);
        let heads = log.state().heads;
        let demote = owner.op(log.group, heads.clone(), OpKind::SetRole { user: admin.key(), admin: false });
        let kick = admin.op(log.group, heads, OpKind::Remove { user: member.key() });
        log.ops.push(kick.clone());
        log.ops.push(demote);
        let folded = fold(log.group, None, &log.ops);
        assert!(folded.state.is_member(&member.key()));
        assert_eq!(folded.state.role(&admin.key()), Some(Role::Member));
        let kick_result = folded.order.iter().find(|op| op.hash == kick.hash()).unwrap();
        assert_eq!(kick_result.change, None);
    }

    #[test]
    fn the_owner_leaving_hands_over_to_the_first_admin_else_the_oldest_member() {
        // With admins: the earliest promoted wins, not the earliest joined.
        let (owner, early, late) = (TestUser::new("Owner"), TestUser::new("Early"), TestUser::new("Late"));
        let mut log = Log::new(&owner);
        log.add(&owner, &early);
        log.add(&owner, &late);
        log.set_admin(&owner, &late, true);
        log.set_admin(&owner, &early, true);
        log.push(&owner, OpKind::Leave);
        let state = log.state();
        assert_eq!(state.owner, Some(late.key()));
        assert_eq!(state.role(&late.key()), Some(Role::Owner));
        assert_eq!(state.role(&early.key()), Some(Role::Admin));

        // Without admins: the member who joined first.
        let mut log = Log::new(&owner);
        log.add(&owner, &early);
        log.add(&owner, &late);
        log.push(&owner, OpKind::Leave);
        assert_eq!(log.state().owner, Some(early.key()));

        // The last one out leaves an ownerless, empty group.
        let mut alone = Log::new(&owner);
        alone.push(&owner, OpKind::Leave);
        assert_eq!(alone.state().owner, None);
        assert!(alone.state().members.is_empty());
    }

    #[test]
    fn rejoining_needs_a_fresh_consent() {
        let (owner, bob) = (TestUser::new("Owner"), TestUser::new("Bob"));
        let mut log = Log::new(&owner);
        log.add(&owner, &bob);
        log.push(&bob, OpKind::Leave);
        // Replaying the first consent (rejoin 0) does not add Bob again.
        log.push(&owner, bob.add_kind(&log.group, 0));
        assert!(!log.state().is_member(&bob.key()));
        log.add(&owner, &bob);
        assert!(log.state().is_member(&bob.key()));
    }

    #[test]
    fn ops_wait_for_missing_parents_and_extend_matches_fold() {
        let (owner, bob) = (TestUser::new("Owner"), TestUser::new("Bob"));
        let mut log = Log::new(&owner);
        log.add(&owner, &bob);
        let orphan = bob.op(log.group, vec![[42; 32]], OpKind::Leave);
        let mut with_orphan = log.ops.clone();
        with_orphan.push(orphan.clone());
        let folded = fold(log.group, None, &with_orphan);
        assert_eq!(folded.pending, vec![orphan.hash()]);
        assert!(folded.state.is_member(&bob.key()));

        let mut state = log.state();
        let rename = owner.op(log.group, state.heads.clone(), OpKind::Rename { name: "n".into() });
        assert!(extend(&mut state, &rename).is_some());
        log.ops.push(rename);
        assert_eq!(state, log.state());
        // A concurrent op does not extend the heads.
        assert!(extend(&mut state, &orphan).is_none());
    }
}
