//! Reliable per-device delivery. Every payload is wrapped in an [`Envelope`], encrypted
//! **once** for each target device and stored in the outbox; retries resend the same
//! ciphertext (re-encrypting would advance the ratchet unboundedly while a device is offline).
//! An ack from the device removes the entry; for chat messages it marks the message delivered.
//!
//! The one exception: a device that answers `Undecryptable` (it no longer has the session) gets
//! the envelope encrypted again, once, on a fresh session from its fallback key. Any other
//! rejection except `InvalidMessage` is retried with the same bytes.
//!
//! If a device is unreachable, the same ciphertext is posted once to its Nostr mailbox
//! (status `Sent`). The recipient later answers with a `Payload::Receipt`, which counts as the
//! ack. Direct retries continue meanwhile; duplicates are re-acked by the receiver.
//!
//! The outbox is sent in queue order with a few envelopes in flight per device, so priorities
//! set by the enqueue order hold (history sync sends the most recent chat first).

use anyhow::{Context, Result};
use iroh::EndpointId;

use super::{Node, Outcome};
use crate::{
    api::{ConversationId, DeliveryStatus, Event},
    clock::now_ms,
    identity::{DeviceEntry, UserKey},
    wire::{self, Envelope, EnvelopeId, Frame, Payload, RejectReason, Reply},
};

/// Envelopes sent to one device at once. Small enough that the first chats of a history sync
/// arrive first, large enough that a big file does not hold up everything behind it.
const MAX_IN_FLIGHT_PER_DEVICE: usize = 2;

impl Node {
    /// Queues `payload` for every device of `user` (a contact).
    /// Our own user (the Notes chat) is skipped: own devices are served by `enqueue_to_own_devices`.
    pub(super) fn enqueue_to_user(
        &mut self,
        user: &UserKey,
        envelope_id: EnvelopeId,
        payload: Payload,
        chat: Option<&ConversationId>,
    ) -> Result<()> {
        if *user == self.me()?.user_key() {
            return Ok(());
        }
        let envelope = Envelope {
            id: envelope_id,
            payload,
        };
        for entry in self.store.devices_of(user)? {
            self.enqueue_sealed(&entry, &envelope, chat, false)?;
        }
        Ok(())
    }

    /// Queues `payload` for each of our other devices.
    pub(super) fn enqueue_to_own_devices(&mut self, payload: Payload) -> Result<()> {
        self.enqueue_to_own_devices_as(rand::random(), payload, None)
    }

    /// Like [`Self::enqueue_to_own_devices`] under a given envelope id. With `chat` the
    /// message of that id in that chat shows the delivery state of this envelope (Notes: there
    /// is no other recipient, so our other devices are the delivery).
    pub(super) fn enqueue_to_own_devices_as(
        &mut self,
        envelope_id: EnvelopeId,
        payload: Payload,
        chat: Option<&ConversationId>,
    ) -> Result<()> {
        let envelope = Envelope {
            id: envelope_id,
            payload,
        };
        for entry in &self.own_other_devices() {
            self.enqueue_sealed(entry, &envelope, chat, false)?;
            // A browser tab can end at any moment and gets no time for the shutdown drain
            // (`drain.rs`), so it puts the letter on the relays now instead of after a failed
            // direct attempt. Native builds drain on exit and avoid the extra relay traffic.
            #[cfg(target_family = "wasm")]
            self.post_to_mailbox(entry.device_id(), envelope.id)?;
        }
        Ok(())
    }

    pub(super) fn enqueue_to_device(
        &mut self,
        entry: &DeviceEntry,
        payload: Payload,
    ) -> Result<()> {
        self.enqueue_sealed(
            entry,
            &Envelope {
                id: rand::random(),
                payload,
            },
            None,
            false,
        )
    }

    /// Acknowledges envelopes that reached us through the mailbox.
    pub(super) fn enqueue_receipt(
        &mut self,
        device: &EndpointId,
        envelope_ids: Vec<EnvelopeId>,
    ) -> Result<()> {
        let Some(entry) = self.store.device_entry(device)? else {
            return Ok(());
        };
        let envelope = Envelope {
            id: rand::random(),
            payload: Payload::Receipt(envelope_ids),
        };
        self.enqueue_sealed(&entry, &envelope, None, true)?;
        self.flush_outbox()
    }

    fn enqueue_sealed(
        &mut self,
        entry: &DeviceEntry,
        envelope: &Envelope,
        chat: Option<&ConversationId>,
        is_receipt: bool,
    ) -> Result<()> {
        let Self { store, me, .. } = self;
        let me = me.as_mut().context("no identity")?;
        let plaintext = wire::encode(envelope);
        let sealed = me.crypto.seal(store, entry, &plaintext)?;
        store.enqueue(
            &entry.device_id(),
            &envelope.id,
            &sealed,
            &plaintext,
            chat,
            is_receipt,
            now_ms(),
        )
    }

    /// Sends queued envelopes in queue order, at most [`MAX_IN_FLIGHT_PER_DEVICE`] per device.
    /// Queue order is the priority (history sync: most recent chat first); later envelopes
    /// wait for a slot instead of splitting the bandwidth, and start as soon as one is acked.
    pub(super) fn flush_outbox(&mut self) -> Result<()> {
        let Some(me) = self.me.as_ref().filter(|me| !me.revoked) else {
            return Ok(());
        };
        let transport = me.transport.clone();
        for key in self.store.outbox_keys()? {
            let (device, envelope_id) = key;
            let busy = self.in_flight.iter().filter(|(d, _)| *d == device).count();
            if busy >= MAX_IN_FLIGHT_PER_DEVICE || self.in_flight.contains(&key) {
                continue;
            }
            let Some(item) = self.store.outbox_item(&device, &envelope_id)? else {
                continue;
            };
            self.in_flight.insert(key);
            let transport = transport.clone();
            let outcomes = self.outcomes_tx.clone();
            n0_future::task::spawn(async move {
                let reply = transport
                    .request(item.device, &Frame::Sealed(item.sealed))
                    .await;
                let _ = outcomes.send(Outcome::Delivery {
                    device: item.device,
                    envelope_id: item.envelope_id,
                    reply,
                });
            });
        }
        Ok(())
    }

    pub(super) fn finish_delivery(
        &mut self,
        device: EndpointId,
        envelope_id: EnvelopeId,
        reply: Result<Reply>,
    ) -> Result<()> {
        self.in_flight.remove(&(device, envelope_id));
        match reply {
            Ok(Reply::Ack { id }) if id == envelope_id => {
                self.acknowledge(&device, &envelope_id, DeliveryStatus::Delivered)?;
                self.own_device_reachable(device)?;
                // A slot for this device is free: send the next queued envelope.
                self.flush_outbox()
            }
            Ok(Reply::Rejected(RejectReason::InvalidMessage)) => {
                tracing::warn!(%device, "envelope rejected as invalid");
                self.acknowledge(&device, &envelope_id, DeliveryStatus::Failed)
            }
            Ok(Reply::Rejected(RejectReason::Undecryptable)) => self.reseal(device, envelope_id),
            Ok(other) => {
                tracing::debug!(%device, ?other, "not accepted yet, will retry");
                Ok(())
            }
            Err(err) => {
                tracing::debug!(%device, ?err, "device unreachable, falling back to its mailbox");
                self.own_device_unreachable(&device);
                // Everything queued for it, not only the envelopes that had a slot.
                for (queued_device, queued_id) in self.store.outbox_keys()? {
                    if queued_device == device {
                        self.post_to_mailbox(device, queued_id)?;
                    }
                }
                Ok(())
            }
        }
    }

    /// The device cannot decrypt the stored ciphertext: encrypt the envelope again on a fresh
    /// session. Only once per entry; after that (or for entries from before schema v8, which
    /// have no plaintext) the envelope fails like an invalid one.
    fn reseal(&mut self, device: EndpointId, envelope_id: EnvelopeId) -> Result<()> {
        let (Some((plaintext, sealed_at_ms)), Some(entry)) = (
            self.store.outbox_envelope(&device, &envelope_id)?,
            self.store.device_entry(&device)?,
        ) else {
            tracing::warn!(%device, "envelope undecryptable again, giving up");
            return self.acknowledge(&device, &envelope_id, DeliveryStatus::Failed);
        };
        tracing::info!(%device, "envelope undecryptable, encrypting it again on a fresh session");
        self.ensure_fresh_session(&entry, sealed_at_ms)?;
        let Self { store, me, .. } = self;
        let me = me.as_mut().context("no identity")?;
        let sealed = me.crypto.seal(store, &entry, &plaintext)?;
        store.reseal(&device, &envelope_id, &sealed)?;
        self.flush_outbox()
    }

    /// Starts a fresh session to `entry`, unless one was started after `sealed_at_ms` already
    /// (then that one is new to the peer too). Many queued envelopes failing together therefore
    /// share one new session instead of each replacing the previous one.
    pub(super) fn ensure_fresh_session(
        &mut self,
        entry: &DeviceEntry,
        sealed_at_ms: u64,
    ) -> Result<()> {
        let device = entry.device_id();
        if self
            .fresh_sessions
            .get(&device)
            .is_some_and(|started_ms| *started_ms > sealed_at_ms)
        {
            return Ok(());
        }
        let Self { store, me, .. } = self;
        me.as_mut()
            .context("no identity")?
            .crypto
            .restart_session(store, entry)?;
        // Strictly after anything sealed before this point, also within the same millisecond.
        self.fresh_sessions
            .insert(device, now_ms().max(sealed_at_ms) + 1);
        Ok(())
    }

    /// Sends `payload` once, directly and without the outbox: for the steps of the consistency
    /// check, which are only useful while the device is reachable and are repeated anyway.
    pub(super) fn send_direct(&mut self, entry: &DeviceEntry, payload: Payload) -> Result<()> {
        let Self { store, me, .. } = self;
        let me = me.as_mut().context("no identity")?;
        let envelope = Envelope {
            id: rand::random(),
            payload,
        };
        let sealed_at_ms = now_ms();
        let sealed = me.crypto.seal(store, entry, &wire::encode(&envelope))?;
        let transport = me.transport.clone();
        let outcomes = self.outcomes_tx.clone();
        let device = entry.device_id();
        n0_future::task::spawn(async move {
            let reply = transport.request(device, &Frame::Sealed(sealed)).await;
            let _ = outcomes.send(Outcome::Direct {
                device,
                sealed_at_ms,
                reply,
            });
        });
        Ok(())
    }

    pub(super) fn finish_direct(
        &mut self,
        device: EndpointId,
        sealed_at_ms: u64,
        reply: Result<Reply>,
    ) -> Result<()> {
        match reply {
            Ok(Reply::Ack { .. }) => Ok(()),
            Ok(Reply::Rejected(RejectReason::Undecryptable)) => {
                let Some(entry) = self.store.device_entry(&device)? else {
                    return Ok(());
                };
                self.ensure_fresh_session(&entry, sealed_at_ms)
            }
            Ok(other) => {
                tracing::debug!(%device, ?other, "direct envelope not accepted");
                Ok(())
            }
            Err(err) => {
                tracing::debug!(%device, ?err, "device unreachable for a direct envelope");
                self.own_device_unreachable(&device);
                Ok(())
            }
        }
    }

    /// Posts a queued envelope once to the device's mailbox (no-op without mailbox / if done).
    pub(super) fn post_to_mailbox(
        &mut self,
        device: EndpointId,
        envelope_id: EnvelopeId,
    ) -> Result<()> {
        let Some(me) = self.me.as_ref() else {
            return Ok(());
        };
        let Some(mailbox) = me.mailbox.clone() else {
            return Ok(());
        };
        let Some(item) = self
            .store
            .outbox_item(&device, &envelope_id)?
            .filter(|item| !item.mailboxed)
        else {
            return Ok(());
        };
        let Some(entry) = self.store.device_entry(&device)? else {
            return Ok(());
        };
        if !self.mailboxing.insert((device, envelope_id)) {
            return Ok(());
        }
        let from = me.device_id;
        let outcomes = self.outcomes_tx.clone();
        n0_future::task::spawn(async move {
            let result = mailbox.post(&entry.mailbox, from, item.sealed).await;
            let _ = outcomes.send(Outcome::Mailboxed {
                device,
                envelope_id,
                result,
            });
        });
        Ok(())
    }

    pub(super) fn finish_mailboxing(
        &mut self,
        device: EndpointId,
        envelope_id: EnvelopeId,
        result: Result<()>,
    ) -> Result<()> {
        self.mailboxing.remove(&(device, envelope_id));
        if let Err(err) = result {
            tracing::info!(%device, ?err, "mailbox post failed, will retry when next unreachable");
            return Ok(());
        }
        let Some(item) = self.store.outbox_item(&device, &envelope_id)? else {
            return Ok(());
        };
        if item.is_receipt {
            self.store.dequeue(&device, &envelope_id)?;
            return Ok(());
        }
        self.store.mark_mailboxed(&device, &envelope_id)?;
        let Some(conversation) = self.store.outbox_chat(&device, &envelope_id)? else {
            return Ok(());
        };
        if self.store.message_status(&conversation, &envelope_id)? == Some(DeliveryStatus::Pending)
        {
            self.store
                .set_status(&conversation, &envelope_id, DeliveryStatus::Sent)?;
            self.events.emit(Event::MessageStatusChanged {
                conversation,
                id: envelope_id,
                status: DeliveryStatus::Sent,
            });
        }
        Ok(())
    }

    /// The device processed the envelope (direct ack or mailbox receipt).
    pub(super) fn acknowledge(
        &mut self,
        device: &EndpointId,
        envelope_id: &EnvelopeId,
        status: DeliveryStatus,
    ) -> Result<()> {
        let (device, envelope_id) = (*device, *envelope_id);
        let Some(Some(conversation)) = self.store.dequeue(&device, &envelope_id)? else {
            return Ok(());
        };
        // Delivered to any device of the contact wins over a failure on another device.
        if self.store.message_status(&conversation, &envelope_id)?
            == Some(DeliveryStatus::Delivered)
        {
            return Ok(());
        }
        self.store.set_status(&conversation, &envelope_id, status)?;
        self.events.emit(Event::MessageStatusChanged {
            conversation,
            id: envelope_id,
            status,
        });
        Ok(())
    }

    /// Drops sessions and queued envelopes of devices that left a device list.
    pub(super) fn forget_devices(&mut self, removed: &[EndpointId]) -> Result<()> {
        let Self { store, me, .. } = self;
        for device in removed {
            store.drop_outbox_for(device)?;
            if let Some(me) = me.as_mut() {
                me.crypto.forget_device(store, device)?;
            }
        }
        Ok(())
    }
}
