//! Group distribution (SPEC.md §14.4):
//!
//! 1. **Gossip**: one iroh-gossip topic per group, bootstrapped from member devices. Every
//!    unit is verified (device signature, member device, rate limit) before it is stored.
//!    iroh-gossip has no validation hook, so forwarding happens before our check; outsiders do
//!    not know the topic id (derived from the group id) and anything they inject is dropped.
//! 2. **Catch-up from members**: range-based reconciliation (`group::sync`) of our log against
//!    a few member devices, then a fetch of what we lack. Devices serve their log for
//!    [`super::LOG_SERVE_WINDOW_MS`]; browsers do not serve.
//! 3. **Nostr fallback**: a unit that reached no gossip neighbour goes to the relays once,
//!    under a tag derived from its epoch secret.
//! 4. **Blobs**: rekey slots, files and the group photo are fetched by hash from members.

use std::collections::HashMap;

use anyhow::Result;
use iroh::EndpointId;
use rand::seq::SliceRandom;

use super::{DeviceKey, GroupRuntime, LOG_SERVE_WINDOW_MS, RATE_LIMIT_MESSAGES, RATE_LIMIT_WINDOW_MS, device_endpoint};
use crate::{
    api::{DeliveryStatus, Event},
    clock::now_ms,
    group::{
        GroupId,
        join::BlobRef,
        message::{GroupWire, MAX_WIRE_BYTES},
        sync::{self, Item, ItemId},
    },
    mailbox::{self, GroupLetter},
    node::{Node, Outcome},
    wire::{Frame, GroupLogRequest, MessageId, Reply},
};

const TOPIC_CONTEXT: &str = "brumm group topic v1";
/// Bootstrap peers handed to gossip when joining a topic.
const MAX_BOOTSTRAP_PEERS: usize = 16;
/// Members a catch-up round reconciles with.
const CATCH_UP_PEERS: usize = 3;
/// Reconciliation steps before giving up on a peer (normally a handful).
const MAX_RECONCILE_STEPS: usize = 32;
/// Log entries per fetch request.
const FETCH_BATCH: usize = 256;
/// Bytes of log entries per fetch reply.
const MAX_FETCH_REPLY_BYTES: usize = 4 * 1024 * 1024;
/// Member devices a blob is asked from, one after another.
const MAX_BLOB_SOURCES: usize = 5;
/// One in this many picked peers is one we have not seen online (to discover devices).
const EXPLORE_SHARE: usize = 4;

/// What the gossip pump forwards to the node.
pub(in crate::node) struct GossipEvent {
    pub group: GroupId,
    pub kind: GossipKind,
}

pub(in crate::node) enum GossipKind {
    Joined(iroh_gossip::GossipSender),
    Received(Vec<u8>),
    Neighbors(usize),
}

/// Why a blob was fetched.
#[derive(Debug, Clone, Copy)]
pub(in crate::node) enum BlobPurpose {
    Avatar,
    /// A group file message waiting in the log.
    File { unit: [u8; 32] },
    /// Our slot of a large rekey.
    Slot { unit: [u8; 32] },
    /// A whole large rekey blob, kept to serve other members' slots (desktop only).
    Replica { unit: [u8; 32] },
}

impl BlobPurpose {
    /// The log entry waiting for this blob, if any.
    fn unit(self) -> Option<[u8; 32]> {
        match self {
            Self::Avatar => None,
            Self::File { unit } | Self::Slot { unit } | Self::Replica { unit } => Some(unit),
        }
    }
}

/// Where a unit came from; only live gossip is rate limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::node) enum UnitSource {
    Gossip,
    CatchUp,
    Letter,
}

fn topic_id(group: &GroupId) -> iroh_gossip::TopicId {
    iroh_gossip::TopicId::from_bytes(blake3::derive_key(TOPIC_CONTEXT, group))
}

impl GroupRuntime {
    /// A member device showed it is online at `at_ms`.
    pub(super) fn note_seen(&mut self, device: &[u8; 32], at_ms: u64) {
        let last = self.seen.entry(*device).or_insert(0);
        *last = (*last).max(at_ms.min(now_ms()));
    }

    /// Up to `count` member devices to talk to: `prefer`'s devices first, then the most recently
    /// seen, plus a share never seen (or long gone) so that devices nobody heard of yet are found.
    /// In a big group most devices are offline; picking at random rarely reaches the online ones.
    pub(super) fn pick_peers(&self, own: &DeviceKey, count: usize, prefer: Option<crate::identity::UserKey>) -> Vec<[u8; 32]> {
        let (preferred, others): (Vec<[u8; 32]>, Vec<[u8; 32]>) = self
            .peer_devices(own)
            .into_iter()
            .partition(|device| prefer.is_some() && self.devices.get(device) == prefer.as_ref());
        let (mut seen, unseen): (Vec<[u8; 32]>, Vec<[u8; 32]>) =
            others.into_iter().partition(|device| self.seen.contains_key(device));
        seen.sort_by_key(|device| std::cmp::Reverse(self.seen.get(device).copied()));
        choose_peers(preferred, &seen, &unseen, count)
    }

    /// Other member devices, shuffled.
    pub(super) fn peer_devices(&self, own: &[u8; 32]) -> Vec<[u8; 32]> {
        let mut peers: Vec<[u8; 32]> = self
            .devices
            .iter()
            .filter(|(device, user)| *device != own && self.state().is_member(user))
            .map(|(device, _)| *device)
            .collect();
        peers.shuffle(&mut rand::rng());
        peers
    }

    /// Token-bucket-ish window per sender device.
    fn within_rate(&mut self, device: &[u8; 32], now: u64) -> bool {
        let (start, count) = self.rate.entry(*device).or_insert((now, 0));
        if now.saturating_sub(*start) > RATE_LIMIT_WINDOW_MS {
            *start = now;
            *count = 0;
        }
        *count += 1;
        *count <= RATE_LIMIT_MESSAGES
    }
}

impl Node {
    /// Joins the group's gossip topic in the background.
    pub(super) fn join_topic(&mut self, group: &GroupId) -> Result<()> {
        let me = self.active_me()?;
        let transport = me.transport.clone();
        let own = *me.device_id.as_bytes();
        let runtime = self.group(group)?;
        if !runtime.is_active() || runtime.pump.is_some() {
            return Ok(());
        }
        let bootstrap: Vec<EndpointId> = runtime
            .pick_peers(&own, MAX_BOOTSTRAP_PEERS, None)
            .iter()
            .filter_map(|device| device_endpoint(device).ok())
            .collect();
        let events = self.gossip_tx.clone();
        let group = *group;
        let pump = n0_future::task::spawn(async move {
            use n0_future::StreamExt;
            let joined = async {
                let gossip = transport.gossip().await?;
                anyhow::Ok(gossip.subscribe(topic_id(&group), bootstrap).await?)
            }
            .await;
            let topic = match joined {
                Ok(topic) => topic,
                Err(err) => {
                    tracing::warn!(?err, "could not join the group topic");
                    return;
                }
            };
            let (sender, mut receiver) = topic.split();
            let send = |kind| {
                events
                    .try_send(GossipEvent { group, kind })
                    .map_err(|err| tracing::debug!(?err, "gossip event dropped"))
            };
            let _ = send(GossipKind::Joined(sender));
            while let Some(event) = receiver.next().await {
                let kind = match event {
                    Ok(iroh_gossip::Event::Received(message)) => {
                        GossipKind::Received(message.content.to_vec())
                    }
                    Ok(iroh_gossip::Event::NeighborUp(_) | iroh_gossip::Event::NeighborDown(_)) => {
                        GossipKind::Neighbors(receiver.neighbors().count())
                    }
                    Ok(_) => continue,
                    Err(err) => {
                        tracing::debug!(?err, "group topic closed");
                        return;
                    }
                };
                if events.send(GossipEvent { group, kind }).await.is_err() {
                    return;
                }
            }
        });
        self.group(&group)?.pump = Some(pump);
        Ok(())
    }

    pub(in crate::node) fn handle_gossip(&mut self, event: GossipEvent) -> Result<()> {
        let group = event.group;
        if !self.groups.get(&group).is_some_and(GroupRuntime::is_active) {
            return Ok(());
        }
        match event.kind {
            GossipKind::Joined(sender) => {
                self.group(&group)?.sender = Some(sender);
                Ok(())
            }
            GossipKind::Received(bytes) => self.receive_group_unit(bytes, UnitSource::Gossip),
            GossipKind::Neighbors(count) => {
                let runtime = self.group(&group)?;
                let first = runtime.neighbors == 0 && count > 0;
                runtime.neighbors = count;
                if first {
                    self.republish(&group)?;
                    self.start_catch_up(&group)?;
                }
                Ok(())
            }
        }
    }

    /// Stores our unit in the log and sends it: gossip if we have neighbours, else the relays.
    /// `track`: the chat message whose tick follows this unit.
    pub(in crate::node) fn publish_unit(
        &mut self,
        group: &GroupId,
        unit: GroupWire,
        tag_seed: [u8; 32],
        track: Option<MessageId>,
    ) -> Result<()> {
        let bytes = unit.encode();
        anyhow::ensure!(bytes.len() <= MAX_WIRE_BYTES, "too large for the group");
        let id = GroupWire::id(&bytes);
        let now = now_ms();
        self.store.log_insert(group, &id, unit.sent_at_ms(), &bytes, now)?;
        self.store.log_mark_processed(group, &id)?;
        self.group_tags.insert(id, (tag_seed, track));
        self.send_unit(group, id, bytes)
    }

    fn send_unit(&mut self, group: &GroupId, id: [u8; 32], bytes: Vec<u8>) -> Result<()> {
        let mailbox = self.me()?.mailbox.clone();
        let runtime = self.group(group)?;
        let gossip = runtime.sender.clone().filter(|_| runtime.neighbors > 0);
        let relays = runtime.letter_relays.clone();
        let Some((tag_seed, _)) = self.group_tags.get(&id).copied() else {
            return Ok(());
        };
        if gossip.is_none() && mailbox.is_none() {
            self.group(group)?.unpublished.push(id);
            return Ok(());
        }
        let outcomes = self.outcomes_tx.clone();
        let group = *group;
        self.group_posting += 1;
        n0_future::task::spawn(async move {
            let mut delivered = false;
            if let Some(sender) = gossip {
                match sender.broadcast(bytes.clone().into()).await {
                    Ok(()) => delivered = true,
                    Err(err) => tracing::debug!(?err, "gossip broadcast failed"),
                }
            }
            if !delivered && let Some(mailbox) = mailbox {
                let tag = mailbox::group_tag_keys(&tag_seed);
                match mailbox.post_group(&tag, &bytes, &relays).await {
                    Ok(()) => delivered = true,
                    Err(err) => tracing::info!(?err, "group letter not posted"),
                }
            }
            let _ = outcomes.send(Outcome::GroupPublished { group, id, delivered });
        });
        Ok(())
    }

    pub(in crate::node) fn finish_group_publish(
        &mut self,
        group: GroupId,
        id: [u8; 32],
        delivered: bool,
    ) -> Result<()> {
        self.group_posting = self.group_posting.saturating_sub(1);
        if !delivered {
            if let Some(runtime) = self.groups.get_mut(&group)
                && !runtime.unpublished.contains(&id)
            {
                runtime.unpublished.push(id);
            }
            return Ok(());
        }
        let Some((_, track)) = self.group_tags.remove(&id) else {
            return Ok(());
        };
        if let Some(message) = track {
            let conversation = Self::conversation_of(&group);
            self.store.set_status(&conversation, &message, DeliveryStatus::Sent)?;
            self.events.emit(Event::MessageStatusChanged {
                conversation,
                id: message,
                status: DeliveryStatus::Sent,
            });
        }
        Ok(())
    }

    /// Sends our units that reached nobody yet (a neighbour appeared, or the drain on quit).
    pub(in crate::node) fn republish(&mut self, group: &GroupId) -> Result<()> {
        let ids = std::mem::take(&mut self.group(group)?.unpublished);
        for (id, bytes) in ids.iter().zip(self.store.log_get(group, &ids)?) {
            self.send_unit(group, *id, bytes)?;
        }
        Ok(())
    }

    /// Sends everything unpublished (the mailbox just came up, or the shutdown drain; counted in
    /// `group_posting`).
    pub(in crate::node) fn drain_groups(&mut self) -> Result<()> {
        let groups: Vec<GroupId> = self
            .groups
            .iter()
            .filter(|(_, runtime)| !runtime.unpublished.is_empty())
            .map(|(group, _)| *group)
            .collect();
        for group in groups {
            self.republish(&group)?;
        }
        Ok(())
    }

    /// A unit from gossip, catch-up or a letter: verify, store once, process.
    pub(in crate::node) fn receive_group_unit(&mut self, bytes: Vec<u8>, source: UnitSource) -> Result<()> {
        if bytes.len() > MAX_WIRE_BYTES {
            return Ok(());
        }
        let Ok(unit) = crate::wire::decode::<GroupWire>(&bytes) else {
            return Ok(());
        };
        let group = unit.group();
        if !self.store.load_group(&group)?.is_some_and(|stored| stored.status == crate::api::GroupStatus::Active) {
            return Ok(());
        }
        let author = unit.author_device();
        let now = now_ms();
        let runtime = self.group(&group)?;
        if runtime.member_of_device(&author).is_none() || !unit.verify() {
            tracing::debug!("dropping a group unit from a non-member device");
            return Ok(());
        }
        let sent_at = if source == UnitSource::Gossip { now } else { unit.sent_at_ms() };
        runtime.note_seen(&author, sent_at);
        if source == UnitSource::Gossip && !runtime.within_rate(&author, now) {
            tracing::info!("group sender over its rate limit");
            return Ok(());
        }
        let id = GroupWire::id(&bytes);
        if !self.store.log_insert(&group, &id, unit.sent_at_ms(), &bytes, now)? {
            return Ok(());
        }
        if self.process_unit(&group, &id, &unit)? {
            self.store.log_mark_processed(&group, &id)?;
        }
        Ok(())
    }

    pub(in crate::node) fn receive_group_letter(&mut self, letter: GroupLetter) -> Result<()> {
        if letter.log_chunk {
            return self.receive_log_chunk(&letter.unit);
        }
        self.store.advance_mailbox_since(letter.posted_at_secs)?;
        self.receive_group_unit(letter.unit, UnitSource::Letter)
    }

    /// Subscribes the mailbox to the tags of every live epoch plus each group's control tag.
    pub(in crate::node) fn update_group_tags(&mut self) -> Result<()> {
        let Some(mailbox) = self.me.as_ref().and_then(|me| me.mailbox.clone()) else {
            return Ok(());
        };
        let mut tags = Vec::new();
        for (group, runtime) in &self.groups {
            if !runtime.is_active() {
                continue;
            }
            tags.push(mailbox::group_tag_keys(group));
            for epoch in runtime.epochs.values() {
                tags.push(mailbox::group_tag_keys(&epoch.secret));
            }
        }
        let since = self.store.mailbox_since()?;
        let (log_tags, log_since) = self.pending_log_tags()?;
        n0_future::task::spawn(async move {
            if let Err(err) = mailbox.set_group_tags(tags, since).await {
                tracing::info!(?err, "could not subscribe to group letters");
            }
            if let Err(err) = mailbox.set_log_tags(log_tags, log_since).await {
                tracing::info!(?err, "could not subscribe to log chunks");
            }
        });
        Ok(())
    }

    /// Without any gossip neighbour (the bootstrap peers were offline), asks the topic to join
    /// the devices seen online most recently, every catch-up interval.
    pub(super) fn join_seen_peers_if_alone(&mut self, group: &GroupId) -> Result<()> {
        let interval = self.group_timing.catch_up_interval.as_millis() as u64;
        let own = *self.active_me()?.device_id.as_bytes();
        let now = now_ms();
        let runtime = self.group(group)?;
        let Some(sender) = runtime.sender.clone() else {
            return Ok(());
        };
        if runtime.neighbors > 0 || runtime.seen.is_empty() || now.saturating_sub(runtime.last_join_peers_ms) < interval {
            return Ok(());
        }
        runtime.last_join_peers_ms = now;
        let peers: Vec<EndpointId> = runtime
            .pick_peers(&own, MAX_BOOTSTRAP_PEERS, None)
            .iter()
            .filter(|device| runtime.seen.contains_key(*device))
            .filter_map(|device| device_endpoint(device).ok())
            .collect();
        n0_future::task::spawn(async move {
            if let Err(err) = sender.join_peers(peers).await {
                tracing::debug!(?err, "could not join seen peers");
            }
        });
        Ok(())
    }

    // ── catch-up ────────────────────────────────────────────────────────────

    pub(super) fn catch_up_if_due(&mut self, group: &GroupId) -> Result<()> {
        let interval = self.group_timing.catch_up_interval.as_millis() as u64;
        let runtime = self.group(group)?;
        if runtime.catching_up || now_ms().saturating_sub(runtime.last_catch_up_ms) < interval {
            return Ok(());
        }
        self.start_catch_up(group)
    }

    /// Reconciles our log with a few member devices and fetches what we lack.
    fn start_catch_up(&mut self, group: &GroupId) -> Result<()> {
        let me = self.active_me()?;
        let transport = me.transport.clone();
        let own = *me.device_id.as_bytes();
        let since_ms = now_ms().saturating_sub(LOG_SERVE_WINDOW_MS);
        let items = self.store.log_items(group, since_ms)?;
        let runtime = self.group(group)?;
        if runtime.catching_up {
            return Ok(());
        }
        let peers: Vec<EndpointId> = runtime
            .pick_peers(&own, CATCH_UP_PEERS, None)
            .iter()
            .filter_map(|device| device_endpoint(device).ok())
            .collect();
        runtime.last_catch_up_ms = now_ms();
        if peers.is_empty() {
            return Ok(());
        }
        runtime.catching_up = true;
        let outcomes = self.outcomes_tx.clone();
        let group = *group;
        n0_future::task::spawn(async move {
            let mut have: std::collections::HashSet<ItemId> = items.iter().map(|item| item.id).collect();
            for peer in peers {
                match catch_up_with(&transport, peer, group, since_ms, &items, &mut have).await {
                    Ok(units) if !units.is_empty() => {
                        let _ = outcomes.send(Outcome::GroupCaughtUp { group, units });
                    }
                    Ok(_) => {}
                    Err(err) => tracing::debug!(%peer, ?err, "group catch-up failed"),
                }
            }
            let _ = outcomes.send(Outcome::GroupCatchUpDone { group });
        });
        Ok(())
    }

    pub(in crate::node) fn finish_catch_up(&mut self, group: GroupId, units: Vec<Vec<u8>>) -> Result<()> {
        let mut sorted: Vec<(u64, Vec<u8>)> = units
            .into_iter()
            .filter_map(|bytes| {
                let unit = crate::wire::decode::<GroupWire>(&bytes).ok()?;
                Some((unit.sent_at_ms(), bytes))
            })
            .collect();
        // Oldest first: ops before the messages that build on them.
        sorted.sort_by_key(|(at, _)| *at);
        for (_, bytes) in sorted {
            self.receive_group_unit(bytes, UnitSource::CatchUp)?;
        }
        if let Some(runtime) = self.groups.get_mut(&group) {
            runtime.last_catch_up_ms = now_ms();
        }
        Ok(())
    }

    pub(in crate::node) fn catch_up_done(&mut self, group: GroupId) {
        if let Some(runtime) = self.groups.get_mut(&group) {
            runtime.catching_up = false;
        }
    }

    /// Serves a member device's catch-up from our log (not in the browser).
    pub(in crate::node) fn serve_group_log(&mut self, from: EndpointId, request: GroupLogRequest) -> Result<Reply> {
        let refused = Reply::Rejected(crate::wire::RejectReason::UnknownSender);
        if cfg!(target_family = "wasm") {
            return Ok(refused);
        }
        let group = match &request {
            GroupLogRequest::Reconcile { group, .. }
            | GroupLogRequest::Fetch { group, .. }
            | GroupLogRequest::Ops { group, .. } => *group,
        };
        let active = self
            .store
            .load_group(&group)?
            .is_some_and(|stored| stored.status == crate::api::GroupStatus::Active);
        if !active || self.group(&group)?.member_of_device(from.as_bytes()).is_none() {
            return Ok(refused);
        }
        self.group(&group)?.note_seen(from.as_bytes(), now_ms());
        match request {
            GroupLogRequest::Ops { checkpoint, skip, .. } => self.serve_ops(&group, checkpoint, skip),
            GroupLogRequest::Reconcile { since_ms, ranges, .. } => {
                let since = since_ms.max(now_ms().saturating_sub(LOG_SERVE_WINDOW_MS));
                let items = self.store.log_items(&group, since)?;
                let step = sync::respond(&items, &ranges);
                Ok(Reply::GroupLogRanges {
                    ranges: step.reply,
                    you_lack: step.they_lack,
                })
            }
            GroupLogRequest::Fetch { ids, .. } => {
                let mut units = Vec::new();
                let mut bytes = 0;
                for unit in self.store.log_get(&group, &ids[..ids.len().min(FETCH_BATCH)])? {
                    bytes += unit.len();
                    if bytes > MAX_FETCH_REPLY_BYTES {
                        break;
                    }
                    units.push(unit);
                }
                Ok(Reply::GroupLogItems(units))
            }
        }
    }

    /// One page of the ops after our checkpoint, or our checkpoint if the asker has another.
    fn serve_ops(&mut self, group: &GroupId, asked: Option<[u8; 32]>, skip: u32) -> Result<Reply> {
        let runtime = self.group(group)?;
        let ours = runtime.checkpoint.as_ref().map(|checkpoint| BlobRef {
            blob: checkpoint.hash(),
            len: crate::wire::encode(checkpoint).len() as u64,
        });
        if ours.map(|ours| ours.blob) != asked {
            return Ok(Reply::GroupLogOps {
                checkpoint: ours,
                ops: Vec::new(),
                more: true,
            });
        }
        let mut ops = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        for op in runtime.ops.iter().skip(skip as usize) {
            bytes += crate::wire::encode(op).len();
            if bytes > MAX_FETCH_REPLY_BYTES {
                more = true;
                break;
            }
            ops.push(op.clone());
        }
        Ok(Reply::GroupLogOps { checkpoint: ours, ops, more })
    }

    // ── blobs ───────────────────────────────────────────────────────────────

    /// Fetches `len` bytes at `offset` of a blob from member devices (`prefer`'s first).
    pub(in crate::node) fn fetch_blob(
        &mut self,
        group: &GroupId,
        hash: [u8; 32],
        offset: u64,
        len: u64,
        prefer: Option<crate::identity::UserKey>,
        purpose: BlobPurpose,
    ) -> Result<()> {
        let me = self.active_me()?;
        let transport = me.transport.clone();
        let own = *me.device_id.as_bytes();
        let runtime = self.group(group)?;
        let sources = runtime.pick_peers(&own, MAX_BLOB_SOURCES, prefer);
        if let Some(unit) = purpose.unit()
            && !runtime.fetching.insert(unit)
        {
            return Ok(());
        }
        let sources: Vec<EndpointId> = sources
            .iter()
            .filter_map(|device| device_endpoint(device).ok())
            .collect();
        let outcomes = self.outcomes_tx.clone();
        let group = *group;
        let chunk_bytes = self.group_timing.blob_chunk_bytes;
        n0_future::task::spawn(async move {
            let data = fetch_chunks(&transport, &sources, hash, offset, len, chunk_bytes).await;
            let _ = outcomes.send(Outcome::GroupBlob {
                group,
                hash,
                offset,
                data,
                purpose,
            });
        });
        Ok(())
    }

    pub(in crate::node) fn finish_blob(
        &mut self,
        group: GroupId,
        hash: [u8; 32],
        offset: u64,
        data: Option<Vec<u8>>,
        purpose: BlobPurpose,
    ) -> Result<()> {
        if let Some(unit) = purpose.unit()
            && let Some(runtime) = self.groups.get_mut(&group)
        {
            runtime.fetching.remove(&unit);
        }
        let Some(data) = data else {
            if let BlobPurpose::Replica { unit } = purpose
                && let Some(runtime) = self.groups.get_mut(&group)
            {
                // Nobody had all of it (yet): fall back to fetching only our slot.
                runtime.slot_only.insert(hash);
                return self.retry_unit(&group, &unit);
            }
            return Ok(());
        };
        match purpose {
            BlobPurpose::Avatar => {
                if *blake3::hash(&data).as_bytes() == hash {
                    self.store.put_blob(&hash, &data, now_ms())?;
                    self.store.set_avatar(&group, &data)?;
                    self.emit_avatar(&group, &data);
                }
                Ok(())
            }
            BlobPurpose::File { unit } => {
                if offset == 0 && *blake3::hash(&data).as_bytes() == hash {
                    self.store.put_blob(&hash, &data, now_ms())?;
                    self.retry_unit(&group, &unit)?;
                }
                Ok(())
            }
            BlobPurpose::Replica { unit } => {
                if offset == 0 && *blake3::hash(&data).as_bytes() == hash {
                    self.store.put_blob(&hash, &data, now_ms())?;
                } else if let Some(runtime) = self.groups.get_mut(&group) {
                    runtime.slot_only.insert(hash);
                }
                self.retry_unit(&group, &unit)
            }
            BlobPurpose::Slot { unit } => {
                // A slot cannot be checked against the blob hash; opening it authenticates it.
                self.group_slots.insert((group, hash), (offset, data));
                self.retry_unit(&group, &unit)
            }
        }
    }

    /// Serves part of a blob to a member device of one of our groups.
    pub(in crate::node) fn serve_blob(&mut self, from: EndpointId, hash: [u8; 32], offset: u64, len: u64) -> Result<Reply> {
        let known = self
            .groups
            .values()
            .any(|runtime| runtime.is_active() && runtime.member_of_device(from.as_bytes()).is_some());
        if !known {
            return Ok(Reply::Rejected(crate::wire::RejectReason::UnknownSender));
        }
        let now = now_ms();
        for runtime in self.groups.values_mut() {
            if runtime.member_of_device(from.as_bytes()).is_some() {
                runtime.note_seen(from.as_bytes(), now);
            }
        }
        let len = len.min(crate::wire::MAX_FILE_BYTES as u64 + 64);
        if let Some(bytes) = self.store.blob_range(&hash, offset, len)? {
            return Ok(Reply::Blob(Some(bytes)));
        }
        // The current checkpoint and photo outlive the blob table's retention.
        let checkpoint = self.groups.values().find_map(|runtime| {
            runtime
                .checkpoint
                .as_ref()
                .filter(|checkpoint| runtime.is_active() && checkpoint.hash() == hash)
        });
        if let Some(checkpoint) = checkpoint {
            let bytes = crate::wire::encode(checkpoint);
            let start = (offset as usize).min(bytes.len());
            let end = start.saturating_add(len as usize).min(bytes.len());
            return Ok(Reply::Blob(Some(bytes[start..end].to_vec())));
        }
        let photo_of = self
            .groups
            .iter()
            .find(|(_, runtime)| runtime.is_active() && runtime.state().avatar == Some(hash))
            .map(|(group, _)| *group);
        let photo = match photo_of {
            Some(group) => self.store.avatar(&group)?,
            None => None,
        };
        Ok(Reply::Blob(photo.filter(|_| offset == 0)))
    }
}

/// `len` bytes at `offset`, in `chunk_bytes` pieces, each from the first source that has it.
/// `None` if some piece came from nobody.
pub(super) async fn fetch_chunks(
    transport: &crate::transport::Transport,
    sources: &[EndpointId],
    hash: [u8; 32],
    offset: u64,
    len: u64,
    chunk_bytes: u64,
) -> Option<Vec<u8>> {
    let chunk_bytes = chunk_bytes.max(1);
    let mut data = Vec::with_capacity(len.min(chunk_bytes) as usize);
    while (data.len() as u64) < len {
        let at = offset + data.len() as u64;
        let want = (len - data.len() as u64).min(chunk_bytes);
        let mut piece = None;
        for source in sources {
            let frame = Frame::BlobGet { hash, offset: at, len: want };
            match transport.request(*source, &frame).await {
                Ok(Reply::Blob(Some(bytes))) if !bytes.is_empty() => {
                    piece = Some(bytes);
                    break;
                }
                Ok(_) => {}
                Err(err) => tracing::debug!(%source, ?err, "blob fetch failed"),
            }
        }
        let piece = piece?;
        let short = (piece.len() as u64) < want;
        data.extend_from_slice(&piece);
        if short {
            // The blob ends here (a caller asked for more than it holds).
            break;
        }
    }
    Some(data)
}

/// `preferred` first, then mostly `seen` (newest first) with one in [`EXPLORE_SHARE`] slots
/// from `unseen`; whichever runs short leaves its slots to the other.
fn choose_peers(preferred: Vec<[u8; 32]>, seen: &[[u8; 32]], unseen: &[[u8; 32]], count: usize) -> Vec<[u8; 32]> {
    let mut picked: Vec<[u8; 32]> = preferred.into_iter().take(count).collect();
    let room = count - picked.len();
    let explore = if unseen.is_empty() { 0 } else { room.div_ceil(EXPLORE_SHARE) };
    let recent = room - explore;
    picked.extend(seen.iter().take(recent));
    picked.extend(unseen.iter().take(count - picked.len()));
    picked.extend(seen.iter().skip(recent).take(count - picked.len()));
    picked
}

/// One peer: reconcile (a few short round trips), then fetch what we lack in batches.
async fn catch_up_with(
    transport: &crate::transport::Transport,
    peer: EndpointId,
    group: GroupId,
    since_ms: u64,
    items: &[Item],
    have: &mut std::collections::HashSet<ItemId>,
) -> Result<Vec<Vec<u8>>> {
    let mut lacking: Vec<ItemId> = Vec::new();
    let mut ranges = sync::initiate(items);
    for _ in 0..MAX_RECONCILE_STEPS {
        if ranges.is_empty() {
            break;
        }
        let request = Frame::GroupLog(GroupLogRequest::Reconcile { group, since_ms, ranges });
        let Reply::GroupLogRanges { ranges: reply, you_lack } = transport.request(peer, &request).await? else {
            anyhow::bail!("peer refused the catch-up");
        };
        lacking.extend(you_lack);
        if reply.is_empty() {
            break;
        }
        let step = sync::respond(items, &reply);
        lacking.extend(step.we_lack);
        ranges = step.reply;
    }
    lacking.retain(|id| have.insert(*id));
    let mut units = Vec::new();
    for batch in lacking.chunks(FETCH_BATCH) {
        let request = Frame::GroupLog(GroupLogRequest::Fetch { group, ids: batch.to_vec() });
        if let Reply::GroupLogItems(found) = transport.request(peer, &request).await? {
            units.extend(found);
        }
    }
    Ok(units)
}

/// Pending unit bookkeeping kept on the node: tag seed and tracked message per own unit.
pub(in crate::node) type UnitTags = HashMap<[u8; 32], ([u8; 32], Option<MessageId>)>;

#[cfg(test)]
mod tests {
    use super::*;

    fn devices(range: std::ops::Range<u8>) -> Vec<[u8; 32]> {
        range.map(|byte| [byte; 32]).collect()
    }

    #[test]
    fn recently_seen_peers_come_first_with_a_share_left_for_discovery() {
        let seen = devices(1..10);
        let unseen = devices(100..200);
        let picked = choose_peers(Vec::new(), &seen, &unseen, 16);
        assert_eq!(picked.len(), 16);
        assert_eq!(&picked[..9], &seen[..]);
        let catch_up = choose_peers(Vec::new(), &seen, &unseen, 3);
        assert_eq!(catch_up, vec![seen[0], seen[1], unseen[0]]);
    }

    #[test]
    fn a_short_side_leaves_its_slots_to_the_other() {
        let seen = devices(1..10);
        assert_eq!(choose_peers(Vec::new(), &seen, &[], 4), seen[..4].to_vec());
        let unseen = devices(100..103);
        assert_eq!(choose_peers(Vec::new(), &[], &unseen, 5), unseen);
        let preferred = devices(50..52);
        let picked = choose_peers(preferred.clone(), &seen, &unseen, 5);
        assert_eq!(&picked[..2], &preferred[..]);
        assert_eq!(picked.len(), 5);
    }
}
