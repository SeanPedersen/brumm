//! Inbound frames and mailbox letters: dispatch, decryption of sealed envelopes and applying
//! their payloads. Every successfully processed ciphertext is remembered so a retried copy is
//! acked again instead of failing to decrypt (its message key is already used). Envelopes that
//! arrive via the mailbox are acknowledged with a `Payload::Receipt` (never for receipts).

use anyhow::Result;
use iroh::EndpointId;

use super::{Node, commands::message_view};
use crate::{
    api::{DeliveryStatus, Direction, Event, Trust},
    clock::now_ms,
    crypto::Sealed,
    identity::{DeviceList, UserKey},
    mailbox::MailboxDelivery,
    transport::InboundRequest,
    wire::{
        self, Avatar, ChatMessage, ContactRecord, Envelope, Frame, HistoryItem, Payload,
        RejectReason, Reply,
    },
};

impl Node {
    pub(super) fn handle_inbound(&mut self, request: InboundRequest) {
        let reply = match self.process_inbound(request.from, request.frame) {
            Ok(reply) => reply,
            Err(err) => {
                tracing::warn!(?err, "inbound frame failed");
                Reply::Rejected(RejectReason::InvalidMessage)
            }
        };
        let _ = request.reply.send(reply);
    }

    fn process_inbound(&mut self, from: EndpointId, frame: Frame) -> Result<Reply> {
        match frame {
            Frame::Hello {
                invite_secret,
                sealed,
            } => self.accept_hello(from, invite_secret, &sealed),
            Frame::Link {
                link_secret,
                sealed,
            } => self.accept_link(from, link_secret, &sealed),
            Frame::Sealed(sealed) => Ok(self.receive_sealed(from, &sealed)?.reply),
        }
    }

    pub(super) fn receive_letter(&mut self, letter: MailboxDelivery) -> Result<()> {
        self.store.advance_mailbox_since(letter.posted_at_secs)?;
        let processed = self.receive_sealed(letter.from, &letter.sealed)?;
        match processed.reply {
            Reply::Ack { id } if processed.wants_receipt => {
                self.enqueue_receipt(&letter.from, vec![id])
            }
            Reply::Ack { .. } => Ok(()),
            other => {
                tracing::info!(from = %letter.from, ?other, "mailbox letter not accepted");
                Ok(())
            }
        }
    }

    fn receive_sealed(&mut self, from: EndpointId, sealed: &Sealed) -> Result<Processed> {
        let rejected = |reason| Processed {
            reply: Reply::Rejected(reason),
            wants_receipt: false,
        };
        let me = self.me()?;
        if me.revoked {
            return Ok(rejected(RejectReason::UnknownSender));
        }
        let my_key = me.user_key();
        let (Some(owner), Some(entry)) = (
            self.store.device_owner(&from)?,
            self.store.device_entry(&from)?,
        ) else {
            return Ok(rejected(RejectReason::UnknownSender));
        };
        let hash = *blake3::hash(&sealed.ciphertext).as_bytes();
        if let Some(id) = self.store.seen_ciphertext(&hash)? {
            // Already processed (and receipted, if it came via the mailbox).
            return Ok(Processed {
                reply: Reply::Ack { id },
                wants_receipt: false,
            });
        }
        let Self { store, me, .. } = self;
        let plaintext = me.as_mut().expect("checked above").crypto.open(
            store,
            &from,
            &entry.cert.olm_identity_key,
            sealed,
        )?;
        let envelope: Envelope = wire::decode(&plaintext)?;
        self.store
            .remember_ciphertext(&hash, &envelope.id, now_ms())?;

        let from_own_device = owner == my_key;
        let wants_receipt = !matches!(envelope.payload, Payload::Receipt(_));
        let accepted = match (envelope.payload, from_own_device) {
            (Payload::Receipt(ids), _) => {
                for id in ids {
                    self.acknowledge(&from, &id, DeliveryStatus::Delivered)?;
                }
                true
            }
            (Payload::Chat(message), false) => self.receive_chat(owner, message)?,
            (Payload::DeviceList(list), _) => self.receive_device_list(owner, list)?,
            (Payload::SentTranscript { contact, message }, true) => {
                self.receive_transcript(contact, message)?
            }
            (Payload::ContactSync(record), true) => self.receive_contact_sync(record)?,
            (Payload::Delete(ids), false) => self.receive_delete(owner, ids)?,
            (Payload::DeleteSync { contact, ids }, true) => {
                self.receive_delete_sync(contact, ids)?
            }
            (Payload::Reaction(reaction), false) => {
                self.receive_reaction(owner, reaction, false)?
            }
            (Payload::ReactionSync { contact, reaction }, true) => {
                self.receive_reaction(contact, reaction, true)?
            }
            (Payload::Avatar(avatar), from_own) => self.receive_avatar(owner, avatar, from_own)?,
            (Payload::History(items), true) => self.receive_history(items)?,
            (Payload::HistoryManifest(counts), true) => self.receive_history_manifest(counts)?,
            (payload, _) => {
                tracing::warn!(
                    ?payload,
                    from_own_device,
                    "payload not allowed from this sender"
                );
                false
            }
        };
        Ok(if accepted {
            Processed {
                reply: Reply::Ack { id: envelope.id },
                wants_receipt,
            }
        } else {
            rejected(RejectReason::InvalidMessage)
        })
    }

    fn receive_chat(&mut self, contact: UserKey, message: ChatMessage) -> Result<bool> {
        if !message.body.is_valid() || self.store.contact(&contact)?.is_none() {
            return Ok(false);
        }
        if self.store.insert_message(
            &contact,
            &message,
            Direction::Incoming,
            DeliveryStatus::Delivered,
            Some(now_ms()),
        )? {
            self.events.emit(Event::MessageAdded(message_view(
                contact,
                &message,
                Direction::Incoming,
                DeliveryStatus::Delivered,
            )));
        }
        Ok(true)
    }

    /// Our other device sent `message` to `contact`.
    fn receive_transcript(&mut self, contact: UserKey, message: ChatMessage) -> Result<bool> {
        if !message.body.is_valid() {
            return Ok(false);
        }
        if self.store.insert_message(
            &contact,
            &message,
            Direction::Outgoing,
            DeliveryStatus::Delivered,
            None,
        )? {
            self.events.emit(Event::MessageAdded(message_view(
                contact,
                &message,
                Direction::Outgoing,
                DeliveryStatus::Delivered,
            )));
        }
        Ok(true)
    }

    /// A contact deletes messages they sent; they cannot touch ours.
    fn receive_delete(&mut self, contact: UserKey, ids: Vec<wire::MessageId>) -> Result<bool> {
        if ids.len() > wire::MAX_DELETE_IDS {
            return Ok(false);
        }
        let removed = self
            .store
            .delete_messages(&contact, &ids, Some(Direction::Incoming))?;
        if !removed.is_empty() {
            self.events.emit(Event::MessagesDeleted {
                contact,
                ids: removed,
            });
            self.emit_preview(&contact)?;
        }
        Ok(true)
    }

    fn receive_delete_sync(&mut self, contact: UserKey, ids: Vec<wire::MessageId>) -> Result<bool> {
        if ids.len() > wire::MAX_DELETE_IDS {
            return Ok(false);
        }
        let removed = self.store.delete_messages(&contact, &ids, None)?;
        if !removed.is_empty() {
            self.events.emit(Event::MessagesDeleted {
                contact,
                ids: removed,
            });
            self.emit_preview(&contact)?;
        }
        Ok(true)
    }

    /// `contact` is the conversation; `mine` tells whose reaction it is. Reactions to messages we do
    /// not have (yet) are acknowledged and dropped.
    fn receive_reaction(
        &mut self,
        contact: UserKey,
        reaction: wire::Reaction,
        mine: bool,
    ) -> Result<bool> {
        if !reaction.is_valid() {
            return Ok(false);
        }
        if !self.store.message_exists(&contact, &reaction.message_id)? {
            return Ok(true);
        }
        if let Some(reactions) = self.store.set_reaction(&contact, &reaction, mine)? {
            self.events.emit(Event::ReactionsChanged {
                contact,
                id: reaction.message_id,
                reactions,
            });
        }
        Ok(true)
    }

    /// Contacts may only set their own photo; our other devices may sync anyone's.
    fn receive_avatar(
        &mut self,
        owner: UserKey,
        avatar: Avatar,
        from_own_device: bool,
    ) -> Result<bool> {
        let allowed = avatar.is_valid()
            && (from_own_device || avatar.subject == owner)
            && (avatar.subject == self.me()?.user_key()
                || self.store.contact(&avatar.subject)?.is_some());
        if !allowed {
            return Ok(false);
        }
        self.store.set_avatar(&avatar.subject, &avatar.image)?;
        self.emit_avatar(&avatar.subject, &avatar.image);
        Ok(true)
    }

    fn receive_device_list(&mut self, owner: UserKey, list: DeviceList) -> Result<bool> {
        if list.user_key != owner || !list.verify() {
            return Ok(false);
        }
        if owner == self.me()?.user_key() {
            return self.apply_own_device_list(list).map(|()| true);
        }
        let known_seq = self
            .store
            .contact_device_list(&owner)?
            .map_or(0, |known| known.seq);
        if list.seq <= known_seq {
            return Ok(true);
        }
        let removed = self.store.set_contact_device_list(&list)?;
        self.forget_devices(&removed)?;
        tracing::info!(devices = list.entries.len(), "contact device list updated");
        if let Some(contact) = self.store.contact(&owner)? {
            self.events.emit(Event::ContactAdded(contact));
        }
        Ok(true)
    }

    fn receive_contact_sync(&mut self, record: ContactRecord) -> Result<bool> {
        let profile = record.profile;
        if !profile.devices.verify() || profile.user_key() == self.me()?.user_key() {
            return Ok(false);
        }
        let known_seq = self
            .store
            .contact_device_list(&profile.user_key())?
            .map(|known| known.seq);
        if known_seq.is_some_and(|seq| seq >= profile.devices.seq) {
            return Ok(true);
        }
        let (contact, removed) =
            self.store
                .upsert_contact(&profile, Trust::Verified, record.added_at_ms)?;
        self.forget_devices(&removed)?;
        self.events.emit(Event::ContactAdded(contact));
        Ok(true)
    }

    fn receive_history(&mut self, items: Vec<HistoryItem>) -> Result<bool> {
        self.note_history_chunk(&items)?;
        let mut changed = Vec::new();
        for item in items {
            if !item.message.body.is_valid() {
                continue;
            }
            let direction = if item.outgoing {
                Direction::Outgoing
            } else {
                Direction::Incoming
            };
            if self.store.insert_message(
                &item.contact,
                &item.message,
                direction,
                DeliveryStatus::Delivered,
                None,
            )? && !changed.contains(&item.contact)
            {
                changed.push(item.contact);
            }
        }
        if !changed.is_empty() {
            for contact in &changed {
                self.emit_preview(contact)?;
            }
            self.events.emit(Event::ConversationsChanged(changed));
        }
        Ok(true)
    }
}

/// Result of processing one sealed envelope.
struct Processed {
    reply: Reply,
    /// The sender expects a `Payload::Receipt` if this arrived via the mailbox.
    wants_receipt: bool,
}
