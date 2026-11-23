//! Changing a group: commands that sign an op, and applying ops from any source (our own,
//! group messages, a welcome, own-device sync). Applying refolds the op log, writes system lines
//! ("Bob joined"), and starts what a change implies: a rekey after a removal, leaving the topic
//! when we were removed, fetching a new group photo.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail, ensure};

use super::GroupRuntime;
use crate::{
    api::{DeliveryStatus, Direction, Event, GroupStatus},
    clock::now_ms,
    group::{
        GroupId, GroupOp, OpKind, group_id, is_valid_group_name,
        epochs::Epoch,
        keys::random_secret,
        message::GroupPayload,
        oplog::{self, Change, FoldedOp},
    },
    identity::UserKey,
    node::Node,
    wire::{Body, ChatMessage, MAX_AVATAR_BYTES},
};

/// Where an op came from: a local command, a group message (in an epoch, so an `Add` derives
/// the next epoch), or a bulk transfer (welcome, own-device sync) that writes no system lines.
#[derive(Debug, Clone, Copy)]
pub(super) enum OpSource {
    Local,
    Message,
    Bulk,
}

impl Node {
    pub(in crate::node) fn create_group(
        &mut self,
        name: String,
        announcement: bool,
        members: Vec<UserKey>,
    ) -> Result<()> {
        let name = name.trim().to_owned();
        ensure!(is_valid_group_name(&name), "the group name is empty or too long");
        let me = self.active_me()?;
        let nonce: [u8; 16] = rand::random();
        let group = group_id(&me.user_key(), &nonce);
        let kind = OpKind::Create {
            name,
            announcement,
            nonce,
            creator: me.profile.clone(),
            device_keys: self.known_own_device_keys()?,
        };
        let create = GroupOp::new(&me.user, group, Vec::new(), now_ms(), kind);
        ensure!(create.verify(), "could not sign the group");
        self.store.insert_group_op(&create)?;
        let folded = oplog::fold(group, None, std::slice::from_ref(&create));
        let runtime = GroupRuntime::new(folded, vec![create], GroupStatus::Active);
        self.groups.insert(group, runtime);
        self.refresh_devices(&group)?;
        let root = Epoch::root(random_secret(), self.group(&group)?.state().heads.clone(), now_ms());
        self.store_epoch(&group, root)?;
        self.save_group_state(&group)?;
        self.update_sending_epoch(&group)?;
        self.system_line(&group, None, "You created the group".to_owned(), now_ms())?;
        self.join_topic(&group)?;
        self.update_group_tags()?;
        self.sync_group_to_own_devices(&group)?;
        if !members.is_empty() {
            self.invite_to_group(group, members)?;
        }
        self.flush_outbox()
    }

    /// Signs `kind` on the current heads, checks it is allowed, posts it to the group and
    /// applies it.
    pub(in crate::node) fn change_group(&mut self, group: GroupId, kind: OpKind) -> Result<GroupOp> {
        self.active_me()?;
        let runtime = self.group(&group)?;
        ensure!(runtime.is_active(), "you are no longer in this group");
        let heads = runtime.state().heads.clone();
        let op = GroupOp::new(&self.active_me()?.user, group, heads, now_ms(), kind);
        let mut trial = self.group(&group)?.state().clone();
        let allowed = oplog::extend(&mut trial, &op).is_some_and(|folded| folded.change.is_some());
        ensure!(op.verify() && allowed, "you are not allowed to do that in this group");
        let posted_in = self.post_group_payload(&group, GroupPayload::Op(Box::new(op.clone())), None)?;
        self.apply_ops(&group, vec![op.clone()], OpSource::Local)?;
        if matches!(op.kind, OpKind::Add { .. })
            && let Some(epoch) = self.group(&group)?.epochs.get(&posted_in).cloned()
        {
            self.store_epoch(&group, epoch.after_add(&op, now_ms()))?;
            self.update_sending_epoch(&group)?;
        }
        Ok(op)
    }

    pub(in crate::node) fn rename_group(&mut self, group: GroupId, name: String) -> Result<()> {
        let name = name.trim().to_owned();
        ensure!(is_valid_group_name(&name), "the group name is empty or too long");
        self.change_group(group, OpKind::Rename { name }).map(drop)
    }

    pub(in crate::node) fn set_group_avatar(&mut self, group: GroupId, image: Vec<u8>) -> Result<()> {
        ensure!(image.len() <= MAX_AVATAR_BYTES, "group photo is too large");
        let blob = (!image.is_empty()).then(|| *blake3::hash(&image).as_bytes());
        if let Some(hash) = &blob {
            self.store.put_blob(hash, &image, now_ms())?;
        }
        self.change_group(group, OpKind::SetAvatar { blob })?;
        self.store.set_avatar(&group, &image)?;
        self.emit_avatar(&group, &image);
        Ok(())
    }

    pub(in crate::node) fn leave_group(&mut self, group: GroupId) -> Result<()> {
        self.change_group(group, OpKind::Leave)?;
        self.sync_group_to_own_devices(&group)?;
        self.flush_outbox()
    }

    /// Stores and folds `ops`; returns the ones that took effect now.
    pub(super) fn apply_ops(
        &mut self,
        group: &GroupId,
        ops: Vec<GroupOp>,
        source: OpSource,
    ) -> Result<Vec<FoldedOp>> {
        let mut fresh = Vec::new();
        for op in ops {
            if op.group != *group || !op.verify() {
                continue;
            }
            let covered = self
                .group(group)?
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.covers(&op.hash()));
            if !covered && self.store.insert_group_op(&op)? {
                fresh.push(op);
            }
        }
        if fresh.is_empty() {
            return Ok(Vec::new());
        }
        let before = self.group(group)?.state().clone();
        let runtime = self.group(group)?;
        let applied = if let [op] = fresh.as_slice()
            && let Some(folded) = oplog::extend(&mut runtime.folded.state, op)
        {
            runtime.ops.push(op.clone());
            runtime.folded.order.push(folded.clone());
            vec![folded]
        } else {
            let known: HashSet<[u8; 32]> = runtime.folded.order.iter().map(|op| op.hash).collect();
            runtime.ops.extend(fresh);
            runtime.folded = oplog::fold(*group, runtime.checkpoint.as_ref(), &runtime.ops);
            runtime
                .folded
                .order
                .iter()
                .filter(|op| !known.contains(&op.hash))
                .cloned()
                .collect()
        };
        self.after_changes(group, &before, &applied, source)?;
        Ok(applied)
    }

    /// Side effects of newly applied ops.
    fn after_changes(
        &mut self,
        group: &GroupId,
        before: &oplog::GroupState,
        applied: &[FoldedOp],
        source: OpSource,
    ) -> Result<()> {
        let me = self.me()?.user_key();
        self.refresh_devices(group)?;
        let after = self.group(group)?.state().clone();
        let mut names: HashMap<UserKey, String> = HashMap::new();
        for state in [before, &after] {
            for (user, member) in &state.members {
                names.insert(*user, member.profile.display_name.clone());
            }
        }
        let name = |user: &UserKey| -> String {
            if *user == me {
                return "You".to_owned();
            }
            names.get(user).cloned().unwrap_or_else(|| "Someone".to_owned())
        };
        let mut removed_me = None;
        let mut rekey_mine = None;
        for folded in applied {
            let Some(change) = &folded.change else { continue };
            if change.needs_rekey() {
                let mine = folded.author == me;
                rekey_mine = Some(rekey_mine.unwrap_or(false) || mine);
            }
            match change {
                Change::Removed(user) if *user == me => removed_me = Some(GroupStatus::Removed),
                Change::Left { user, .. } if *user == me => removed_me = Some(GroupStatus::Left),
                Change::Avatar(blob) => self.group_avatar_changed(group, *blob, Some(folded.author))?,
                _ => {}
            }
            if matches!(source, OpSource::Bulk) {
                continue;
            }
            for line in system_lines(change, &name(&folded.author), &name) {
                self.system_line(group, Some(folded.hash), line, folded.at_ms)?;
            }
        }
        if let Some(status) = removed_me {
            return self.deactivate_group(group, status);
        }
        self.save_group_state(group)?;
        if let Some(mine) = rekey_mine {
            self.schedule_rekey(group, mine);
        }
        self.update_sending_epoch(group)
    }

    /// A line about the group itself in its chat, keyed by the op (so it is written once).
    pub(super) fn system_line(
        &mut self,
        group: &GroupId,
        op: Option<[u8; 32]>,
        text: String,
        at_ms: u64,
    ) -> Result<()> {
        let conversation = Self::conversation_of(group);
        let id = op.map_or_else(rand::random, |hash| {
            let mut id = [0; 16];
            id.copy_from_slice(&hash[..16]);
            id
        });
        let message = ChatMessage {
            id,
            sent_at_ms: at_ms,
            body: Body::Text(text),
        };
        if self.store.insert_group_message(
            &conversation,
            &message,
            Direction::Incoming,
            DeliveryStatus::Delivered,
            &[],
        )? && let Some(view) = self.store.message_view(&conversation, &id)?
        {
            self.events.emit(Event::MessageAdded(view));
        }
        Ok(())
    }

    /// Fetches a new group photo from members unless we have it.
    fn group_avatar_changed(
        &mut self,
        group: &GroupId,
        blob: Option<[u8; 32]>,
        prefer: Option<UserKey>,
    ) -> Result<()> {
        let Some(hash) = blob else {
            self.store.set_avatar(group, &[])?;
            self.emit_avatar(group, &[]);
            return Ok(());
        };
        if let Some(image) = self.store.blob_range(&hash, 0, MAX_AVATAR_BYTES as u64)? {
            self.store.set_avatar(group, &image)?;
            self.emit_avatar(group, &image);
            return Ok(());
        }
        self.fetch_blob(group, hash, 0, MAX_AVATAR_BYTES as u64, prefer, super::net::BlobPurpose::Avatar)
    }

    /// Fetches the group photo if the group has one we do not hold (after joining or a sync).
    pub(super) fn ensure_group_photo(&mut self, group: &GroupId) -> Result<()> {
        let Some(hash) = self.group(group)?.state().avatar else {
            return Ok(());
        };
        if self.store.avatar(group)?.is_some() {
            return Ok(());
        }
        let owner = self.group(group)?.state().owner;
        self.group_avatar_changed(group, Some(hash), owner)
    }

    pub(in crate::node) fn load_members(
        &mut self,
        group: crate::group::GroupId,
        query: String,
        offset: u32,
        limit: u32,
    ) -> Result<()> {
        let (members, total) = self.store.group_members_page(&group, &query, offset, limit)?;
        self.events.emit(Event::Members {
            group,
            query,
            offset,
            members,
            total,
        });
        Ok(())
    }

    pub(in crate::node) fn require_admin(&mut self, group: &GroupId) -> Result<()> {
        let me = self.active_me()?.user_key();
        let runtime = self.group(group)?;
        if !runtime.is_active() || !runtime.state().is_admin(&me) {
            bail!("only the owner and admins can do that");
        }
        Ok(())
    }
}

impl GroupRuntime {
    pub(super) fn new(folded: oplog::Folded, ops: Vec<GroupOp>, status: GroupStatus) -> Self {
        Self {
            folded,
            checkpoint: None,
            ops,
            status,
            devices: HashMap::new(),
            epochs: HashMap::new(),
            sending: None,
            pump: None,
            sender: None,
            neighbors: 0,
            letter_relays: Vec::new(),
            rekey_due_ms: None,
            heal: false,
            key_requests: HashMap::new(),
            unknown_since: HashMap::new(),
            pending_grants: HashMap::new(),
            rate: HashMap::new(),
            last_catch_up_ms: 0,
            catching_up: false,
            unpublished: Vec::new(),
            fetching: HashSet::new(),
            slot_only: HashSet::new(),
            joined_at_ms: now_ms(),
            checkpoint_tried_at: 0,
            seen: HashMap::new(),
            last_join_peers_ms: 0,
        }
    }
}

/// What the chat shows for a change, from the author's point of view.
fn system_lines(change: &Change, author: &str, name: &dyn Fn(&UserKey) -> String) -> Vec<String> {
    match change {
        Change::Created => Vec::new(),
        Change::Added(user) => vec![format!("{author} added {}", name(user))],
        Change::Removed(user) => vec![format!("{author} removed {}", name(user))],
        Change::Left { user, new_owner } => {
            let mut lines = vec![format!("{} left", name(user))];
            if let Some(owner) = new_owner {
                lines.push(format!("{} is now the owner", name(owner)));
            }
            lines
        }
        Change::RoleSet { user, admin: true } => vec![format!("{} is now an admin", name(user))],
        Change::RoleSet { user, admin: false } => {
            vec![format!("{} is no longer an admin", name(user))]
        }
        Change::Renamed(group_name) => vec![format!("{author} renamed the group to “{group_name}”")],
        Change::Announcement(true) => vec!["Only admins can post now".to_owned()],
        Change::Announcement(false) => vec!["Everyone can post now".to_owned()],
        Change::Avatar(_) => vec![format!("{author} changed the group photo")],
        Change::HistorySharing(true) => vec!["New members see the last days of history".to_owned()],
        Change::HistorySharing(false) => vec!["New members no longer see earlier messages".to_owned()],
    }
}
