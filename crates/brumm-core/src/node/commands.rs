//! UI command handling: identity creation/restore, invites and sending text.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use iroh::SecretKey;

use super::{Me, Node, next_seq, profile_view};
use crate::{
    api::{
        AttachmentView, Command, ConversationId, DeliveryStatus, Direction, Event, MessageView,
        ReactionsView,
    },
    clock::now_ms,
    e2ee::DeviceCrypto,
    identity::{
        DeviceEntry, Profile, UserIdentity, UserKey, default_device_label, normalize_display_name,
    },
    group::OpKind,
    invite::Invite,
    mailbox,
    store::StoredIdentity,
    transport::Transport,
    wire::{Avatar, Body, ChatMessage, MAX_DELETE_IDS, MessageId, Payload, Reaction},
};

impl Node {
    pub(super) async fn handle_command(&mut self, command: Command) -> Result<()> {
        if self.logging_out {
            bail!("this device is logging out");
        }
        match command {
            Command::CreateIdentity { display_name } => {
                self.create_identity(UserIdentity::generate(), &display_name, true)
                    .await
            }
            Command::RestoreIdentity {
                phrase,
                display_name,
            } => {
                let user = UserIdentity::from_phrase(&phrase).context("invalid seed phrase")?;
                self.create_identity(user, &display_name, false).await
            }
            Command::StartLinking => self.start_linking().await,
            Command::CreateInvite => self.create_invite(),
            Command::AcceptInvite { link } => self.accept_invite(&link),
            Command::LinkDevice { link } => self.link_device(&link),
            Command::RemoveDevice { device_id } => self.remove_device(device_id),
            Command::LoadConversation {
                conversation,
                limit,
            } => {
                let page = self.store.conversation_page(&conversation, None, limit)?;
                self.events.emit(Event::Conversation {
                    conversation,
                    messages: page.messages,
                    has_older: page.has_older,
                });
                Ok(())
            }
            Command::LoadOlder {
                conversation,
                before,
                limit,
            } => {
                let page = self
                    .store
                    .conversation_page(&conversation, Some(&before), limit)?;
                self.events.emit(Event::OlderMessages {
                    conversation,
                    messages: page.messages,
                    has_older: page.has_older,
                });
                Ok(())
            }
            Command::LoadNewer {
                conversation,
                after,
                limit,
            } => {
                let page = self
                    .store
                    .conversation_page_after(&conversation, after, limit)?;
                self.events.emit(Event::NewerMessages {
                    conversation,
                    after,
                    messages: page.messages,
                    arrivals: page.arrivals,
                    cursor: page.cursor,
                    has_newer: page.has_newer,
                });
                Ok(())
            }
            Command::LoadLatest {
                conversation,
                limit,
            } => {
                let page = self.store.conversation_tail(&conversation, limit)?;
                self.events.emit(Event::LatestMessages {
                    conversation,
                    limit,
                    messages: page.messages,
                    arrivals: page.arrivals,
                    cursor: page.cursor,
                });
                Ok(())
            }
            Command::LoadMessage { conversation, id } => {
                let message = self.store.message_view(&conversation, &id)?;
                self.events.emit(Event::Message {
                    conversation,
                    id,
                    message,
                });
                Ok(())
            }
            Command::RememberOpenChat { conversation } => self.store.set_last_opened(&conversation),
            Command::SendText {
                conversation,
                text,
                id,
            } => self.send_any(conversation, Body::Text(text.trim().to_owned()), id),
            Command::SendFile {
                conversation,
                name,
                data,
                id,
            } => self.send_any(conversation, Body::File { name, data }, id),
            Command::DeleteMessages {
                conversation: ConversationId::Group(group),
                ids,
                for_everyone,
            } => self.delete_group_messages(group, ids, for_everyone),
            Command::DeleteMessages {
                conversation,
                ids,
                for_everyone,
            } => self.delete_messages(conversation, ids, for_everyone),
            Command::SetAvatar { image } => self.set_avatar(image),
            Command::LoadAttachment {
                conversation,
                id,
                max_edge,
            } => self.load_attachment(conversation, id, max_edge),
            Command::StoreImagePreview {
                conversation,
                id,
                width,
                height,
                max_edge,
                bytes,
            } => self.store_image_preview(conversation, id, width, height, max_edge, bytes),
            Command::SaveAttachment {
                conversation,
                id,
                path,
            } => self.save_attachment(conversation, id, path),
            Command::React {
                conversation: ConversationId::Group(group),
                id,
                emoji,
            } => self.react_in_group(group, id, emoji),
            Command::React {
                conversation,
                id,
                emoji,
            } => self.react(conversation, id, emoji),
            Command::LogOut => self.begin_logout(),
            Command::CreateGroup {
                name,
                announcement,
                members,
            } => self.create_group(name, announcement, members),
            Command::InviteToGroup { group, users } => self.invite_to_group(group, users),
            Command::AcceptGroupInvitation { group } => self.answer_group_invitation(group, true),
            Command::DeclineGroupInvitation { group } => {
                self.answer_group_invitation(group, false)
            }
            Command::RemoveMember { group, user } => self
                .change_group(group, OpKind::Remove { user })
                .map(drop),
            Command::SetRole { group, user, admin } => self
                .change_group(group, OpKind::SetRole { user, admin })
                .map(drop),
            Command::RenameGroup { group, name } => self.rename_group(group, name),
            Command::SetAnnouncement { group, on } => self
                .change_group(group, OpKind::SetAnnouncement(on))
                .map(drop),
            Command::SetHistorySharing { group, on } => self
                .change_group(group, OpKind::SetHistorySharing(on))
                .map(drop),
            Command::SetGroupAvatar { group, image } => self.set_group_avatar(group, image),
            Command::LeaveGroup { group } => self.leave_group(group),
            Command::LoadMembers {
                group,
                query,
                offset,
                limit,
            } => self.load_members(group, query, offset, limit),
            Command::RefreshGroupKeys { group } => self.refresh_group_keys(&group),
            Command::Resume => self.resume(),
        }
    }

    async fn create_identity(
        &mut self,
        user: UserIdentity,
        display_name: &str,
        is_new: bool,
    ) -> Result<()> {
        if self.me.is_some() || self.link_target.is_some() {
            bail!("this device already has (or is joining) an identity");
        }
        let display_name =
            normalize_display_name(display_name).context("display name is empty or invalid")?;
        let device_secret = SecretKey::generate();
        let (crypto, fallback_key) = DeviceCrypto::generate(&device_secret.to_bytes());
        let entry = DeviceEntry {
            cert: user.certify_device(device_secret.public(), crypto.identity_key(), now_ms()),
            fallback_key,
            mailbox: mailbox::address(&device_secret.to_bytes(), self.mailbox_relays.clone()),
            label: default_device_label(),
        };
        let profile = Profile {
            display_name,
            devices: user.sign_device_list(next_seq(0), vec![entry]),
        };
        self.store.save_identity(&StoredIdentity {
            seed_phrase: user.phrase(),
            device_secret: device_secret.clone(),
            profile: profile.clone(),
            olm_account: crypto.account_pickle(),
        })?;
        let transport = Transport::start(device_secret.clone(), self.inbound_tx.clone());
        self.start_mailbox(&device_secret, self.mailbox_relays.clone());
        let seed_phrase = is_new.then(|| user.phrase());
        self.events.emit(Event::IdentityCreated {
            profile: profile_view(&profile),
            seed_phrase,
        });
        self.me = Some(Me {
            user,
            device_secret: device_secret.to_bytes(),
            profile,
            device_id: device_secret.public(),
            crypto,
            transport,
            mailbox: None,
            revoked: false,
        });
        self.emit_devices();
        Ok(())
    }

    fn create_invite(&mut self) -> Result<()> {
        self.active_me()?;
        let secret: [u8; 16] = rand::random();
        let Self {
            store, me, events, ..
        } = self;
        let me = me.as_mut().expect("checked by active_me");
        let one_time_key = me.crypto.new_one_time_key(store)?;
        store.add_invite(&secret, now_ms())?;
        let transport = me.transport.clone();
        let display_name = me.profile.display_name.clone();
        let cert = me.own_cert().clone();
        let events = events.clone();
        n0_future::task::spawn(async move {
            match transport.addr().await {
                Ok(addr) => {
                    let invite = Invite::new(display_name, cert, addr, secret, one_time_key);
                    events.emit(Event::InviteCreated {
                        link: invite.to_link(),
                    });
                }
                Err(err) => events.emit(Event::Error(format!("could not create invite: {err:#}"))),
            }
        });
        Ok(())
    }

    /// A caller-chosen `id` that is already stored or deleted is a retry: nothing happens.
    fn send_any(
        &mut self,
        conversation: ConversationId,
        body: Body,
        id: Option<MessageId>,
    ) -> Result<()> {
        if let Some(id) = id
            && self.store.message_known(&conversation, &id)?
        {
            return Ok(());
        }
        let id = id.unwrap_or_else(rand::random);
        match conversation {
            ConversationId::Group(group) => self.send_group_message(group, id, body),
            ConversationId::Direct(_) => self.send_text(conversation, id, body),
        }
    }

    /// Sends a text or file message to a contact (and a transcript to our other devices).
    /// The conversation keyed by our own user key is the "Notes" chat: it only syncs to our devices.
    fn send_text(&mut self, conversation: ConversationId, id: MessageId, body: Body) -> Result<()> {
        let contact = direct_only(conversation)?;
        let is_notes = contact == self.active_me()?.user_key();
        if !is_notes {
            self.store.contact(&contact)?.context("unknown contact")?;
        }
        let message = ChatMessage {
            id,
            sent_at_ms: now_ms(),
            body,
        };
        if !message.body.is_valid() {
            bail!("message is empty, too long or the file is too large");
        }
        // Notes are delivered to our other devices; with none, there is nobody to wait for.
        let status = if is_notes && self.own_other_devices().is_empty() {
            DeliveryStatus::Delivered
        } else {
            DeliveryStatus::Pending
        };
        self.store
            .insert_message(&conversation, &message, Direction::Outgoing, status, None)?;
        self.events.emit(Event::MessageAdded(message_view(
            conversation,
            &message,
            Direction::Outgoing,
            status,
        )));
        self.enqueue_to_user(
            &contact,
            message.id,
            Payload::Chat(message.clone()),
            Some(&conversation),
        )?;
        let message_id = message.id;
        let transcript = Payload::SentTranscript { contact, message };
        if is_notes {
            // Tracked: the transcript's ack is this message's delivery.
            self.enqueue_to_own_devices_as(message_id, transcript, Some(&conversation))?;
        } else {
            self.enqueue_to_own_devices(transcript)?;
        }
        self.flush_outbox()
    }
}

impl Node {
    fn delete_messages(
        &mut self,
        conversation: ConversationId,
        ids: Vec<MessageId>,
        for_everyone: bool,
    ) -> Result<()> {
        self.active_me()?;
        let contact = direct_only(conversation)?;
        if ids.is_empty() || ids.len() > MAX_DELETE_IDS {
            bail!("nothing to delete or too many messages at once");
        }
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
            self.store.drop_outbox_envelopes(&removed)?;
            self.enqueue_to_user(
                &contact,
                rand::random(),
                Payload::Delete(removed.clone()),
                None,
            )?;
        }
        self.enqueue_to_own_devices(Payload::DeleteSync {
            contact,
            ids: removed,
        })?;
        self.flush_outbox()
    }

    fn react(&mut self, conversation: ConversationId, id: MessageId, emoji: String) -> Result<()> {
        self.active_me()?;
        let contact = direct_only(conversation)?;
        let reaction = Reaction {
            message_id: id,
            emoji,
            at_ms: now_ms(),
        };
        if !reaction.is_valid() || !self.store.message_exists(&conversation, &id)? {
            bail!("cannot react to this message");
        }
        if let Some(reactions) = self.store.set_reaction(&conversation, &reaction, true)? {
            self.events.emit(Event::ReactionsChanged {
                conversation,
                id,
                reactions,
            });
        }
        self.enqueue_to_user(
            &contact,
            rand::random(),
            Payload::Reaction(reaction.clone()),
            None,
        )?;
        self.enqueue_to_own_devices(Payload::ReactionSync { contact, reaction })?;
        self.flush_outbox()
    }

    fn load_attachment(
        &mut self,
        conversation: ConversationId,
        id: MessageId,
        max_edge: u32,
    ) -> Result<()> {
        if let Some(preview) = self.store.image_preview(&conversation, &id, max_edge)? {
            self.events.emit(Event::ImagePreview {
                conversation,
                id,
                width: preview.width,
                height: preview.height,
                bytes: Arc::new(preview.bytes),
            });
            return Ok(());
        }
        if let Some(Body::File { data, .. }) = self.store.message_body(&conversation, &id)? {
            self.events.emit(Event::Attachment {
                conversation,
                id,
                data: Arc::new(data),
            });
        }
        Ok(())
    }

    /// Ignores a preview that does not belong to a stored message or is larger than a bubble image.
    fn store_image_preview(
        &mut self,
        conversation: ConversationId,
        id: MessageId,
        width: u32,
        height: u32,
        max_edge: u32,
        bytes: Vec<u8>,
    ) -> Result<()> {
        if !preview_is_storable(width, height, max_edge, bytes.len())
            || !self.store.message_exists(&conversation, &id)?
        {
            return Ok(());
        }
        self.store
            .set_image_preview(&conversation, &id, width, height, max_edge, &bytes)
    }

    /// Reads the file from the database and writes it on another thread. The buffer dies with that thread.
    fn save_attachment(
        &mut self,
        conversation: ConversationId,
        id: MessageId,
        path: std::path::PathBuf,
    ) -> Result<()> {
        let Some(Body::File { name, data }) = self.store.message_body(&conversation, &id)? else {
            bail!("message has no file");
        };
        #[cfg(target_family = "wasm")]
        {
            let _ = path;
            self.events.emit(Event::FileExport {
                name,
                data: Arc::new(data),
            });
            Ok(())
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = name;
            let events = self.events.clone();
            std::thread::spawn(move || {
                if let Err(err) = std::fs::write(&path, data) {
                    events.emit(Event::Error(format!("could not save file: {err}")));
                }
            });
            Ok(())
        }
    }

    /// Stores our new photo and shares it with every contact and our other devices.
    fn set_avatar(&mut self, image: Vec<u8>) -> Result<()> {
        let me = self.active_me()?.user_key();
        let avatar = Avatar { subject: me, image };
        if !avatar.is_valid() {
            bail!("profile photo is too large");
        }
        self.store.set_avatar(&me, &avatar.image)?;
        self.emit_avatar(&me, &avatar.image);
        for contact in self.store.contacts()? {
            self.enqueue_to_user(
                &contact.user_key,
                rand::random(),
                Payload::Avatar(avatar.clone()),
                None,
            )?;
        }
        self.enqueue_to_own_devices(Payload::Avatar(avatar))?;
        self.flush_outbox()
    }

    pub(super) fn emit_avatar(&self, user: &UserKey, image: &[u8]) {
        let image = (!image.is_empty()).then(|| image.to_vec());
        self.events
            .emit(Event::AvatarChanged { user: *user, image });
    }

    /// Tells the UI the new last message after a bulk change (delete, history sync).
    pub(super) fn emit_preview(&self, conversation: &ConversationId) -> Result<()> {
        let last = self.store.last_message(conversation)?;
        self.events.emit(Event::PreviewChanged {
            conversation: *conversation,
            last,
        });
        Ok(())
    }
}

/// The contact of a 1:1 conversation (group conversations are routed before this).
fn direct_only(conversation: ConversationId) -> Result<UserKey> {
    conversation.direct().context("not a 1:1 conversation")
}

/// A preview is a small re-encode. Anything larger is the original, which stays in `messages`.
const MAX_STORED_PREVIEW_BYTES: usize = 1024 * 1024;
const MAX_STORED_PREVIEW_EDGE: u32 = 4096;

fn preview_is_storable(width: u32, height: u32, max_edge: u32, len: usize) -> bool {
    width > 0
        && height > 0
        && max_edge > 0
        && width <= MAX_STORED_PREVIEW_EDGE
        && height <= MAX_STORED_PREVIEW_EDGE
        && max_edge <= MAX_STORED_PREVIEW_EDGE
        && len > 0
        && len <= MAX_STORED_PREVIEW_BYTES
}

pub(super) fn message_view(
    conversation: ConversationId,
    message: &ChatMessage,
    direction: Direction,
    status: DeliveryStatus,
) -> MessageView {
    let (text, attachment) = match &message.body {
        Body::Text(text) => (text.clone(), None),
        Body::File { name, data } => (
            String::new(),
            Some(AttachmentView {
                name: name.clone(),
                size: data.len(),
            }),
        ),
    };
    MessageView {
        id: message.id,
        conversation,
        direction,
        text,
        attachment,
        reactions: ReactionsView::default(),
        sent_at_ms: message.sent_at_ms,
        status,
        sender: None,
        system: false,
    }
}
