//! Joining (SPEC.md §14.6) and own-device sync of groups. All of it travels over the 1:1 Olm
//! outbox: invitations and replies between contacts, the welcome from the admin who added us,
//! and [`GroupSync`] between our own devices (also for a freshly linked device).

use anyhow::{Context, Result};

use super::{GroupRuntime, ops::OpSource, pending::PendingLog};
use crate::{
    api::{ConversationId, DeliveryStatus, Direction, Event, GroupStatus},
    clock::now_ms,
    group::{
        GroupId, OpKind, sign_consent,
        epochs::Epoch,
        join::{
            Acceptance, GroupInvitation, GroupSync, GroupWelcome, InvitationReply, LogSource,
            MAX_SHARED_HISTORY_BYTES, MAX_SHARED_MESSAGES, OpLog, SHARED_HISTORY_DAYS, SharedMessage,
        },
        oplog,
    },
    identity::{DeviceEntry, UserKey},
    node::Node,
    wire::{self, ChatMessage, Payload},
};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

impl Node {
    /// Owner/admin: invites contacts; each joins once it accepts.
    pub(in crate::node) fn invite_to_group(&mut self, group: GroupId, users: Vec<UserKey>) -> Result<()> {
        self.require_admin(&group)?;
        let runtime = self.group(&group)?;
        let invitation = GroupInvitation {
            group,
            name: runtime.state().name.clone(),
            member_count: runtime.state().members.len() as u32,
            at_ms: now_ms(),
        };
        let members: Vec<UserKey> = runtime.state().members.keys().copied().collect();
        for user in users {
            if members.contains(&user) {
                continue;
            }
            self.store.contact(&user)?.context("you can only invite your contacts")?;
            self.store.note_invite_sent(&group, &user, now_ms())?;
            self.enqueue_to_user(&user, rand::random(), Payload::GroupInvitation(invitation.clone()), None)?;
        }
        self.flush_outbox()
    }

    /// From a contact. Ignored if we are already in the group.
    pub(in crate::node) fn receive_group_invitation(
        &mut self,
        inviter: UserKey,
        invitation: GroupInvitation,
    ) -> Result<bool> {
        if !invitation.is_valid() || self.store.contact(&inviter)?.is_none() {
            return Ok(false);
        }
        let active = self
            .store
            .load_group(&invitation.group)?
            .is_some_and(|stored| stored.status == GroupStatus::Active);
        if !active {
            self.store
                .save_invitation(&invitation.group, &inviter, &wire::encode(&invitation), now_ms())?;
            self.emit_invitations()?;
        }
        Ok(true)
    }

    pub(in crate::node) fn emit_invitations(&self) -> Result<()> {
        self.events.emit(Event::GroupInvitations(self.store.invitations()?));
        Ok(())
    }

    pub(in crate::node) fn answer_group_invitation(&mut self, group: GroupId, accept: bool) -> Result<()> {
        let me_key = self.active_me()?.user_key();
        // An accepted invitation stays listed ("waiting to be added") until the welcome is in.
        let inviter = if accept {
            self.store.set_invitation_accepted(&group)?
        } else {
            self.store.delete_pending_log(&group)?;
            self.store.take_invitation(&group)?.map(|(inviter, _)| inviter)
        }
        .context("no such invitation")?;
        let accept = if accept {
            let rejoin = self
                .store
                .load_group(&group)?
                .and_then(|stored| stored.state.departures.get(&me_key).copied())
                .unwrap_or(0);
            let device_keys = self.known_own_device_keys()?;
            let me = self.active_me()?;
            Some(Acceptance {
                profile: me.profile.clone(),
                device_keys,
                rejoin,
                consent: sign_consent(&me.user, &group, rejoin),
            })
        } else {
            None
        };
        let reply = InvitationReply { group, accept };
        self.enqueue_to_user(&inviter, rand::random(), Payload::GroupInvitationReply(Box::new(reply)), None)?;
        self.emit_invitations()?;
        self.flush_outbox()
    }

    /// The invitee answered: on accept, sign the `Add` and welcome them.
    pub(in crate::node) fn receive_invitation_reply(&mut self, from: UserKey, reply: InvitationReply) -> Result<bool> {
        let group = reply.group;
        if !self.store.take_invite_sent(&group, &from)? {
            return Ok(true);
        }
        let Some(acceptance) = reply.accept else {
            return Ok(true);
        };
        if acceptance.profile.user_key() != from || !acceptance.profile.devices.verify() {
            return Ok(false);
        }
        let entries = &acceptance.profile.devices.entries;
        let newcomer_devices: Vec<[u8; 32]> = entries.iter().map(|entry| *entry.device_id().as_bytes()).collect();
        let newcomer_relays = relays_of(entries);
        let kind = OpKind::Add {
            member: acceptance.profile,
            device_keys: acceptance.device_keys,
            rejoin: acceptance.rejoin,
            consent: acceptance.consent,
        };
        let op = match self.change_group(group, kind) {
            Ok(op) => op,
            Err(err) => {
                tracing::info!(?err, "could not add the invitee");
                return Ok(true);
            }
        };
        let runtime = self.group(&group)?;
        let epoch = runtime
            .epochs
            .values()
            .find(|epoch| epoch.heads.contains(&op.hash()) && epoch.parent.is_some())
            .cloned()
            .context("no epoch for the newcomer")?;
        let history = if runtime.state().history_sharing {
            self.shared_history(&group)?
        } else {
            Vec::new()
        };
        let welcome = GroupWelcome {
            group,
            log: self.log_source(&group, &newcomer_devices, newcomer_relays)?,
            epoch,
            history,
        };
        self.enqueue_to_user(&from, rand::random(), Payload::GroupWelcome(Box::new(welcome)), None)?;
        self.flush_outbox()?;
        Ok(true)
    }

    /// The last days of the group, newest first, within the welcome's budget.
    fn shared_history(&mut self, group: &GroupId) -> Result<Vec<SharedMessage>> {
        let conversation = ConversationId::Group(*group);
        let since = now_ms().saturating_sub(SHARED_HISTORY_DAYS * DAY_MS);
        let page = self
            .store
            .conversation_page(&conversation, None, Some(MAX_SHARED_MESSAGES as u32))?;
        let me = self.me()?.user_key();
        let mut shared = Vec::new();
        let mut bytes = 0;
        for view in page.messages.iter().rev() {
            if view.system || view.sent_at_ms < since {
                continue;
            }
            let sender = match (&view.sender, view.direction) {
                (Some(sender), _) => sender.user_key,
                (None, Direction::Outgoing) => me,
                (None, Direction::Incoming) => continue,
            };
            let Some(body) = self.store.message_body(&conversation, &view.id)? else {
                continue;
            };
            let message = SharedMessage {
                id: view.id,
                sender,
                sent_at_ms: view.sent_at_ms,
                body,
            };
            let size = wire::encode(&message).len();
            if bytes + size > MAX_SHARED_HISTORY_BYTES {
                // Too big for what is left (a file): older short messages may still fit.
                continue;
            }
            bytes += size;
            shared.push(message);
        }
        Ok(shared)
    }

    /// From the admin who added us.
    pub(in crate::node) fn receive_group_welcome(&mut self, from: UserKey, welcome: GroupWelcome) -> Result<bool> {
        let me = self.me()?.user_key();
        let group = welcome.group;
        let log = match welcome.log {
            LogSource::Inline(log) => *log,
            LogSource::Pointer(_) => {
                let active = self
                    .store
                    .load_group(&group)?
                    .is_some_and(|stored| stored.status == GroupStatus::Active);
                if !active {
                    self.wait_for_log(PendingLog::Welcome { from, welcome })?;
                }
                return Ok(true);
            }
        };
        let folded = oplog::fold(group, log.checkpoint.as_ref(), &log.ops);
        if !folded.state.is_admin(&from) || !folded.state.is_member(&me) {
            return Ok(false);
        }
        // The inviter just reached us: its devices are the first peers to try.
        if let Some(inviter) = folded.state.members.get(&from) {
            let devices = inviter.profile.devices.entries.iter().map(|entry| *entry.device_id().as_bytes());
            self.pending_fetches.hint_seen(group, devices);
        }
        let already = self
            .store
            .load_group(&group)?
            .is_some_and(|stored| stored.status == GroupStatus::Active);
        self.install_group(group, log, vec![welcome.epoch], GroupStatus::Active)?;
        if already {
            return Ok(true);
        }
        let admin = folded
            .state
            .members
            .get(&from)
            .map(|member| member.profile.display_name.clone())
            .unwrap_or_default();
        self.system_line(&group, None, "You joined the group".to_owned(), now_ms())?;
        if !welcome.history.is_empty() {
            self.system_line(&group, None, format!("History shared by {admin}"), now_ms())?;
            self.insert_shared_history(&group, welcome.history)?;
        }
        self.sync_group_to_own_devices(&group)?;
        self.flush_outbox()?;
        Ok(true)
    }

    fn insert_shared_history(&mut self, group: &GroupId, history: Vec<SharedMessage>) -> Result<()> {
        let conversation = ConversationId::Group(*group);
        let me = self.me()?.user_key();
        for shared in history {
            let message = ChatMessage {
                id: shared.id,
                sent_at_ms: shared.sent_at_ms,
                body: shared.body,
            };
            if !message.body.is_valid() {
                continue;
            }
            let direction = if shared.sender == me { Direction::Outgoing } else { Direction::Incoming };
            self.store
                .insert_group_message(&conversation, &message, direction, DeliveryStatus::Delivered, &shared.sender)?;
        }
        self.events.emit(Event::ConversationsChanged(vec![conversation]));
        self.emit_preview(&conversation)
    }

    /// Stores a group's log and epochs (welcome or own-device sync) and joins it if active.
    fn install_group(&mut self, group: GroupId, log: OpLog, epochs: Vec<Epoch>, status: GroupStatus) -> Result<()> {
        // Gone before the group shows up, so the list never has both.
        let invited = self.store.take_invitation(&group)?.is_some();
        if !self.groups.contains_key(&group) && self.store.load_group(&group)?.is_none() {
            let mut runtime = GroupRuntime::new(oplog::fold(group, log.checkpoint.as_ref(), &[]), Vec::new(), status);
            runtime.checkpoint = log.checkpoint.clone();
            if let Some(checkpoint) = &log.checkpoint {
                self.store.set_group_checkpoint(&group, checkpoint)?;
            }
            self.groups.insert(group, runtime);
            let state = self.group(&group)?.state().clone();
            let me = self.me()?.user_key();
            let view = self.group(&group)?.view(&me);
            self.store.save_group(&state, &view, now_ms())?;
            if let Some(checkpoint) = &log.checkpoint {
                self.store.set_group_checkpoint(&group, checkpoint)?;
            }
        }
        self.apply_ops(&group, log.ops, OpSource::Bulk)?;
        let was_active = self.group(&group)?.is_active();
        if status == GroupStatus::Active {
            self.group(&group)?.status = GroupStatus::Active;
            for epoch in epochs {
                self.store_epoch(&group, epoch)?;
            }
            self.refresh_devices(&group)?;
            let hints = self.pending_fetches.take_seen_hints(&group);
            let now = now_ms();
            let runtime = self.group(&group)?;
            for device in hints {
                runtime.note_seen(&device, now);
            }
            self.save_group_state(&group)?;
            self.update_sending_epoch(&group)?;
            self.join_topic(&group)?;
            self.ensure_group_photo(&group)?;
            self.update_group_tags()?;
            self.retry_unprocessed(&group)?;
        } else if was_active {
            self.deactivate_group(&group, status)?;
        }
        if invited {
            self.emit_invitations()?;
        }
        Ok(())
    }

    /// Hands a group (log, every epoch we hold, our status) to our other devices.
    pub(in crate::node) fn sync_group_to_own_devices(&mut self, group: &GroupId) -> Result<()> {
        let sync = self.group_sync(group)?;
        self.enqueue_to_own_devices(Payload::GroupSync(Box::new(sync)))
    }

    fn group_sync(&mut self, group: &GroupId) -> Result<GroupSync> {
        let relays = relays_of(&self.me()?.profile.devices.entries);
        let log = self.log_source(group, &[], relays)?;
        let runtime = self.group(group)?;
        Ok(GroupSync {
            group: *group,
            log,
            epochs: runtime.epochs.values().cloned().collect(),
            active: runtime.is_active(),
        })
    }

    /// Every group to a freshly linked device.
    pub(in crate::node) fn sync_groups_to_device(&mut self, entry: &DeviceEntry) -> Result<()> {
        for group in self.store.group_ids()? {
            let sync = self.group_sync(&group)?;
            self.enqueue_to_device(entry, Payload::GroupSync(Box::new(sync)))?;
        }
        Ok(())
    }

    /// From one of our own devices.
    pub(in crate::node) fn receive_group_sync(&mut self, sync: GroupSync) -> Result<bool> {
        let me = self.me()?.user_key();
        let log = match sync.log {
            LogSource::Inline(log) => *log,
            LogSource::Pointer(_) => {
                self.wait_for_log(PendingLog::Sync(sync))?;
                return Ok(true);
            }
        };
        // A sync comes from one of our own devices: those are the first peers to try.
        let own_devices: Vec<[u8; 32]> =
            self.me()?.profile.devices.entries.iter().map(|entry| *entry.device_id().as_bytes()).collect();
        self.pending_fetches.hint_seen(sync.group, own_devices);
        let folded = oplog::fold(sync.group, log.checkpoint.as_ref(), &log.ops);
        if !folded.state.created && log.checkpoint.is_none() {
            return Ok(false);
        }
        let status = match (sync.active, folded.state.is_member(&me)) {
            (true, true) => GroupStatus::Active,
            (_, _) if folded.state.departures.contains_key(&me) => GroupStatus::Left,
            _ => GroupStatus::Removed,
        };
        let new = self
            .store
            .load_group(&sync.group)?
            .is_none_or(|stored| stored.status != GroupStatus::Active);
        self.install_group(sync.group, log, sync.epochs, status)?;
        if status == GroupStatus::Active
            && new
            && let Err(err) = self.announce_device_keys(&sync.group)
        {
            tracing::info!(?err, "device keys not announced yet");
        }
        Ok(true)
    }
}

/// Every mailbox relay of these devices, once each.
fn relays_of(entries: &[DeviceEntry]) -> Vec<String> {
    let mut relays: Vec<String> = Vec::new();
    for relay in entries.iter().flat_map(|entry| &entry.mailbox.relays) {
        if !relays.contains(relay) {
            relays.push(relay.clone());
        }
    }
    relays
}
