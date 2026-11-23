//! Groups in the node actor (SPEC.md §14). The pure protocol lives in [`crate::group`]; these
//! modules keep one [`GroupRuntime`] per group we are in and wire it to storage and network:
//!
//! - [`ops`]: commands that change a group (signed ops) and applying ops from members
//! - [`join`]: invitations, the welcome for a newcomer, own-device sync
//! - [`keys`]: epochs, rekeys (batched), key requests and grants
//! - [`messages`]: sealing and opening group messages, applying their payloads
//! - [`net`]: gossip topics, member-served catch-up, blobs, Nostr fallback
//! - [`pending`]: a welcome or sync whose op log is fetched from members or Nostr chunks
//!
//! Group members who are not contacts never get an Olm session: everything for them travels
//! as device-signed group units ([`crate::group::message::GroupWire`]).

mod join;
mod keys;
mod messages;
mod net;
mod ops;
mod pending;

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use anyhow::{Context, Result};
use n0_future::task::JoinHandle;

use super::Node;
use crate::{
    api::{ConversationId, Event, GroupStatus, GroupView},
    clock::now_ms,
    group::{
        DeviceKey, DeviceKeys, GroupId, GroupOp,
        checkpoint::Checkpoint,
        epochs::Epoch,
        keys::{EpochId, REKEY_BATCH_WINDOW_MS, Secret, device_keypair, open_at_rest, seal_at_rest},
        oplog::{self, Folded},
    },
    identity::{DeviceList, UserKey},
    store::EpochRow,
    wire,
};

pub(super) use net::{BlobPurpose, GossipEvent, UnitTags};
pub(super) use pending::PendingFetches;

const AT_REST_CONTEXT: &str = "brumm 2026 group state at rest v1";
/// Retention of the group log for others, and of fetched blobs.
pub(super) const LOG_SERVE_WINDOW_MS: u64 = 14 * 24 * 60 * 60 * 1000;
/// Per-group storage cap of the served log (the newest entries stay).
pub(super) const GROUP_LOG_CAP_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_CATCH_UP_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DEFAULT_KEY_REQUEST_DELAY: Duration = Duration::from_secs(10);
/// Relays a group letter goes to besides our own.
const MAX_LETTER_RELAYS: usize = 4;

/// Bytes per `BlobGet`: a checkpoint of a big group (~14 MiB) is fetched in pieces, each under
/// the reply limit.
const DEFAULT_BLOB_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// Timing (and some sizes) of the group machinery; tests shorten the times.
#[derive(Debug, Clone, Copy)]
pub struct GroupTiming {
    /// How long the author of a kick/leave waits before rekeying, collecting more removals.
    pub rekey_delay: Duration,
    /// Catch-up with a few members' logs at least this often per group.
    pub catch_up_interval: Duration,
    /// How long a message of an unknown epoch waits before we ask members for the key.
    pub key_request_delay: Duration,
    /// Rekeys whose slots are at most this large carry them inline; larger ones are a blob that
    /// members fetch (and desktops replicate). Tests lower it to exercise the blob path.
    pub inline_rekey_bytes: usize,
    /// Ops between two checkpoints. Tests lower it to make checkpoints happen.
    pub checkpoint_ops: u64,
    /// Bytes per blob request; a bigger blob is fetched in pieces. Tests lower it.
    pub blob_chunk_bytes: u64,
    /// An encoded op log up to this size goes inside a welcome or sync; a larger one by
    /// pointer. Tests set 0 to force the pointer.
    pub inline_log_bytes: usize,
    /// A pointed-to op log up to this size is also posted as Nostr chunks. Tests set 0 to
    /// leave only the members.
    pub nostr_log_bytes: usize,
}

impl Default for GroupTiming {
    fn default() -> Self {
        Self {
            rekey_delay: Duration::from_millis(REKEY_BATCH_WINDOW_MS),
            catch_up_interval: DEFAULT_CATCH_UP_INTERVAL,
            key_request_delay: DEFAULT_KEY_REQUEST_DELAY,
            inline_rekey_bytes: crate::group::keys::INLINE_REKEY_BYTES,
            checkpoint_ops: crate::group::checkpoint::CHECKPOINT_OPS,
            blob_chunk_bytes: DEFAULT_BLOB_CHUNK_BYTES,
            inline_log_bytes: crate::group::join::INLINE_LOG_BYTES,
            nostr_log_bytes: crate::group::join::MAX_NOSTR_LOG_BYTES,
        }
    }
}

/// Newest log units read at load to know which member devices were online lately.
const SEEN_FROM_LOG_UNITS: u32 = 500;
/// Messages per sender device a receiver accepts per window (SPEC.md §14.7).
const RATE_LIMIT_MESSAGES: u32 = 60;
const RATE_LIMIT_WINDOW_MS: u64 = 10_000;

/// Everything the node keeps in memory for one group (loaded on use).
pub(super) struct GroupRuntime {
    folded: Folded,
    checkpoint: Option<Checkpoint>,
    /// Every stored op after the checkpoint.
    ops: Vec<GroupOp>,
    status: GroupStatus,
    /// Current device → user, from the members' newest device lists.
    devices: HashMap<DeviceKey, UserKey>,
    epochs: HashMap<EpochId, Epoch>,
    sending: Option<EpochId>,
    /// The task that joined the gossip topic and pumps its events to the node.
    pump: Option<JoinHandle<()>>,
    /// Set once the topic is joined.
    sender: Option<iroh_gossip::GossipSender>,
    neighbors: usize,
    /// Relays of other members' mailboxes, for group letters (most common first).
    letter_relays: Vec<String>,
    /// When this device rekeys unless someone else did (Unix ms).
    rekey_due_ms: Option<u64>,
    /// A device was revoked or a refresh was asked for: rekey even if an epoch is complete.
    heal: bool,
    /// When we last asked for an epoch.
    key_requests: HashMap<EpochId, u64>,
    /// Epochs messages were sealed with that we do not have, since when.
    unknown_since: HashMap<EpochId, u64>,
    /// Grants we will send unless another member is faster: due time per (device, epoch).
    pending_grants: HashMap<(DeviceKey, EpochId), u64>,
    rate: HashMap<DeviceKey, (u64, u32)>,
    /// When each member device last showed it is online (sent a unit, asked or answered us).
    /// In a big group most devices are offline; peers are picked from these first.
    seen: HashMap<DeviceKey, u64>,
    /// When we last asked gossip to join recently seen peers while we had no neighbour.
    last_join_peers_ms: u64,
    last_catch_up_ms: u64,
    catching_up: bool,
    /// Our own units that reached neither a gossip neighbour nor a relay yet.
    unpublished: Vec<[u8; 32]>,
    /// Log entries waiting for a blob (files) or a slot.
    fetching: HashSet<[u8; 32]>,
    /// Rekey blobs no member could hand over whole; only our slot is fetched for them.
    slot_only: HashSet<[u8; 32]>,
    /// When this device got the group; older units of unknown epochs predate us.
    joined_at_ms: u64,
    /// Op count at our last own checkpoint attempt: one that was not accepted is not rebuilt
    /// (megabytes, every tick) until more ops arrive.
    checkpoint_tried_at: u64,
}

impl GroupRuntime {
    fn state(&self) -> &oplog::GroupState {
        &self.folded.state
    }

    fn is_active(&self) -> bool {
        self.status == GroupStatus::Active
    }

    /// The user of a current member device.
    fn member_of_device(&self, device: &DeviceKey) -> Option<UserKey> {
        self.devices
            .get(device)
            .filter(|user| self.state().is_member(user))
            .copied()
    }

    fn view(&self, me: &UserKey) -> GroupView {
        let state = self.state();
        GroupView {
            id: state.group,
            name: state.name.clone(),
            status: self.status,
            my_role: if self.is_active() { state.role(me) } else { None },
            announcement: state.announcement,
            history_sharing: state.history_sharing,
            member_count: state.members.len() as u32,
            owner: state.owner,
        }
    }
}

/// Key for epoch secrets and sender chains at rest, derived like the Olm pickle key.
fn at_rest_key(device_secret: &[u8; 32]) -> Secret {
    blake3::derive_key(AT_REST_CONTEXT, device_secret)
}

impl Node {
    pub(super) fn device_secret(&self) -> Result<[u8; 32]> {
        Ok(self.me()?.device_secret)
    }

    /// This device's group key record.
    pub(super) fn own_device_keys(&self) -> Result<DeviceKeys> {
        let secret = self.device_secret()?;
        let (_, public) = device_keypair(&secret);
        // Derived, so the same record is produced every time (stable issue time).
        Ok(DeviceKeys::sign(&secret, public, 0))
    }

    /// Our own device keys plus those our other devices published.
    fn known_own_device_keys(&self) -> Result<Vec<DeviceKeys>> {
        let me = self.me()?;
        let mut keys = vec![self.own_device_keys()?];
        for known in self.store.device_keys_of(&me.user_key())? {
            if known.device_id != keys[0].device_id
                && me.profile.devices.entry(&device_endpoint(&known.device_id)?).is_some()
            {
                keys.push(known);
            }
        }
        Ok(keys)
    }

    /// The runtime of a group we know, loaded from storage on first use.
    pub(super) fn group(&mut self, group: &GroupId) -> Result<&mut GroupRuntime> {
        if !self.groups.contains_key(group) {
            let runtime = self.load_group_runtime(group)?.context("unknown group")?;
            self.groups.insert(*group, runtime);
        }
        Ok(self.groups.get_mut(group).expect("inserted above"))
    }

    fn load_group_runtime(&self, group: &GroupId) -> Result<Option<GroupRuntime>> {
        let Some(stored) = self.store.load_group(group)? else {
            return Ok(None);
        };
        let ops = self.store.group_ops(group)?;
        let folded = oplog::fold(*group, stored.checkpoint.as_ref(), &ops);
        let mut runtime = GroupRuntime::new(folded, ops, stored.status);
        runtime.checkpoint = stored.checkpoint;
        runtime.sending = stored.sending_epoch;
        runtime.joined_at_ms = stored.joined_at_ms;
        for (sent_at_ms, bytes) in self.store.log_recent(group, SEEN_FROM_LOG_UNITS)? {
            if let Ok(unit) = wire::decode::<crate::group::message::GroupWire>(&bytes) {
                runtime.note_seen(&unit.author_device(), sent_at_ms);
            }
        }
        let key = at_rest_key(&self.device_secret()?);
        for row in self.store.epochs(group)? {
            if let Some(epoch) = open_at_rest(&key, &row.sealed)
                .and_then(|plain| wire::decode::<Epoch>(&plain).ok())
            {
                runtime.epochs.insert(epoch.id, epoch);
            }
        }
        (runtime.devices, runtime.letter_relays) = self.member_devices(&runtime.folded.state)?;
        Ok(Some(runtime))
    }

    /// The newest device list we know for `user`: own profile, contact, group peer or the
    /// profile in the op log.
    pub(super) fn device_list_of(
        &self,
        user: &UserKey,
        state: &oplog::GroupState,
    ) -> Result<Option<DeviceList>> {
        let me = self.me()?;
        if *user == me.user_key() {
            return Ok(Some(me.profile.devices.clone()));
        }
        let mut best = state
            .members
            .get(user)
            .map(|member| member.profile.devices.clone());
        for candidate in [
            self.store.contact_device_list(user)?,
            self.store.peer_device_list(user)?,
        ]
        .into_iter()
        .flatten()
        {
            if best.as_ref().is_none_or(|known| known.seq < candidate.seq) {
                best = Some(candidate);
            }
        }
        Ok(best)
    }

    /// Member devices, and the relays their mailboxes use (most common first, capped).
    fn member_devices(
        &self,
        state: &oplog::GroupState,
    ) -> Result<(HashMap<DeviceKey, UserKey>, Vec<String>)> {
        let mut devices = HashMap::new();
        let mut relays: HashMap<String, usize> = HashMap::new();
        for user in state.members.keys() {
            if let Some(list) = self.device_list_of(user, state)? {
                for entry in &list.entries {
                    devices.insert(*entry.device_id().as_bytes(), *user);
                    for relay in &entry.mailbox.relays {
                        *relays.entry(relay.clone()).or_default() += 1;
                    }
                }
            }
        }
        let mut ranked: Vec<(String, usize)> = relays.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let relays = ranked
            .into_iter()
            .take(MAX_LETTER_RELAYS)
            .map(|(relay, _)| relay)
            .collect();
        Ok((devices, relays))
    }

    fn refresh_devices(&mut self, group: &GroupId) -> Result<()> {
        let state = self.group(group)?.folded.state.clone();
        let (devices, relays) = self.member_devices(&state)?;
        let runtime = self.group(group)?;
        runtime.devices = devices;
        runtime.letter_relays = relays;
        Ok(())
    }

    /// Persists the folded state and tells the UI.
    fn save_group_state(&mut self, group: &GroupId) -> Result<()> {
        let me = self.me()?.user_key();
        let runtime = self.group(group)?;
        let view = runtime.view(&me);
        let state = runtime.folded.state.clone();
        self.store.save_group(&state, &view, now_ms())?;
        self.store.sync_group_members(&state)?;
        self.events.emit(Event::GroupChanged(view));
        Ok(())
    }

    /// Stores an epoch (sealed at rest) and makes it usable.
    fn store_epoch(&mut self, group: &GroupId, epoch: Epoch) -> Result<bool> {
        let key = at_rest_key(&self.device_secret()?);
        let inserted = self.store.insert_epoch(
            group,
            &EpochRow {
                epoch: epoch.id,
                sealed: seal_at_rest(&key, &wire::encode(&epoch)),
                created_at_ms: epoch.created_at_ms,
                retired_at_ms: None,
            },
        )?;
        self.group(group)?.epochs.insert(epoch.id, epoch);
        Ok(inserted)
    }

    /// Ready, then every active group joins its topic and catches up (after `Ready`, which
    /// reads only the stored views).
    pub(super) fn start_groups(&mut self) -> Result<()> {
        if self.me.as_ref().is_none_or(|me| me.revoked) {
            return Ok(());
        }
        for group in self.store.active_group_ids()? {
            self.group(&group)?;
            self.update_sending_epoch(&group)?;
            self.join_topic(&group)?;
        }
        self.update_group_tags()
    }

    /// Periodic work: due rekeys, key requests and grants, catch-up, file retries.
    pub(super) fn group_tick(&mut self) -> Result<()> {
        if self.me.as_ref().is_none_or(|me| me.revoked) {
            return Ok(());
        }
        let groups: Vec<GroupId> = self
            .groups
            .iter()
            .filter(|(_, runtime)| runtime.is_active())
            .map(|(group, _)| *group)
            .collect();
        for group in groups {
            self.rekey_if_due(&group)?;
            self.checkpoint_if_due(&group)?;
            self.send_due_grants(&group)?;
            self.request_missing_keys(&group)?;
            self.catch_up_if_due(&group)?;
            self.join_seen_peers_if_alone(&group)?;
            self.retry_unprocessed(&group)?;
        }
        self.retry_pending_logs()
    }

    pub(super) fn conversation_of(group: &GroupId) -> ConversationId {
        ConversationId::Group(*group)
    }

    /// Stops everything for a group we left or were removed from: keys, topic, timers.
    fn deactivate_group(&mut self, group: &GroupId, status: GroupStatus) -> Result<()> {
        let runtime = self.group(group)?;
        runtime.status = status;
        runtime.epochs.clear();
        runtime.sending = None;
        runtime.rekey_due_ms = None;
        runtime.pending_grants.clear();
        runtime.sender = None;
        runtime.neighbors = 0;
        if let Some(pump) = runtime.pump.take() {
            pump.abort();
        }
        self.store.drop_group_keys(group)?;
        self.store.set_sending_epoch(group, None)?;
        self.save_group_state(group)?;
        self.update_group_tags()
    }
}

/// The iroh endpoint id of a device key (they are the same 32 bytes).
pub(super) fn device_endpoint(device: &DeviceKey) -> Result<iroh::EndpointId> {
    Ok(iroh::EndpointId::from_bytes(device)?)
}
