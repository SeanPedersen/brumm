//! Reliable per-device delivery. Every payload is wrapped in an [`Envelope`], encrypted
//! **once** for each target device and stored in the outbox; retries resend the same
//! ciphertext (re-encrypting would advance the ratchet unboundedly while a device is offline).
//! An ack from the device removes the entry; for chat messages it marks the message delivered.
//!
//! If a device is unreachable, the same ciphertext is posted once to its Nostr mailbox
//! (status `Sent`). The recipient later answers with a `Payload::Receipt`, which counts as the
//! ack. Direct retries continue meanwhile; duplicates are re-acked by the receiver.

use anyhow::{Context, Result};
use iroh::EndpointId;

use super::{Node, Outcome};
use crate::{
    api::{DeliveryStatus, Event},
    clock::now_ms,
    identity::{DeviceEntry, UserKey},
    wire::{self, Envelope, EnvelopeId, Frame, Payload, RejectReason, Reply},
};

impl Node {
    /// Queues `payload` for every device of `user` (a contact).
    /// Our own user (the Notes chat) is skipped: own devices are served by `enqueue_to_own_devices`.
    pub(super) fn enqueue_to_user(
        &mut self,
        user: &UserKey,
        envelope_id: EnvelopeId,
        payload: Payload,
        chat_contact: Option<&UserKey>,
    ) -> Result<()> {
        if *user == self.me()?.user_key() {
            return Ok(());
        }
        let envelope = Envelope {
            id: envelope_id,
            payload,
        };
        for entry in self.store.devices_of(user)? {
            self.enqueue_sealed(&entry, &envelope, chat_contact, false)?;
        }
        Ok(())
    }

    /// Queues `payload` for each of our other devices.
    pub(super) fn enqueue_to_own_devices(&mut self, payload: Payload) -> Result<()> {
        let me = self.me()?;
        let others: Vec<DeviceEntry> = me
            .profile
            .devices
            .entries
            .iter()
            .filter(|entry| entry.device_id() != me.device_id)
            .cloned()
            .collect();
        let envelope = Envelope {
            id: rand::random(),
            payload,
        };
        for entry in &others {
            self.enqueue_sealed(entry, &envelope, None, false)?;
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
        chat_contact: Option<&UserKey>,
        is_receipt: bool,
    ) -> Result<()> {
        let Self { store, me, .. } = self;
        let me = me.as_mut().context("no identity")?;
        let sealed = me.crypto.seal(store, entry, &wire::encode(envelope))?;
        store.enqueue(
            &entry.device_id(),
            &envelope.id,
            &sealed,
            chat_contact,
            is_receipt,
            now_ms(),
        )
    }

    /// Sends every queued envelope that is not already in flight.
    pub(super) fn flush_outbox(&mut self) -> Result<()> {
        let Some(me) = self.me.as_ref().filter(|me| !me.revoked) else {
            return Ok(());
        };
        let transport = me.transport.clone();
        for item in self.store.outbox()? {
            let key = (item.device, item.envelope_id);
            if !self.in_flight.insert(key) {
                continue;
            }
            let transport = transport.clone();
            let outcomes = self.outcomes_tx.clone();
            tokio::spawn(async move {
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
                self.acknowledge(&device, &envelope_id, DeliveryStatus::Delivered)
            }
            Ok(Reply::Rejected(RejectReason::InvalidMessage)) => {
                tracing::warn!(%device, "envelope rejected as invalid");
                self.acknowledge(&device, &envelope_id, DeliveryStatus::Failed)
            }
            Ok(other) => {
                tracing::debug!(%device, ?other, "not accepted yet, will retry");
                Ok(())
            }
            Err(err) => {
                tracing::debug!(%device, ?err, "device unreachable, falling back to its mailbox");
                self.post_to_mailbox(device, envelope_id)
            }
        }
    }

    /// Posts a queued envelope once to the device's mailbox (no-op without mailbox / if done).
    fn post_to_mailbox(&mut self, device: EndpointId, envelope_id: EnvelopeId) -> Result<()> {
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
        tokio::spawn(async move {
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
        let Some(contact) = self.store.outbox_chat_contact(&device, &envelope_id)? else {
            return Ok(());
        };
        if self.store.message_status(&contact, &envelope_id)? == Some(DeliveryStatus::Pending) {
            self.store
                .set_status(&contact, &envelope_id, DeliveryStatus::Sent)?;
            self.events.emit(Event::MessageStatusChanged {
                contact,
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
        let Some(Some(contact)) = self.store.dequeue(&device, &envelope_id)? else {
            return Ok(());
        };
        // Delivered to any device of the contact wins over a failure on another device.
        if self.store.message_status(&contact, &envelope_id)? == Some(DeliveryStatus::Delivered) {
            return Ok(());
        }
        self.store.set_status(&contact, &envelope_id, status)?;
        self.events.emit(Event::MessageStatusChanged {
            contact,
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
