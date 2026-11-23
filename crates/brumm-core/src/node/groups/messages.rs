//! Group messages in the node: sealing with our sender chain in the sending epoch, opening
//! with the sender's chain, and applying the payloads (chat, deletes, reactions, files, ops,
//! checkpoints, device lists and device keys).

use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};

use super::{at_rest_key, ops::OpSource};
use crate::{
    api::{ConversationId, DeliveryStatus, Direction, Event},
    clock::now_ms,
    group::{
        GroupId, GroupOp, OpHash, OpKind,
        checkpoint::{self, Checkpoint},
        keys::{ChainState, EpochId, open_at_rest, open_with_key, random_secret, seal_at_rest, seal_with_key},
        message::{GroupMessage, GroupPayload, GroupWire, MAX_CHECKPOINT_BYTES},
        oplog,
    },
    identity::UserKey,
    node::Node,
    wire::{self, Body, ChatMessage, MAX_DELETE_IDS, MessageId, Reaction},
};

/// Associated data of a group file's ciphertext.
const FILE_AAD: &[u8] = b"brumm group file v1";
/// A unit stamped this much before we joined is from before our time, not from a member with a
/// lagging clock (whose units must not be dropped).
const JOIN_CLOCK_SKEW_MS: u64 = 60 * 60 * 1000;
/// How long a checkpoint waits for the ops it covers before it is dropped.
const CHECKPOINT_WAIT_MS: u64 = 10 * 60 * 1000;
/// ChaCha20-Poly1305 tag after a file's ciphertext.
const FILE_TAG_BYTES: u64 = 16;

impl Node {
    /// Seals `payload` in the sending epoch with our chain and publishes it. Returns the epoch.
    pub(in crate::node) fn post_group_payload(
        &mut self,
        group: &GroupId,
        payload: GroupPayload,
        track: Option<MessageId>,
    ) -> Result<EpochId> {
        let device_secret = self.device_secret()?;
        let own = *self.me()?.device_id.as_bytes();
        let epoch = self.sending_epoch(group)?;
        let mut chain = self
            .load_chain(group, &epoch.id, &own)?
            .unwrap_or_else(|| ChainState::new(&epoch.secret, &own));
        let (counter, key) = chain.next_key();
        self.save_chain(group, &epoch.id, &own, &chain)?;
        let message = GroupMessage::seal(&device_secret, *group, epoch.id, counter, &key, now_ms(), &payload);
        self.publish_unit(group, GroupWire::Message(message), epoch.secret, track)?;
        Ok(epoch.id)
    }

    fn load_chain(&self, group: &GroupId, epoch: &EpochId, device: &[u8; 32]) -> Result<Option<ChainState>> {
        let key = at_rest_key(&self.device_secret()?);
        Ok(self
            .store
            .chain(group, epoch, device)?
            .and_then(|sealed| open_at_rest(&key, &sealed))
            .and_then(|plain| wire::decode(&plain).ok()))
    }

    fn save_chain(&self, group: &GroupId, epoch: &EpochId, device: &[u8; 32], chain: &ChainState) -> Result<()> {
        let key = at_rest_key(&self.device_secret()?);
        self.store
            .save_chain(group, epoch, device, &seal_at_rest(&key, &wire::encode(chain)))
    }

    /// `SendText` / `SendFile` in a group.
    pub(in crate::node) fn send_group_message(
        &mut self,
        group: GroupId,
        id: MessageId,
        body: Body,
    ) -> Result<()> {
        let me = self.active_me()?.user_key();
        let runtime = self.group(&group)?;
        ensure!(runtime.is_active(), "you are no longer in this group");
        ensure!(runtime.state().may_post(&me), "only admins can post in this group");
        let message = ChatMessage {
            id,
            sent_at_ms: now_ms(),
            body,
        };
        ensure!(message.body.is_valid(), "message is empty, too long or the file is too large");
        let conversation = Self::conversation_of(&group);
        self.store.insert_group_message(
            &conversation,
            &message,
            Direction::Outgoing,
            DeliveryStatus::Pending,
            &me,
        )?;
        if let Some(view) = self.store.message_view(&conversation, &message.id)? {
            self.events.emit(Event::MessageAdded(view));
        }
        let payload = match message.body {
            Body::Text(_) => GroupPayload::Chat(message.clone()),
            Body::File { name, data } => {
                let key = random_secret();
                let ciphertext = seal_with_key(&key, FILE_AAD, &data);
                let blob = *blake3::hash(&ciphertext).as_bytes();
                self.store.put_blob(&blob, &ciphertext, now_ms())?;
                GroupPayload::File {
                    id: message.id,
                    sent_at_ms: message.sent_at_ms,
                    name,
                    size: data.len() as u64,
                    blob,
                    key,
                }
            }
        };
        self.post_group_payload(&group, payload, Some(message.id))?;
        Ok(())
    }

    pub(in crate::node) fn delete_group_messages(
        &mut self,
        group: GroupId,
        ids: Vec<MessageId>,
        for_everyone: bool,
    ) -> Result<()> {
        let conversation = Self::conversation_of(&group);
        ensure!(!ids.is_empty() && ids.len() <= MAX_DELETE_IDS, "nothing to delete or too many messages at once");
        if for_everyone && !self.store.all_outgoing(&conversation, &ids)? {
            bail!("you can only delete your own messages for everyone");
        }
        let removed = self.store.delete_messages(&conversation, &ids, None)?;
        if removed.is_empty() {
            return Ok(());
        }
        self.events.emit(Event::MessagesDeleted {
            conversation,
            ids: removed.clone(),
        });
        self.emit_preview(&conversation)?;
        if for_everyone {
            self.post_group_payload(&group, GroupPayload::Delete(removed), None)?;
        }
        Ok(())
    }

    pub(in crate::node) fn react_in_group(&mut self, group: GroupId, id: MessageId, emoji: String) -> Result<()> {
        let me = self.active_me()?.user_key();
        let conversation = Self::conversation_of(&group);
        let reaction = Reaction {
            message_id: id,
            emoji,
            at_ms: now_ms(),
        };
        ensure!(
            reaction.is_valid() && self.store.message_exists(&conversation, &id)?,
            "cannot react to this message"
        );
        if let Some(reactions) = self.store.set_group_reaction(&conversation, &me, true, &reaction)? {
            self.events.emit(Event::ReactionsChanged {
                conversation,
                id,
                reactions,
            });
        }
        self.post_group_payload(&group, GroupPayload::Reaction(reaction), None)?;
        Ok(())
    }

    /// `Ok(true)`: done with this unit (applied or dropped for good); `Ok(false)`: keep it and
    /// try again later (epoch, ops or a blob missing).
    pub(super) fn process_unit(&mut self, group: &GroupId, id: &[u8; 32], unit: &GroupWire) -> Result<bool> {
        let author = unit.author_device();
        let Some(user) = self.group(group)?.member_of_device(&author) else {
            return Ok(true);
        };
        match unit {
            GroupWire::Message(message) => self.receive_group_message(group, id, message, user),
            GroupWire::Rekey(rekey) => self.receive_rekey(group, rekey, id),
            GroupWire::KeyRequest(request) => {
                self.receive_key_request(group, request, &user)?;
                Ok(true)
            }
            GroupWire::KeyGrant(grant) => {
                self.receive_key_grant(group, grant)?;
                Ok(true)
            }
        }
    }

    fn receive_group_message(
        &mut self,
        group: &GroupId,
        id: &[u8; 32],
        message: &GroupMessage,
        user: UserKey,
    ) -> Result<bool> {
        let runtime = self.group(group)?;
        let Some(epoch) = runtime.epochs.get(&message.epoch).cloned() else {
            // Catch-up also hands over units sealed before we joined: nobody grants those keys.
            if message.sent_at_ms.saturating_add(JOIN_CLOCK_SKEW_MS) < runtime.joined_at_ms {
                return Ok(true);
            }
            runtime.unknown_since.entry(message.epoch).or_insert_with(now_ms);
            return Ok(false);
        };
        let mut chain = self
            .load_chain(group, &epoch.id, &message.sender_device)?
            .unwrap_or_else(|| ChainState::new(&epoch.secret, &message.sender_device));
        let Some(key) = chain.key_for(message.counter) else {
            tracing::debug!("group message key already used or too far ahead");
            return Ok(true);
        };
        let Some(payload) = message.open(&key) else {
            tracing::warn!("group message does not decrypt");
            return Ok(true);
        };
        let processed = self.apply_group_payload(group, id, &epoch.id, user, message.sent_at_ms, payload)?;
        if processed {
            self.save_chain(group, &epoch.id, &message.sender_device, &chain)?;
        }
        Ok(processed)
    }

    fn apply_group_payload(
        &mut self,
        group: &GroupId,
        unit: &[u8; 32],
        epoch: &EpochId,
        user: UserKey,
        sent_at_ms: u64,
        payload: GroupPayload,
    ) -> Result<bool> {
        let me = self.me()?.user_key();
        let conversation = Self::conversation_of(group);
        match payload {
            GroupPayload::Chat(message) => {
                if !message.body.is_valid() || !self.group(group)?.state().may_post(&user) {
                    return Ok(true);
                }
                self.insert_from_member(&conversation, &message, &user, &me)?;
                Ok(true)
            }
            GroupPayload::File { id, sent_at_ms, name, size, blob, key } => {
                let len = size + FILE_TAG_BYTES;
                let Some(ciphertext) = self.store.blob_range(&blob, 0, len)? else {
                    self.fetch_blob(group, blob, 0, len, Some(user), super::net::BlobPurpose::File { unit: *unit })?;
                    return Ok(false);
                };
                let Some(data) = open_with_key(&key, FILE_AAD, &ciphertext) else {
                    return Ok(true);
                };
                let message = ChatMessage {
                    id,
                    sent_at_ms,
                    body: Body::File { name, data },
                };
                if message.body.is_valid() && self.group(group)?.state().may_post(&user) {
                    self.insert_from_member(&conversation, &message, &user, &me)?;
                }
                Ok(true)
            }
            GroupPayload::Delete(ids) => {
                if ids.len() > MAX_DELETE_IDS {
                    return Ok(true);
                }
                let mut allowed = Vec::new();
                for id in ids {
                    match self.store.message_sender(&conversation, &id)? {
                        Some(sender) if sender == user => allowed.push(id),
                        None if !self.store.message_exists(&conversation, &id)? => allowed.push(id),
                        _ => {}
                    }
                }
                let removed = self.store.delete_messages(&conversation, &allowed, None)?;
                if !removed.is_empty() {
                    self.events.emit(Event::MessagesDeleted { conversation, ids: removed });
                    self.emit_preview(&conversation)?;
                }
                Ok(true)
            }
            GroupPayload::Reaction(reaction) => {
                if reaction.is_valid()
                    && self.store.message_exists(&conversation, &reaction.message_id)?
                    && let Some(reactions) =
                        self.store.set_group_reaction(&conversation, &user, user == me, &reaction)?
                {
                    self.events.emit(Event::ReactionsChanged {
                        conversation,
                        id: reaction.message_id,
                        reactions,
                    });
                }
                Ok(true)
            }
            GroupPayload::Op(op) => {
                let op = *op;
                if op.author != user {
                    return Ok(true);
                }
                let is_add = matches!(op.kind, OpKind::Add { .. });
                self.apply_ops(group, vec![op.clone()], OpSource::Message)?;
                if is_add && op.verify() && let Some(parent) = self.group(group)?.epochs.get(epoch).cloned() {
                    self.install_epoch(group, parent.after_add(&op, now_ms()))?;
                }
                let _ = sent_at_ms;
                Ok(true)
            }
            GroupPayload::Checkpoint(checkpoint) => self.receive_checkpoint(group, *checkpoint, sent_at_ms),
            GroupPayload::CheckpointBlob { blob, len } => {
                if len > MAX_CHECKPOINT_BYTES || !self.group(group)?.state().is_admin(&user) {
                    return Ok(true);
                }
                let Some(bytes) = self.store.blob_range(&blob, 0, len)?.filter(|bytes| bytes.len() as u64 == len) else {
                    self.fetch_blob(group, blob, 0, len, Some(user), super::net::BlobPurpose::File { unit: *unit })?;
                    return Ok(false);
                };
                match wire::decode::<Checkpoint>(&bytes) {
                    Ok(checkpoint) if checkpoint.author == user => {
                        self.receive_checkpoint(group, checkpoint, sent_at_ms)
                    }
                    _ => Ok(true),
                }
            }
            GroupPayload::DeviceList(list) => {
                if list.user_key != user || !list.verify() || user == me {
                    return Ok(true);
                }
                let state = self.group(group)?.state().clone();
                let before = self.device_list_of(&user, &state)?;
                if !self.store.save_peer_device_list(&list)? {
                    return Ok(true);
                }
                let revoked = before.is_some_and(|before| {
                    before
                        .entries
                        .iter()
                        .any(|entry| list.entry(&entry.device_id()).is_none())
                });
                self.refresh_devices(group)?;
                if revoked {
                    self.group(group)?.heal = true;
                    self.schedule_rekey(group, false);
                }
                Ok(true)
            }
            GroupPayload::DeviceKeys(keys) => {
                let state = self.group(group)?.state().clone();
                let listed = self
                    .device_list_of(&user, &state)?
                    .is_some_and(|list| list.entries.iter().any(|entry| *entry.device_id().as_bytes() == keys.device_id));
                if keys.verify() && listed {
                    self.store.save_device_keys(&user, &keys)?;
                }
                Ok(true)
            }
        }
    }

    fn insert_from_member(
        &mut self,
        conversation: &ConversationId,
        message: &ChatMessage,
        user: &UserKey,
        me: &UserKey,
    ) -> Result<()> {
        let (direction, status) = if user == me {
            (Direction::Outgoing, DeliveryStatus::Sent)
        } else {
            (Direction::Incoming, DeliveryStatus::Delivered)
        };
        if self
            .store
            .insert_group_message(conversation, message, direction, status, user)?
            && let Some(view) = self.store.message_view(conversation, &message.id)?
        {
            self.events.emit(Event::MessageAdded(view));
        }
        Ok(())
    }

    /// Installs a checkpoint that our own log confirms. `Ok(false)`: it covers ops we do not
    /// have yet (they come with the same catch-up), so the unit waits, for at most
    /// [`CHECKPOINT_WAIT_MS`] after it was sent.
    fn receive_checkpoint(&mut self, group: &GroupId, checkpoint: Checkpoint, sent_at_ms: u64) -> Result<bool> {
        let runtime = self.group(group)?;
        let current = runtime.checkpoint.clone();
        let newer = current.as_ref().is_none_or(|current| checkpoint.supersedes(current));
        if checkpoint.group != *group || !newer || !checkpoint.verify() {
            return Ok(true);
        }
        let known: HashSet<OpHash> = runtime.ops.iter().map(GroupOp::hash).collect();
        let missing_ops = checkpoint.covered.iter().any(|hash| {
            !known.contains(hash) && !current.as_ref().is_some_and(|current| current.covers(hash))
        });
        if missing_ops {
            return Ok(now_ms() > sent_at_ms.saturating_add(CHECKPOINT_WAIT_MS));
        }
        if !checkpoint.matches_log(current.as_ref(), &runtime.ops) {
            return Ok(true);
        }
        self.store.set_group_checkpoint(group, &checkpoint)?;
        self.store.prune_group_ops(group, &checkpoint.covered)?;
        let runtime = self.group(group)?;
        runtime.ops.retain(|op| !checkpoint.covers(&op.hash()));
        runtime.folded = oplog::fold(*group, Some(&checkpoint), &runtime.ops);
        runtime.checkpoint = Some(checkpoint);
        Ok(true)
    }

    /// Owner/admin devices write the next checkpoint once enough ops piled up. It is a blob
    /// (megabytes in a large group); the unit only names it.
    pub(in crate::node) fn checkpoint_if_due(&mut self, group: &GroupId) -> Result<()> {
        let me = self.me()?.user_key();
        let interval = self.group_timing.checkpoint_ops;
        let runtime = self.group(group)?;
        let seq = runtime.state().seq;
        if !runtime.state().is_admin(&me)
            || runtime.checkpoint_tried_at == seq
            || !checkpoint::is_due(runtime.state(), runtime.checkpoint.as_ref(), interval)
        {
            return Ok(());
        }
        runtime.checkpoint_tried_at = seq;
        let runtime = self.groups.get(group).context("group loaded above")?;
        let user = &self.me.as_ref().context("no identity")?.user;
        let next = Checkpoint::new(user, runtime.state(), runtime.checkpoint.as_ref(), &runtime.ops);
        let bytes = wire::encode(&next);
        let blob = next.hash();
        // Applied here first: a failed post must not make it due again. Members that miss the
        // announcement get another admin's checkpoint or our next one.
        self.receive_checkpoint(group, next, now_ms())?;
        if self.group(group)?.checkpoint.as_ref().map(Checkpoint::hash) != Some(blob) {
            tracing::warn!("own checkpoint rejected by our own log check");
            return Ok(());
        }
        self.store.put_blob(&blob, &bytes, now_ms())?;
        let payload = GroupPayload::CheckpointBlob {
            blob,
            len: bytes.len() as u64,
        };
        if let Err(err) = self.post_group_payload(group, payload, None) {
            tracing::info!(?err, "checkpoint not announced to the group");
        }
        Ok(())
    }

    /// Tries again the units that waited for an epoch, op or blob.
    pub(in crate::node) fn retry_unprocessed(&mut self, group: &GroupId) -> Result<()> {
        for (id, bytes) in self.store.log_unprocessed(group)? {
            if self.group(group)?.fetching.contains(&id) {
                continue;
            }
            self.retry_bytes(group, &id, &bytes)?;
        }
        Ok(())
    }

    pub(super) fn retry_unit(&mut self, group: &GroupId, id: &[u8; 32]) -> Result<()> {
        if let Some(bytes) = self.store.log_get(group, &[*id])?.pop() {
            self.retry_bytes(group, id, &bytes)?;
        }
        Ok(())
    }

    fn retry_bytes(&mut self, group: &GroupId, id: &[u8; 32], bytes: &[u8]) -> Result<()> {
        let Ok(unit) = wire::decode::<GroupWire>(bytes) else {
            return self.store.log_mark_processed(group, id);
        };
        if self.process_unit(group, id, &unit)? {
            self.store.log_mark_processed(group, id)?;
        }
        Ok(())
    }

    /// Our own key record into a group we just joined from another of our devices.
    pub(super) fn announce_device_keys(&mut self, group: &GroupId) -> Result<()> {
        let keys = self.own_device_keys()?;
        self.post_group_payload(group, GroupPayload::DeviceKeys(keys), None)
            .map(drop)
            .context("could not announce this device to the group")
    }

    /// Our new device list into every active group (after linking or removing a device).
    pub(in crate::node) fn announce_device_list_to_groups(&mut self) -> Result<()> {
        let list = self.me()?.profile.devices.clone();
        for group in self.store.active_group_ids()? {
            if let Err(err) = self.post_group_payload(&group, GroupPayload::DeviceList(list.clone()), None) {
                tracing::info!(?err, "device list not posted to a group");
            }
        }
        Ok(())
    }
}
