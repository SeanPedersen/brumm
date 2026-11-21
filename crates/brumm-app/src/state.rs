//! UI-side mirror of core state, updated only by applying core [`Event`]s.
//!
//! Startup stays cheap: the core sends one [`ConversationPreview`] per chat instead of the full
//! history. A conversation's messages are loaded only when it is opened (its newest page; older
//! pages follow on scroll-up), and caches for
//! conversations and file bytes are bounded so long chats cannot grow RAM without limit.
//! Photo bytes are not decoded here. An attachment event is queued and a background thread
//! builds the preview.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

use brumm_core::{
    api::{Command, ContactView, DeviceView, Event, MessageView, ProfileView, Trust},
    identity::UserKey,
    wire::MessageId,
};

const MAX_VISIBLE_ERRORS: usize = 3;
/// Messages loaded when a chat opens and per scroll-up page: more than a tall window shows,
/// few enough that opening any chat is instant.
pub const CONVERSATION_PAGE: u32 = 50;
const NOTES_NAME: &str = "Notes";
/// File bytes kept in memory across all chats; older entries are dropped first.
const ATTACHMENT_CACHE_BYTES: usize = 48 * 1024 * 1024;
const ATTACHMENT_CACHE_ENTRIES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Onboarding,
    Main,
    Revoked,
}

/// A profile photo ready for `egui::Image`; the uri changes whenever the bytes do.
#[derive(Clone)]
pub struct AvatarImage {
    pub uri: String,
    pub bytes: Arc<[u8]>,
}

/// Bubble-sized image bytes, fetched on demand, plus the source pixel size.
///
/// A photo larger than the bubble is re-encoded down to that size. The original stays in the
/// database; Save As reads it back and does not leave it in this cache.
#[derive(Clone)]
pub struct LoadedAttachment {
    pub bytes: Arc<[u8]>,
    pub image_size: Option<(u32, u32)>,
}

/// Stable image-cache key for an attachment; matches what the bubble renders.
pub fn attachment_uri(id: &MessageId) -> String {
    format!("bytes://attachment/{}", hex(id))
}

#[derive(Default)]
pub struct AppState {
    pub avatars: HashMap<UserKey, AvatarImage>,
    pub attachments: HashMap<(UserKey, MessageId), LoadedAttachment>,
    pub(crate) attachment_order: VecDeque<(UserKey, MessageId)>,
    pub(crate) attachment_bytes: usize,
    pub ready: bool,
    pub profile: Option<ProfileView>,
    pub contacts: Vec<ContactView>,
    pub selected: Option<UserKey>,
    /// Chat the user just opened. The app sends [`brumm_core::api::Command::RememberOpenChat`] once.
    pub remember_open: Option<UserKey>,
    /// Messages of opened chats only: the newest page, plus older pages loaded on scroll-up.
    pub conversations: HashMap<UserKey, Vec<MessageView>>,
    /// Chats with more stored messages above the loaded ones.
    pub has_older: HashSet<UserKey>,
    /// Newest message per chat, for the sidebar's preview and ordering.
    pub previews: HashMap<UserKey, MessageView>,
    pub unread: HashMap<UserKey, usize>,
    /// Shown once right after identity creation so the user can write it down.
    pub seed_phrase_to_show: Option<String>,
    pub invite_link: Option<String>,
    /// Shown while this fresh device waits to be linked.
    pub link_ticket: Option<String>,
    pub devices: Vec<DeviceView>,
    pub revoked: bool,
    /// Chats whose initial history is still arriving (freshly linked device): received, expected.
    pub history_sync: HashMap<UserKey, (u32, u32)>,
    /// `Command::LogOut` was sent; the UI blocks until `Event::LoggedOut`.
    pub logging_out: bool,
    /// Opened conversations changed in bulk; the app reloads them from the core.
    pub stale_conversations: Vec<UserKey>,
    pub connecting: bool,
    pub errors: Vec<String>,
    /// Originals waiting for the background preview thread. The UI thread does not decode them.
    pub(crate) pending_previews: VecDeque<PendingPreview>,
}

/// One photo whose bytes arrived and still need a bubble-sized preview.
pub(crate) struct PendingPreview {
    pub contact: UserKey,
    pub id: MessageId,
    pub bytes: Arc<Vec<u8>>,
    pub max_edge: u32,
}

impl AppState {
    /// Loads the newest page of a chat, or as many messages as are already shown (a reload
    /// after a bulk change keeps the pages the user scrolled through).
    pub fn load_command(&self, contact: UserKey) -> Command {
        let loaded = self.conversations.get(&contact).map_or(0, Vec::len);
        let limit = (loaded as u32).max(CONVERSATION_PAGE);
        Command::LoadConversation {
            contact,
            limit: Some(limit),
        }
    }

    /// The next older page (or everything older for `all`), if any is left.
    pub fn older_command(&self, contact: UserKey, all: bool) -> Option<Command> {
        if !self.has_older.contains(&contact) {
            return None;
        }
        let oldest = self.conversations.get(&contact)?.first()?;
        Some(Command::LoadOlder {
            contact,
            before: oldest.id,
            limit: (!all).then_some(CONVERSATION_PAGE),
        })
    }

    fn set_has_older(&mut self, contact: UserKey, has_older: bool) {
        if has_older {
            self.has_older.insert(contact);
        } else {
            self.has_older.remove(&contact);
        }
    }

    /// Any message of this chat is known (loaded, or as the sidebar preview).
    pub fn has_messages(&self, contact: &UserKey) -> bool {
        self.previews.contains_key(contact)
            || self
                .conversations
                .get(contact)
                .is_some_and(|messages| !messages.is_empty())
    }

    pub fn phase(&self) -> Phase {
        match (self.ready, &self.profile, self.revoked) {
            (false, _, _) => Phase::Starting,
            (true, _, true) => Phase::Revoked,
            (true, None, _) => Phase::Onboarding,
            (true, Some(_), _) => Phase::Main,
        }
    }

    /// The "Notes" chat is the conversation with our own user key.
    pub fn is_notes(&self, key: &UserKey) -> bool {
        self.profile
            .as_ref()
            .is_some_and(|profile| &profile.user_key == key)
    }

    /// Lists the Notes chat as a pseudo-contact so it appears in the chat list from the start.
    fn ensure_notes_contact(&mut self) {
        let Some(profile) = &self.profile else { return };
        if self.contact(&profile.user_key).is_some() {
            return;
        }
        let notes = ContactView {
            user_key: profile.user_key,
            display_name: NOTES_NAME.to_owned(),
            fingerprint: profile.fingerprint.clone(),
            trust: Trust::Verified,
            added_at_ms: 0,
        };
        self.contacts.push(notes);
    }

    pub fn contact(&self, key: &UserKey) -> Option<&ContactView> {
        self.contacts
            .iter()
            .find(|contact| &contact.user_key == key)
    }

    /// Selects a chat and drops cached data of the previous one; only one chat is shown at a time.
    /// A changed selection is remembered on this device.
    pub fn select(&mut self, key: UserKey, ctx: &egui::Context) {
        self.open(key, ctx, true);
    }

    /// Shows `key` without writing it back. Used when restoring the chat saved at the last run.
    fn restore(&mut self, key: UserKey, ctx: &egui::Context) {
        self.open(key, ctx, false);
        self.stale_conversations.push(key);
    }

    fn open(&mut self, key: UserKey, ctx: &egui::Context, remember: bool) {
        if remember && self.selected != Some(key) {
            self.remember_open = Some(key);
        }
        self.selected = Some(key);
        self.unread.remove(&key);
        self.conversations.retain(|cached, _| *cached == key);
        let stale: Vec<(UserKey, MessageId)> = self
            .attachments
            .keys()
            .filter(|(contact, _)| *contact != key)
            .copied()
            .collect();
        for id in stale {
            self.forget_attachment(id, ctx);
        }
    }

    /// Adds or updates a contact; jumps to it only when the user just connected to it.
    fn upsert_contact(&mut self, contact: ContactView, ctx: &egui::Context) {
        let key = contact.user_key;
        match self.contacts.iter_mut().find(|known| known.user_key == key) {
            Some(known) => *known = contact,
            None => self.contacts.push(contact),
        }
        self.contacts
            .sort_by_key(|contact| contact.display_name.to_lowercase());
        if std::mem::take(&mut self.connecting) {
            self.select(key, ctx);
        }
    }

    pub fn apply(&mut self, event: Event, ctx: &egui::Context) {
        match event {
            Event::Ready {
                profile,
                contacts,
                previews,
                last_opened,
            } => {
                self.ready = true;
                self.profile = profile;
                self.previews = previews
                    .into_iter()
                    .filter_map(|preview| preview.last.map(|last| (preview.contact, last)))
                    .collect();
                self.contacts = contacts;
                self.ensure_notes_contact();
                // History loads on the next frame via LoadConversation, so Ready stays a few keyed reads.
                if let Some(contact) = last_opened.filter(|key| self.contact(key).is_some()) {
                    self.restore(contact, ctx);
                }
            }
            Event::IdentityCreated {
                profile,
                seed_phrase,
            } => {
                self.profile = Some(profile);
                self.seed_phrase_to_show = seed_phrase;
                self.ensure_notes_contact();
                self.link_ticket = None;
            }
            Event::LinkTicket { link } => self.link_ticket = Some(link),
            Event::Devices(devices) => self.devices = devices,
            Event::DeviceRevoked => self.revoked = true,
            Event::InviteCreated { link } => self.invite_link = Some(link),
            Event::ContactAdded(contact) => self.upsert_contact(contact, ctx),
            Event::ConversationsChanged(contacts) => {
                for contact in contacts {
                    // The open chat keeps showing its messages until the reload replaces them;
                    // dropping them would blank the chat once per synced history chunk.
                    if self.selected == Some(contact) {
                        self.stale_conversations.push(contact);
                    } else {
                        self.conversations.remove(&contact);
                    }
                }
            }
            Event::Conversation {
                contact,
                messages,
                has_older,
            } => {
                if let Some(last) = messages.last() {
                    self.remember_preview(last.clone());
                }
                self.conversations.insert(contact, messages);
                self.set_has_older(contact, has_older);
            }
            Event::OlderMessages {
                contact,
                messages,
                has_older,
            } => {
                // A reload may have replaced the list meanwhile; never insert a message twice.
                let Some(loaded) = self.conversations.get_mut(&contact) else {
                    return;
                };
                let known: HashSet<MessageId> = loaded.iter().map(|message| message.id).collect();
                let mut merged: Vec<MessageView> = messages
                    .into_iter()
                    .filter(|message| !known.contains(&message.id))
                    .collect();
                merged.append(loaded);
                *loaded = merged;
                self.set_has_older(contact, has_older);
            }
            Event::PreviewChanged { contact, last } => match last {
                Some(last) => {
                    self.previews.insert(contact, last);
                }
                None => {
                    self.previews.remove(&contact);
                }
            },
            Event::MessageAdded(message) => {
                let contact = message.contact;
                if self.selected != Some(contact) {
                    *self.unread.entry(contact).or_default() += 1;
                }
                if self.selected == Some(contact) {
                    self.conversations
                        .entry(contact)
                        .or_default()
                        .push(message.clone());
                }
                self.remember_preview(message);
            }
            Event::MessageStatusChanged {
                contact,
                id,
                status,
            } => {
                let message = self.conversations.get_mut(&contact).and_then(|messages| {
                    messages.iter_mut().rev().find(|message| message.id == id)
                });
                if let Some(message) = message {
                    message.status = status;
                }
            }
            Event::MessagesDeleted { contact, ids } => {
                if let Some(messages) = self.conversations.get_mut(&contact) {
                    messages.retain(|message| !ids.contains(&message.id));
                }
                for id in &ids {
                    self.forget_attachment((contact, *id), ctx);
                }
            }
            Event::ReactionsChanged {
                contact,
                id,
                reactions,
            } => {
                let message = self.conversations.get_mut(&contact).and_then(|messages| {
                    messages.iter_mut().rev().find(|message| message.id == id)
                });
                if let Some(message) = message {
                    message.reactions = reactions;
                }
            }
            Event::AvatarChanged { user, image } => match image {
                Some(bytes) => {
                    let uri = format!("bytes://avatar/{}-{}", hex_prefix(&user), checksum(&bytes));
                    self.avatars.insert(
                        user,
                        AvatarImage {
                            uri,
                            bytes: bytes.into(),
                        },
                    );
                }
                None => {
                    if let Some(old) = self.avatars.remove(&user) {
                        ctx.forget_image(&old.uri);
                    }
                }
            },
            Event::Attachment { contact, id, data } => {
                self.pending_previews.push_back(PendingPreview {
                    contact,
                    id,
                    bytes: data,
                    max_edge: crate::ui::files::preview_max_edge(ctx.pixels_per_point()),
                });
            }
            Event::ImagePreview {
                contact,
                id,
                width,
                height,
                bytes,
            } => {
                self.remember_attachment(
                    (contact, id),
                    LoadedAttachment {
                        bytes: arc_bytes(bytes),
                        image_size: Some((width, height)),
                    },
                    ctx,
                );
            }
            Event::HistorySync(chats) => {
                self.history_sync = chats
                    .into_iter()
                    .map(|chat| (chat.contact, (chat.received, chat.expected)))
                    .collect();
            }
            Event::LoggedOut => {
                ctx.forget_all_images();
                *self = Self {
                    ready: true,
                    ..Self::default()
                };
            }
            Event::Error(error) => {
                self.connecting = false;
                self.logging_out = false;
                self.errors.push(error);
                let overflow = self.errors.len().saturating_sub(MAX_VISIBLE_ERRORS);
                self.errors.drain(..overflow);
            }
        }
    }

    /// Keeps the newest message per chat; older arrivals (history sync) do not move the preview back.
    fn remember_preview(&mut self, message: MessageView) {
        let newer = self
            .previews
            .get(&message.contact)
            .is_none_or(|known| (known.sent_at_ms, known.id) <= (message.sent_at_ms, message.id));
        if newer {
            self.previews.insert(message.contact, message);
        }
    }

    fn remember_attachment(
        &mut self,
        key: (UserKey, MessageId),
        loaded: LoadedAttachment,
        ctx: &egui::Context,
    ) {
        self.forget_attachment(key, ctx);
        self.attachment_bytes += loaded.bytes.len();
        self.attachments.insert(key, loaded);
        self.attachment_order.push_back(key);
        while self.attachments.len() > ATTACHMENT_CACHE_ENTRIES
            || self.attachment_bytes > ATTACHMENT_CACHE_BYTES
        {
            let Some(oldest) = self.attachment_order.pop_front() else {
                break;
            };
            if let Some(dropped) = self.attachments.remove(&oldest) {
                self.attachment_bytes = self.attachment_bytes.saturating_sub(dropped.bytes.len());
                ctx.forget_image(&attachment_uri(&oldest.1));
            }
        }
    }

    pub(crate) fn take_pending_preview(&mut self) -> Option<PendingPreview> {
        self.pending_previews.pop_front()
    }

    pub(crate) fn install_attachment(
        &mut self,
        key: (UserKey, MessageId),
        loaded: LoadedAttachment,
        ctx: &egui::Context,
    ) {
        self.remember_attachment(key, loaded, ctx);
    }

    fn forget_attachment(&mut self, key: (UserKey, MessageId), ctx: &egui::Context) {
        if let Some(dropped) = self.attachments.remove(&key) {
            self.attachment_bytes = self.attachment_bytes.saturating_sub(dropped.bytes.len());
            self.attachment_order.retain(|entry| *entry != key);
            ctx.forget_image(&attachment_uri(&key.1));
        }
    }
}

fn arc_bytes(data: Arc<Vec<u8>>) -> Arc<[u8]> {
    match Arc::try_unwrap(data) {
        Ok(bytes) => Arc::from(bytes),
        Err(bytes) => Arc::from(&bytes[..]),
    }
}

const HEX_PREFIX_BYTES: usize = 4;

fn hex_prefix(key: &UserKey) -> String {
    key[..HEX_PREFIX_BYTES]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Cheap content fingerprint (FNV-1a) so a changed photo gets a fresh image-cache uri.
fn checksum(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;
    bytes.iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

#[cfg(test)]
mod tests {
    use brumm_core::api::{ContactView, ProfileView, Trust};

    use super::*;

    fn contact(user_key: UserKey, name: &str) -> ContactView {
        ContactView {
            user_key,
            display_name: name.to_owned(),
            fingerprint: "ab".into(),
            trust: Trust::Verified,
            added_at_ms: 0,
        }
    }

    fn message(contact: UserKey, id: u8) -> MessageView {
        MessageView {
            id: [id; 16],
            contact,
            direction: brumm_core::api::Direction::Incoming,
            text: "hi".into(),
            attachment: None,
            reactions: Default::default(),
            sent_at_ms: u64::from(id),
            status: brumm_core::api::DeliveryStatus::Delivered,
        }
    }

    fn ready(
        state: &mut AppState,
        me: UserKey,
        contacts: Vec<ContactView>,
        last_opened: Option<UserKey>,
    ) {
        state.apply(
            Event::Ready {
                profile: Some(ProfileView {
                    user_key: me,
                    display_name: "Ada".into(),
                    fingerprint: "cd".into(),
                }),
                contacts,
                previews: Vec::new(),
                last_opened,
            },
            &egui::Context::default(),
        );
    }

    #[test]
    fn ready_reopens_saved_chat_without_writing_it_again() {
        let mut state = AppState::default();
        let me = [1; 32];
        let bob = [2; 32];
        ready(&mut state, me, vec![contact(bob, "Bob")], Some(bob));
        assert_eq!(state.selected, Some(bob));
        assert_eq!(state.stale_conversations, vec![bob]);
        assert_eq!(state.remember_open, None);
    }

    #[test]
    fn bulk_change_keeps_the_open_chat_until_it_is_reloaded() {
        let mut state = AppState::default();
        let (me, bob, carol) = ([1; 32], [2; 32], [3; 32]);
        ready(
            &mut state,
            me,
            vec![contact(bob, "Bob"), contact(carol, "Carol")],
            Some(bob),
        );
        state.stale_conversations.clear();
        let ctx = egui::Context::default();
        for chat in [bob, carol] {
            state.apply(
                Event::Conversation {
                    contact: chat,
                    messages: vec![message(chat, 1)],
                    has_older: false,
                },
                &ctx,
            );
        }

        state.apply(Event::ConversationsChanged(vec![bob, carol]), &ctx);
        assert_eq!(
            state.conversations[&bob].len(),
            1,
            "open chat stays visible"
        );
        assert!(!state.conversations.contains_key(&carol));
        assert_eq!(state.stale_conversations, vec![bob]);
        assert!(state.has_messages(&carol), "its preview still counts");
    }

    #[test]
    fn older_pages_prepend_once_and_reloads_keep_the_loaded_extent() {
        let mut state = AppState::default();
        let (me, bob) = ([1; 32], [2; 32]);
        ready(&mut state, me, vec![contact(bob, "Bob")], Some(bob));
        let ctx = egui::Context::default();
        state.apply(
            Event::Conversation {
                contact: bob,
                messages: vec![message(bob, 3), message(bob, 4)],
                has_older: true,
            },
            &ctx,
        );
        assert!(matches!(
            state.older_command(bob, false),
            Some(Command::LoadOlder { before, limit: Some(CONVERSATION_PAGE), .. }) if before == [3; 16]
        ));

        // A duplicate (id 3) from a racing reload is not inserted twice.
        state.apply(
            Event::OlderMessages {
                contact: bob,
                messages: vec![message(bob, 1), message(bob, 2), message(bob, 3)],
                has_older: false,
            },
            &ctx,
        );
        let ids: Vec<u8> = state.conversations[&bob].iter().map(|m| m.id[0]).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        assert!(state.older_command(bob, false).is_none());
        assert!(matches!(
            state.load_command(bob),
            Command::LoadConversation {
                limit: Some(CONVERSATION_PAGE),
                ..
            }
        ));
    }

    #[test]
    fn ready_reopens_notes_and_skips_unknown_keys() {
        let mut state = AppState::default();
        let me = [1; 32];
        ready(&mut state, me, Vec::new(), Some(me));
        assert_eq!(state.selected, Some(me));
        assert!(state.is_notes(&me));

        let mut state = AppState::default();
        ready(&mut state, me, Vec::new(), Some([9; 32]));
        assert_eq!(state.selected, None);
        assert!(state.stale_conversations.is_empty());
    }

    #[test]
    fn a_large_photo_keeps_only_the_preview() {
        let pixels = image::RgbImage::from_pixel(2000, 1000, image::Rgb([40, 90, 140]));
        let mut png = Vec::new();
        pixels
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode png");
        let source_len = png.len();
        let mut state = AppState::default();
        let contact = [3; 32];
        let id = [4; 16];
        let ctx = egui::Context::default();
        state.apply(
            Event::Attachment {
                contact,
                id,
                data: Arc::new(png),
            },
            &ctx,
        );
        assert!(state.attachments.is_empty());
        let job = state.take_pending_preview().expect("queued");
        assert_eq!(job.bytes.len(), source_len);
        let prepared = crate::ui::files::prepare_chat_image(job.bytes.as_slice(), job.max_edge)
            .expect("image");
        let preview = prepared.preview.expect("downscaled");
        assert!(preview.len() < source_len);
        state.install_attachment(
            (contact, id),
            LoadedAttachment {
                bytes: Arc::from(preview),
                image_size: Some(prepared.size),
            },
            &ctx,
        );
        let loaded = state.attachments.get(&(contact, id)).expect("cached");
        assert_eq!(loaded.image_size, Some((2000, 1000)));
        assert!(loaded.bytes.len() < 64 * 1024);
        assert!(loaded.bytes.len() < source_len);
    }
}
