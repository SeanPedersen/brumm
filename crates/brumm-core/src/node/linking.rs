//! Multi-device (SPEC.md §3.2–3.4): linking a fresh device, removing devices and applying
//! updates to our own signed device list. After a link, the linking device syncs contacts and
//! recent history to the new device and announces the new list to every contact.

use anyhow::{Context, Result, bail, ensure};
use iroh::{EndpointId, SecretKey};

use super::{
    LinkTarget, Me, Node, Outcome, PendingLink, RETENTION_MS, history_sync, next_seq, profile_view,
};
use crate::{
    api::{DeviceId, Event},
    clock::now_ms,
    crypto::{self, Sealed},
    e2ee::DeviceCrypto,
    identity::{
        DeviceEntry, DeviceList, MAX_DEVICES, Profile, UserIdentity, default_device_label,
        normalize_display_name,
    },
    link::{LinkBundle, LinkTicket},
    mailbox,
    store::StoredIdentity,
    transport::Transport,
    wire::{
        self, Avatar, ContactRecord, Envelope, Frame, LinkSecret, Payload, RejectReason, Reply,
    },
};

impl Node {
    // ── new device side ─────────────────────────────────────────────────────

    pub(super) async fn start_linking(&mut self) -> Result<()> {
        if self.me.is_some() {
            bail!("this device already has an identity");
        }
        if let Some(target) = &self.link_target {
            self.events.emit(Event::LinkTicket {
                link: target.ticket.to_link(),
            });
            return Ok(());
        }
        let device_secret = SecretKey::generate();
        let (mut crypto, fallback_key) = DeviceCrypto::generate(&device_secret.to_bytes());
        let one_time_key = crypto.new_one_time_key_unsaved();
        let transport = Transport::start(device_secret.clone(), self.inbound_tx.clone());
        let ticket = LinkTicket::new(
            transport.addr().await?,
            rand::random(),
            crypto.identity_key(),
            one_time_key,
            fallback_key,
            mailbox::address(&device_secret.to_bytes(), self.mailbox_relays.clone()),
            default_device_label(),
        );
        self.events.emit(Event::LinkTicket {
            link: ticket.to_link(),
        });
        self.link_target = Some(LinkTarget {
            device_secret,
            crypto,
            transport,
            ticket,
        });
        Ok(())
    }

    /// Handles `Frame::Link` on a device waiting to be linked.
    pub(super) fn accept_link(
        &mut self,
        from: EndpointId,
        link_secret: LinkSecret,
        sealed: &Sealed,
    ) -> Result<Reply> {
        let Some(target) = self
            .link_target
            .as_mut()
            .filter(|target| target.ticket.link_secret == link_secret)
        else {
            return Ok(Reply::Rejected(RejectReason::UnknownInvite));
        };
        let (session, plaintext, sender_identity) = target.crypto.accept_inbound_unsaved(sealed)?;
        let bundle: LinkBundle = wire::decode(&plaintext)?;
        let user = UserIdentity::from_phrase(&bundle.seed_phrase)
            .context("link bundle has an invalid seed")?;
        let my_device = target.device_secret.public();
        let lists_me = bundle.devices.entry(&my_device).is_some_and(|entry| {
            entry.cert.olm_identity_key == target.crypto.identity_key()
                && entry.fallback_key == target.ticket.fallback_key
                && entry.mailbox == target.ticket.mailbox
        });
        let lists_sender = bundle
            .devices
            .entry(&from)
            .is_some_and(|entry| entry.cert.olm_identity_key == sender_identity);
        let valid = user.public() == bundle.devices.user_key
            && bundle.devices.verify()
            && lists_me
            && lists_sender
            && normalize_display_name(&bundle.display_name).as_ref() == Some(&bundle.display_name);
        if !valid {
            return Ok(Reply::Rejected(RejectReason::InvalidProfile));
        }

        let target = self.link_target.take().expect("matched above");
        let profile = Profile {
            display_name: bundle.display_name,
            devices: bundle.devices,
        };
        self.store.save_identity(&StoredIdentity {
            seed_phrase: user.phrase(),
            device_secret: target.device_secret.clone(),
            profile: profile.clone(),
            olm_account: target.crypto.account_pickle(),
        })?;
        let others = other_devices(&profile.devices, &my_device);
        self.store.replace_devices(&user.public(), &others)?;
        self.events.emit(Event::IdentityCreated {
            profile: profile_view(&profile),
            seed_phrase: None,
        });
        let relays = target.ticket.mailbox.relays.clone();
        self.start_mailbox(&target.device_secret, relays);
        self.me = Some(Me {
            user,
            profile,
            device_id: my_device,
            crypto: target.crypto,
            transport: target.transport,
            mailbox: None,
            revoked: false,
        });
        let Self { store, me, .. } = self;
        me.as_mut()
            .expect("just set")
            .crypto
            .adopt(store, from, session)?;
        self.emit_devices();
        Ok(Reply::Ack { id: link_secret })
    }

    // ── existing device side ────────────────────────────────────────────────

    pub(super) fn link_device(&mut self, link: &str) -> Result<()> {
        let ticket = LinkTicket::from_link(link)?;
        let me = self.active_me()?;
        let new_device = ticket.device_addr.id;
        let current = &me.profile.devices;
        ensure!(
            current.entry(&new_device).is_none(),
            "that device is already linked"
        );
        ensure!(
            current.entries.len() < MAX_DEVICES,
            "you can link at most {MAX_DEVICES} devices"
        );

        let entry = DeviceEntry {
            cert: me
                .user
                .certify_device(new_device, ticket.olm_identity_key, now_ms()),
            fallback_key: ticket.fallback_key,
            mailbox: ticket.mailbox.clone(),
            label: ticket.label.clone(),
        };
        let mut entries = current.entries.clone();
        entries.push(entry);
        let devices = me.user.sign_device_list(next_seq(current.seq), entries);
        let bundle = LinkBundle {
            seed_phrase: me.user.phrase(),
            display_name: me.profile.display_name.clone(),
            devices: devices.clone(),
        };
        let mut session = me
            .crypto
            .start_outbound(&ticket.olm_identity_key, &ticket.one_time_key)?;
        let frame = Frame::Link {
            link_secret: ticket.link_secret,
            sealed: crypto::seal(&mut session, &wire::encode(&bundle))?,
        };
        let transport = me.transport.clone();
        self.pending_links.insert(
            new_device,
            PendingLink {
                session,
                devices,
                link_secret: ticket.link_secret,
            },
        );
        let outcomes = self.outcomes_tx.clone();
        n0_future::task::spawn(async move {
            let reply = transport.request(ticket.device_addr, &frame).await;
            let _ = outcomes.send(Outcome::Link {
                device: new_device,
                reply,
            });
        });
        Ok(())
    }

    pub(super) fn finish_link(&mut self, device: EndpointId, reply: Result<Reply>) -> Result<()> {
        let pending = self
            .pending_links
            .remove(&device)
            .context("no pending link for this device")?;
        match reply.context("could not reach the new device — is it still showing the link?")? {
            Reply::Ack { id } if id == pending.link_secret => {}
            other => bail!("new device refused the link: {other:?}"),
        }
        let entry = pending
            .devices
            .entry(&device)
            .cloned()
            .context("new device missing from list")?;
        self.apply_own_device_list(pending.devices)?;
        let Self { store, me, .. } = self;
        me.as_mut()
            .context("no identity")?
            .crypto
            .adopt(store, device, pending.session)?;

        self.sync_to_new_device(&entry)?;
        self.announce_device_list()?;
        self.flush_outbox()
    }

    /// Sends contacts and the retained message history to a freshly linked device.
    fn sync_to_new_device(&mut self, entry: &DeviceEntry) -> Result<()> {
        for contact in self.store.contacts()? {
            if let Some(profile) = self.store.contact_profile(&contact.user_key)? {
                let record = ContactRecord {
                    profile,
                    added_at_ms: contact.added_at_ms,
                };
                self.enqueue_to_device(entry, Payload::ContactSync(record))?;
            }
        }
        for (subject, image) in self.store.avatars()? {
            self.enqueue_to_device(entry, Payload::Avatar(Avatar { subject, image }))?;
        }
        let notes = self.active_me()?.user_key();
        let history = self
            .store
            .history_since(now_ms().saturating_sub(RETENTION_MS), &notes)?;
        // Ahead of the chunks, so the new device can show per-chat progress.
        let counts = history_sync::count_per_contact(&history);
        self.enqueue_to_device(entry, Payload::HistoryManifest(counts))?;
        for chunk in history_sync::prioritized_chunks(history) {
            self.enqueue_to_device(entry, Payload::History(chunk))?;
        }
        Ok(())
    }

    pub(super) fn remove_device(&mut self, device_id: DeviceId) -> Result<()> {
        let device = EndpointId::from_bytes(&device_id)?;
        let me = self.active_me()?;
        ensure!(
            device != me.device_id,
            "remove this device from one of your other devices"
        );
        let current = &me.profile.devices;
        let removed_entry = current.entry(&device).cloned().context("unknown device")?;
        let remaining = current
            .entries
            .iter()
            .filter(|entry| entry.device_id() != device)
            .cloned()
            .collect();
        let devices = me.user.sign_device_list(next_seq(current.seq), remaining);

        self.notify_removed_device(&removed_entry, &devices)?;
        self.apply_own_device_list(devices)?;
        self.announce_device_list()?;
        self.flush_outbox()
    }

    /// Best effort: tell the removed device directly (its outbox entries are dropped next).
    fn notify_removed_device(&mut self, entry: &DeviceEntry, devices: &DeviceList) -> Result<()> {
        let Self { store, me, .. } = self;
        let me = me.as_mut().context("no identity")?;
        let envelope = Envelope {
            id: rand::random(),
            payload: Payload::DeviceList(devices.clone()),
        };
        let sealed = me.crypto.seal(store, entry, &wire::encode(&envelope))?;
        let transport = me.transport.clone();
        let device = entry.device_id();
        n0_future::task::spawn(async move {
            if let Err(err) = transport.request(device, &Frame::Sealed(sealed)).await {
                tracing::info!(%device, ?err, "removed device not reachable; it will stay unaware");
            }
        });
        Ok(())
    }

    pub(super) fn announce_device_list(&mut self) -> Result<()> {
        let devices = self.me()?.profile.devices.clone();
        for contact in self.store.contacts()? {
            self.enqueue_to_user(
                &contact.user_key,
                rand::random(),
                Payload::DeviceList(devices.clone()),
                None,
            )?;
        }
        self.enqueue_to_own_devices(Payload::DeviceList(devices))
    }

    // ── both ────────────────────────────────────────────────────────────────

    /// Adopts a newer own device list (from a link, a removal or another own device).
    pub(super) fn apply_own_device_list(&mut self, list: DeviceList) -> Result<()> {
        let me = self.me.as_mut().context("no identity")?;
        if list.seq <= me.profile.devices.seq {
            return Ok(());
        }
        let still_member = list.entry(&me.device_id).is_some();
        let others = other_devices(&list, &me.device_id);
        let user_key = me.user_key();
        me.profile.devices = list;
        self.store.save_own_profile(&me.profile)?;
        if !still_member {
            me.revoked = true;
            self.store.set_revoked()?;
            self.events.emit(Event::DeviceRevoked);
            return Ok(());
        }
        let removed = self.store.replace_devices(&user_key, &others)?;
        self.forget_devices(&removed)?;
        self.emit_devices();
        Ok(())
    }
}

pub(super) fn other_devices(list: &DeviceList, me: &EndpointId) -> Vec<DeviceEntry> {
    list.entries
        .iter()
        .filter(|entry| &entry.device_id() != me)
        .cloned()
        .collect()
}
