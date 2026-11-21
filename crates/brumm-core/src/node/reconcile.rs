//! Own-device consistency check, the node part: when to run it and applying its results.
//! The comparison itself is pure and lives in [`crate::reconcile`].
//!
//! A round with another own device starts when it is reachable (it acked something or sent us
//! something directly), at startup, and every `reconcile_interval` while it was seen recently.
//! It never starts while envelopes for that device are still queued (they are the expected
//! difference), and at most once per interval per device. The digest and the answer go out
//! directly, without the outbox, so an offline device does not pile them up; the missing
//! messages and deletions themselves go through the outbox like any other envelope.
//!
//! Deletions win: a message one device deleted is never re-inserted on it (tombstone in the
//! store). Offered such a message, the device tells the sender to delete it as well.

use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use iroh::EndpointId;
use n0_future::time::Instant;

use super::Node;
use crate::{
    clock::now_ms,
    identity::{DeviceEntry, UserKey},
    reconcile,
    wire::{HistoryItem, MessageId, Payload, SyncDigest, SyncHave, chunk_history},
};

/// A device seen this recently keeps getting periodic rounds. Bounds the ratchet steps spent
/// on digests to a device that went offline without us noticing.
const PRESENCE_ROUNDS: u32 = 6;
/// The compared window ends this far inside the retention period, so a message that one side
/// just pruned and the other not yet is never re-sent.
const RETENTION_EDGE_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Default)]
pub(super) struct ReconcileState {
    /// Last round started per own device (rate limit).
    last_round: HashMap<EndpointId, Instant>,
    /// Last direct sign of life per own device.
    last_seen: HashMap<EndpointId, Instant>,
}

impl Node {
    /// Our other device acked or sent us something directly.
    pub(super) fn own_device_reachable(&mut self, device: EndpointId) -> Result<()> {
        let Some(entry) = self.own_other_device(&device) else {
            return Ok(());
        };
        self.reconcile.last_seen.insert(device, Instant::now());
        self.start_round(&entry)
    }

    pub(super) fn own_device_unreachable(&mut self, device: &EndpointId) {
        self.reconcile.last_seen.remove(device);
    }

    /// Periodic tick: devices never tried since startup, and those seen recently.
    pub(super) fn reconcile_tick(&mut self) -> Result<()> {
        let presence = self.reconcile_interval * PRESENCE_ROUNDS;
        for entry in self.own_other_devices() {
            let device = entry.device_id();
            let untried = !self.reconcile.last_round.contains_key(&device);
            let recent = self
                .reconcile
                .last_seen
                .get(&device)
                .is_some_and(|seen| seen.elapsed() < presence);
            if untried || recent {
                self.start_round(&entry)?;
            }
        }
        Ok(())
    }

    fn start_round(&mut self, entry: &DeviceEntry) -> Result<()> {
        let device = entry.device_id();
        let rate_limited = self
            .reconcile
            .last_round
            .get(&device)
            .is_some_and(|last| last.elapsed() < self.reconcile_interval);
        if rate_limited || self.store.has_outbox_for(&device)? {
            return Ok(());
        }
        self.reconcile.last_round.insert(device, Instant::now());
        let since_ms = now_ms().saturating_sub(super::RETENTION_MS - RETENTION_EDGE_MS);
        let index = self.store.message_index(since_ms, &self.me()?.user_key())?;
        self.send_direct(
            entry,
            Payload::SyncDigest(reconcile::digest(since_ms, &index)),
        )
    }

    /// Step 2. Not while we still have envelopes queued for the asking device: they will arrive
    /// first, and its next round compares again.
    pub(super) fn receive_sync_digest(
        &mut self,
        from: EndpointId,
        digest: SyncDigest,
    ) -> Result<bool> {
        let Some(entry) = self.own_other_device(&from) else {
            return Ok(false);
        };
        if self.store.has_outbox_for(&from)? {
            return Ok(true);
        }
        let index = self
            .store
            .message_index(digest.since_ms, &self.me()?.user_key())?;
        if let Some(have) = reconcile::answer(&digest, &index) {
            self.send_direct(&entry, Payload::SyncHave(have))?;
        }
        Ok(true)
    }

    /// Step 3: send what it lacks; ask it to delete what we deleted.
    pub(super) fn receive_sync_have(&mut self, from: EndpointId, have: SyncHave) -> Result<bool> {
        let Some(entry) = self.own_other_device(&from) else {
            return Ok(false);
        };
        let index = self
            .store
            .message_index(have.since_ms, &self.me()?.user_key())?;
        let repair = reconcile::repair(&have, &index);
        let mut items = Vec::new();
        for (contact, ids) in &repair.send {
            items.extend(self.store.history_items(contact, ids)?);
        }
        for chunk in chunk_history(items) {
            self.enqueue_to_device(&entry, Payload::SyncMessages(chunk))?;
        }
        for (contact, ids) in &repair.they_have {
            let deleted = self.store.deleted_among(contact, ids)?;
            self.send_delete_sync(&entry, *contact, deleted)?;
        }
        self.flush_outbox()?;
        Ok(true)
    }

    /// Messages another own device found missing here. One we deleted is refused, and the
    /// sender is asked to delete it too.
    pub(super) fn receive_sync_messages(
        &mut self,
        from: EndpointId,
        items: Vec<HistoryItem>,
    ) -> Result<bool> {
        let Some(entry) = self.own_other_device(&from) else {
            return Ok(false);
        };
        let refused = self.insert_history(items)?;
        let mut deleted: HashMap<UserKey, Vec<MessageId>> = HashMap::new();
        for item in refused {
            deleted
                .entry(item.contact)
                .or_default()
                .push(item.message.id);
        }
        for (contact, ids) in deleted {
            self.send_delete_sync(&entry, contact, ids)?;
        }
        self.flush_outbox()?;
        Ok(true)
    }

    fn send_delete_sync(
        &mut self,
        entry: &DeviceEntry,
        contact: UserKey,
        ids: Vec<MessageId>,
    ) -> Result<()> {
        for ids in ids.chunks(crate::wire::MAX_DELETE_IDS) {
            self.enqueue_to_device(
                entry,
                Payload::DeleteSync {
                    contact,
                    ids: ids.to_vec(),
                },
            )?;
        }
        Ok(())
    }

    fn own_other_devices(&self) -> Vec<DeviceEntry> {
        let Some(me) = self.me.as_ref().filter(|me| !me.revoked) else {
            return Vec::new();
        };
        me.profile
            .devices
            .entries
            .iter()
            .filter(|entry| entry.device_id() != me.device_id)
            .cloned()
            .collect()
    }

    fn own_other_device(&self, device: &EndpointId) -> Option<DeviceEntry> {
        self.own_other_devices()
            .into_iter()
            .find(|entry| entry.device_id() == *device)
    }
}

/// Default for [`super::NodeConfig::reconcile_interval`].
pub(super) const DEFAULT_RECONCILE_INTERVAL: Duration = Duration::from_secs(5 * 60);
