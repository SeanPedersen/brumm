//! Group epochs in the node (SPEC.md §14.3): choosing the sending epoch, rekeying after
//! removals (batched: the author waits [`super::GroupTiming::rekey_delay`], others longer and
//! jittered, so normally one device rekeys), opening rekeys, and recovering a missed epoch with
//! a key request that any member answers.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};

use super::{GroupRuntime, net::BlobPurpose};
use crate::{
    clock::now_ms,
    group::{
        DeviceKey, DeviceKeys, GroupId, OpHash,
        epochs::{self, Epoch},
        keys::{self, EpochId, MIN_REKEY_INTERVAL_MS, Rekey, SLOT_BYTES, open_slot, random_secret, slot_range},
        message::{GroupWire, KeyGrant, KeyRequest},
        oplog::{self, Change, GroupState},
    },
    identity::UserKey,
    node::Node,
};

/// Spread of the moment members answer a key request, so usually only one does.
const GRANT_JITTER_MS: u64 = 1500;
/// How often we ask again for the same missing epoch.
const KEY_REQUEST_REPEAT_MS: u64 = 60_000;

/// Who a rekey is for (see [`keys::recipients`]).
struct RekeyRecipients {
    /// The op heads the new epoch covers.
    membership: Vec<OpHash>,
    excluded: Vec<DeviceKey>,
    extra: Vec<DeviceKeys>,
    /// Device keys from the op state.
    op_keys: Vec<DeviceKeys>,
}

impl GroupRuntime {
    fn complete_epochs(&self) -> Vec<&Epoch> {
        let membership = epochs::membership_ops(&self.folded.order);
        self.epochs
            .values()
            .filter(|epoch| {
                epochs::is_complete(&epoch.heads, &membership, &self.ops, self.checkpoint.as_ref())
            })
            .collect()
    }

    /// The state at `heads`, if we know every op it needs.
    fn state_at(&self, heads: &[OpHash]) -> Option<GroupState> {
        let mut sorted = heads.to_vec();
        sorted.sort_unstable();
        if sorted == self.state().heads {
            return Some(self.state().clone());
        }
        let ancestry = oplog::ancestry(heads, &self.ops);
        let known: HashSet<OpHash> = self.ops.iter().map(|op| op.hash()).collect();
        let reachable = ancestry.iter().all(|hash| {
            known.contains(hash)
                || self
                    .checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.covers(hash))
        });
        if !reachable {
            return None;
        }
        let ops: Vec<_> = self
            .ops
            .iter()
            .filter(|op| ancestry.contains(&op.hash()))
            .cloned()
            .collect();
        Some(oplog::fold(self.state().group, self.checkpoint.as_ref(), &ops).state)
    }

    /// Whether `user` may read `epoch`: it was made after the user's latest `Add`.
    fn entitled(&self, user: &UserKey, epoch: &Epoch) -> bool {
        let joined = self.folded.order.iter().rev().find(|folded| match &folded.change {
            Some(Change::Added(added)) => added == user,
            Some(Change::Created) => folded.author == *user,
            _ => false,
        });
        let ancestry = oplog::ancestry(&epoch.heads, &self.ops);
        match joined {
            Some(folded) => ancestry.contains(&folded.hash),
            // Joined before the checkpoint: any epoch past it.
            None => self.state().is_member(user)
                && self
                    .checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.heads.iter().all(|head| ancestry.contains(head))),
        }
    }
}

/// Always-on devices (not browsers) fetch whole rekey blobs and serve slots (SPEC §14.4).
const REPLICATES_REKEY_BLOBS: bool = !cfg!(target_family = "wasm");

impl Node {
    /// Picks the epoch to send with (lowest id among the complete ones) and retires the rest.
    /// With none complete, a rekey is due.
    pub(in crate::node) fn update_sending_epoch(&mut self, group: &GroupId) -> Result<()> {
        let me = self.me()?.user_key();
        let runtime = self.group(group)?;
        if !runtime.is_active() {
            return Ok(());
        }
        let chosen = epochs::choose(runtime.complete_epochs()).map(|epoch| epoch.id);
        let Some(chosen) = chosen else {
            let mine = runtime
                .folded
                .order
                .iter()
                .rev()
                .find(|folded| folded.change.as_ref().is_some_and(Change::needs_rekey))
                .is_some_and(|folded| folded.author == me);
            self.schedule_rekey(group, mine);
            return Ok(());
        };
        let runtime = self.group(group)?;
        if !runtime.heal {
            runtime.rekey_due_ms = None;
        }
        if runtime.sending == Some(chosen) {
            return Ok(());
        }
        runtime.sending = Some(chosen);
        self.store.set_sending_epoch(group, Some(&chosen))?;
        self.store.retire_epochs_except(group, &chosen, now_ms())?;
        self.update_group_tags()
    }

    /// The epoch to seal with: the chosen one, else (during a pending rekey) the newest we have.
    pub(super) fn sending_epoch(&mut self, group: &GroupId) -> Result<Epoch> {
        let runtime = self.group(group)?;
        if let Some(epoch) = runtime.sending.and_then(|id| runtime.epochs.get(&id)) {
            return Ok(epoch.clone());
        }
        runtime
            .epochs
            .values()
            .max_by_key(|epoch| (epoch.created_at_ms, epoch.id))
            .cloned()
            .context("no key for this group yet; waiting for a member")
    }

    /// Rekeys after the batch window: soon if this user caused the removal, later (jittered)
    /// otherwise, so the author normally does it and others only step in when it does not.
    pub(super) fn schedule_rekey(&mut self, group: &GroupId, mine: bool) {
        let delay = self.group_timing.rekey_delay.as_millis() as u64;
        let wait = if mine {
            delay
        } else {
            2 * delay + rand::random::<u64>() % delay.max(1)
        };
        let due = now_ms() + wait;
        if let Some(runtime) = self.groups.get_mut(group)
            && runtime.is_active()
            && runtime.rekey_due_ms.is_none_or(|known| known > due)
        {
            runtime.rekey_due_ms = Some(due);
        }
    }

    /// Manual heal (rate limited per device).
    pub(in crate::node) fn refresh_group_keys(&mut self, group: &GroupId) -> Result<()> {
        let last = self.store.load_group(group)?.context("unknown group")?.last_rekey_ms;
        if now_ms().saturating_sub(last) < MIN_REKEY_INTERVAL_MS {
            bail!("the group key was replaced moments ago");
        }
        self.rekey(group)
    }

    pub(super) fn rekey_if_due(&mut self, group: &GroupId) -> Result<()> {
        let runtime = self.group(group)?;
        if runtime.rekey_due_ms.is_none_or(|due| due > now_ms()) {
            return Ok(());
        }
        runtime.rekey_due_ms = None;
        let needed = runtime.heal || runtime.complete_epochs().is_empty();
        if needed { self.rekey(group) } else { Ok(()) }
    }

    /// A new epoch for the current members' devices.
    fn rekey(&mut self, group: &GroupId) -> Result<()> {
        let device_secret = self.device_secret()?;
        let parent = self.sending_epoch(group).ok();
        let RekeyRecipients {
            membership,
            excluded,
            extra,
            op_keys,
        } = self.rekey_recipients(group)?;
        let secret = random_secret();
        let now = now_ms();
        let recipients = keys::recipients(&op_keys, &excluded, &extra);
        let (mut rekey, slots) = Rekey::new(
            &device_secret,
            *group,
            &secret,
            parent.as_ref().map(|epoch| epoch.id),
            membership.clone(),
            excluded,
            extra,
            &recipients,
            now,
        );
        if slots.len() > self.group_timing.inline_rekey_bytes {
            rekey.inline = None;
        }
        if rekey.inline.is_none() {
            self.store.put_blob(&rekey.blob, &slots, now)?;
        }
        self.store.insert_rekey(&rekey)?;
        self.store.set_rekey_opened(group, &rekey.epoch)?;
        self.store.set_last_rekey(group, now)?;
        tracing::info!(devices = recipients.len(), "group rekeyed");
        let tag = parent.map_or(*group, |epoch| epoch.secret);
        self.publish_unit(group, GroupWire::Rekey(rekey), tag, None)?;
        let runtime = self.group(group)?;
        runtime.heal = false;
        self.store_epoch(group, Epoch::root(secret, membership, now))?;
        self.update_sending_epoch(group)?;
        self.update_group_tags()
    }

    /// Recipients of a rekey now: the op state's device keys minus devices no longer in their
    /// user's list, plus keys we learned since.
    fn rekey_recipients(&mut self, group: &GroupId) -> Result<RekeyRecipients> {
        let state = self.group(group)?.state().clone();
        let own = self.own_device_keys()?;
        let mut op_keys = Vec::new();
        let mut excluded = Vec::new();
        let mut extra = Vec::new();
        for (user, member) in &state.members {
            let current: HashSet<DeviceKey> = self
                .device_list_of(user, &state)?
                .map(|list| list.entries.iter().map(|entry| *entry.device_id().as_bytes()).collect())
                .unwrap_or_default();
            let in_op: HashSet<DeviceKey> = member.device_keys.iter().map(|keys| keys.device_id).collect();
            for keys in &member.device_keys {
                op_keys.push(keys.clone());
                if !current.contains(&keys.device_id) {
                    excluded.push(keys.device_id);
                }
            }
            let mut learned = self.store.device_keys_of(user)?;
            if *user == self.me()?.user_key() {
                learned.push(own.clone());
            }
            for keys in learned {
                if current.contains(&keys.device_id)
                    && !in_op.contains(&keys.device_id)
                    && !extra.iter().any(|known: &DeviceKeys| known.device_id == keys.device_id)
                {
                    extra.push(keys);
                }
            }
        }
        Ok(RekeyRecipients {
            membership: state.heads.clone(),
            excluded,
            extra,
            op_keys,
        })
    }

    /// Stores a rekey and opens our slot (fetching it when the blob is not inline).
    /// `Ok(false)`: not usable yet (ops or the slot still missing).
    pub(super) fn receive_rekey(&mut self, group: &GroupId, rekey: &Rekey, unit: &[u8; 32]) -> Result<bool> {
        self.store.insert_rekey(rekey)?;
        let device_secret = self.device_secret()?;
        let own: DeviceKey = *self.me()?.device_id.as_bytes();
        let runtime = self.group(group)?;
        if runtime.epochs.contains_key(&rekey.epoch) {
            return Ok(true);
        }
        let Some(state) = runtime.state_at(&rekey.membership) else {
            return Ok(false);
        };
        let op_keys: Vec<DeviceKeys> = state
            .members
            .values()
            .flat_map(|member| member.device_keys.iter().cloned())
            .collect();
        let recipients = rekey.recipients(&op_keys);
        let Some(range) = slot_range(&recipients, &own) else {
            tracing::info!("not among a rekey's recipients; will ask for the key");
            return Ok(true);
        };
        let fetched = self
            .group_slots
            .get(&(*group, rekey.blob))
            .filter(|(offset, _)| *offset == range.start as u64)
            .map(|(_, slot)| slot.clone());
        let slot = match &rekey.inline {
            Some(slots) => slots.get(range.clone()).map(<[u8]>::to_vec),
            None => match fetched {
                Some(slot) => Some(slot),
                None => self
                    .store
                    .blob_range(&rekey.blob, range.start as u64, SLOT_BYTES as u64)?,
            },
        };
        let Some(slot) = slot.filter(|slot| slot.len() == SLOT_BYTES) else {
            // Desktops keep the whole blob and serve slots from it, so 30k slot fetches spread
            // over the group instead of all hitting the device that rekeyed.
            let replicate = REPLICATES_REKEY_BLOBS && !self.group(group)?.slot_only.contains(&rekey.blob);
            let (offset, len, purpose) = if replicate {
                (0, rekey.blob_len, BlobPurpose::Replica { unit: *unit })
            } else {
                (range.start as u64, SLOT_BYTES as u64, BlobPurpose::Slot { unit: *unit })
            };
            self.fetch_blob(group, rekey.blob, offset, len, None, purpose)?;
            return Ok(false);
        };
        let Some(secret) = open_slot(group, &rekey.epoch, &slot, &own, &device_secret) else {
            tracing::warn!("our rekey slot does not open");
            return Ok(true);
        };
        self.group_slots.remove(&(*group, rekey.blob));
        self.store.set_rekey_opened(group, &rekey.epoch)?;
        self.install_epoch(group, Epoch::root(secret, rekey.membership.clone(), now_ms()))?;
        Ok(true)
    }

    /// A new usable epoch: store it, maybe switch to it, and retry what waited for it.
    pub(super) fn install_epoch(&mut self, group: &GroupId, epoch: Epoch) -> Result<()> {
        let id = epoch.id;
        if !self.store_epoch(group, epoch)? {
            return Ok(());
        }
        let runtime = self.group(group)?;
        runtime.unknown_since.remove(&id);
        runtime.heal = false;
        self.update_sending_epoch(group)?;
        self.update_group_tags()?;
        self.retry_unprocessed(group)
    }

    /// Asks members for epochs our messages need and nobody delivered.
    pub(super) fn request_missing_keys(&mut self, group: &GroupId) -> Result<()> {
        let device_secret = self.device_secret()?;
        let delay = self.group_timing.key_request_delay.as_millis() as u64;
        let now = now_ms();
        let runtime = self.group(group)?;
        let due: Vec<EpochId> = runtime
            .unknown_since
            .iter()
            .filter(|(epoch, since)| {
                now.saturating_sub(**since) >= delay
                    && !runtime.epochs.contains_key(*epoch)
                    && runtime
                        .key_requests
                        .get(*epoch)
                        .is_none_or(|last| now.saturating_sub(*last) >= KEY_REQUEST_REPEAT_MS)
            })
            .map(|(epoch, _)| *epoch)
            .collect();
        for epoch in due {
            self.group(group)?.key_requests.insert(epoch, now);
            let request = KeyRequest::new(&device_secret, *group, epoch, now);
            tracing::info!("asking group members for a missing epoch");
            self.publish_unit(group, GroupWire::KeyRequest(request), *group, None)?;
        }
        Ok(())
    }

    /// Schedules a grant if we hold the epoch and the asking member may read it.
    pub(super) fn receive_key_request(
        &mut self,
        group: &GroupId,
        request: &KeyRequest,
        user: &UserKey,
    ) -> Result<()> {
        let own: DeviceKey = *self.me()?.device_id.as_bytes();
        if request.device == own {
            return Ok(());
        }
        let keys = match self.store.device_keys(&request.device)? {
            Some(keys) => Some(keys),
            None => self.group(group)?.state().members.get(user).and_then(|member| {
                member
                    .device_keys
                    .iter()
                    .find(|keys| keys.device_id == request.device)
                    .cloned()
            }),
        };
        let runtime = self.group(group)?;
        let Some(epoch) = runtime.epochs.get(&request.epoch) else {
            return Ok(());
        };
        if keys.is_none() || !runtime.entitled(user, epoch) {
            return Ok(());
        }
        let due = now_ms() + rand::random::<u64>() % GRANT_JITTER_MS;
        runtime
            .pending_grants
            .entry((request.device, request.epoch))
            .or_insert(due);
        Ok(())
    }

    pub(super) fn send_due_grants(&mut self, group: &GroupId) -> Result<()> {
        let device_secret = self.device_secret()?;
        let now = now_ms();
        let runtime = self.group(group)?;
        let due: Vec<(DeviceKey, EpochId)> = runtime
            .pending_grants
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(key, _)| *key)
            .collect();
        for (device, epoch_id) in due {
            let runtime = self.group(group)?;
            runtime.pending_grants.remove(&(device, epoch_id));
            let Some(epoch) = runtime.epochs.get(&epoch_id).cloned() else {
                continue;
            };
            let user = runtime.member_of_device(&device);
            let keys = match self.store.device_keys(&device)? {
                Some(keys) => Some(keys),
                None => user.and_then(|user| {
                    self.groups.get(group).and_then(|runtime| {
                        runtime.state().members.get(&user).and_then(|member| {
                            member.device_keys.iter().find(|keys| keys.device_id == device).cloned()
                        })
                    })
                }),
            };
            let Some(keys) = keys else { continue };
            let grant = KeyGrant::new(&device_secret, *group, &epoch, &keys, now);
            self.publish_unit(group, GroupWire::KeyGrant(grant), *group, None)?;
        }
        Ok(())
    }

    /// Someone answered a key request: ours (open it), or another's (no need to answer it too).
    pub(super) fn receive_key_grant(&mut self, group: &GroupId, grant: &KeyGrant) -> Result<()> {
        let device_secret = self.device_secret()?;
        let own: DeviceKey = *self.me()?.device_id.as_bytes();
        self.group(group)?
            .pending_grants
            .remove(&(grant.to_device, grant.epoch));
        if let Some(epoch) = grant.open(&own, &device_secret, now_ms()) {
            self.install_epoch(group, epoch)?;
        }
        Ok(())
    }
}
