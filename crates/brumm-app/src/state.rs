//! UI-side mirror of core state, updated only by applying core [`Event`]s.

use std::{collections::HashMap, io::Cursor, sync::Arc};

use brumm_core::{
    api::{ContactView, DeviceView, Event, MessageView, ProfileView, Trust},
    identity::UserKey,
    wire::MessageId,
};

const MAX_VISIBLE_ERRORS: usize = 3;
const NOTES_NAME: &str = "Notes";

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

/// Bytes of a file message, fetched on demand, plus the pixel size if it is an image.
#[derive(Clone)]
pub struct LoadedAttachment {
    pub bytes: Arc<[u8]>,
    pub image_size: Option<(u32, u32)>,
}

#[derive(Default)]
pub struct AppState {
    pub avatars: HashMap<UserKey, AvatarImage>,
    pub attachments: HashMap<(UserKey, MessageId), LoadedAttachment>,
    pub ready: bool,
    pub profile: Option<ProfileView>,
    pub contacts: Vec<ContactView>,
    pub selected: Option<UserKey>,
    pub conversations: HashMap<UserKey, Vec<MessageView>>,
    pub unread: HashMap<UserKey, usize>,
    /// Shown once right after identity creation so the user can write it down.
    pub seed_phrase_to_show: Option<String>,
    pub invite_link: Option<String>,
    /// Shown while this fresh device waits to be linked.
    pub link_ticket: Option<String>,
    pub devices: Vec<DeviceView>,
    pub revoked: bool,
    /// Conversations changed in bulk; the app reloads them from the core.
    pub stale_conversations: Vec<UserKey>,
    pub connecting: bool,
    pub errors: Vec<String>,
}

impl AppState {
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
        self.profile.as_ref().is_some_and(|profile| &profile.user_key == key)
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
        self.stale_conversations.push(notes.user_key);
        self.contacts.push(notes);
    }

    pub fn contact(&self, key: &UserKey) -> Option<&ContactView> {
        self.contacts.iter().find(|contact| &contact.user_key == key)
    }

    pub fn select(&mut self, key: UserKey) {
        self.selected = Some(key);
        self.unread.remove(&key);
    }

    /// Adds or updates a contact; jumps to it only when the user just connected to it.
    fn upsert_contact(&mut self, contact: ContactView) {
        let key = contact.user_key;
        match self.contacts.iter_mut().find(|known| known.user_key == key) {
            Some(known) => *known = contact,
            None => self.contacts.push(contact),
        }
        self.contacts.sort_by_key(|contact| contact.display_name.to_lowercase());
        if std::mem::take(&mut self.connecting) {
            self.select(key);
        }
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Ready { profile, contacts } => {
                self.ready = true;
                self.profile = profile;
                // Chat-list previews and ordering need every conversation's latest message.
                self.stale_conversations.extend(contacts.iter().map(|contact| contact.user_key));
                self.contacts = contacts;
                self.ensure_notes_contact();
            }
            Event::IdentityCreated { profile, seed_phrase } => {
                self.profile = Some(profile);
                self.seed_phrase_to_show = seed_phrase;
                self.ensure_notes_contact();
                self.link_ticket = None;
            }
            Event::LinkTicket { link } => self.link_ticket = Some(link),
            Event::Devices(devices) => self.devices = devices,
            Event::DeviceRevoked => self.revoked = true,
            Event::InviteCreated { link } => self.invite_link = Some(link),
            Event::ContactAdded(contact) => self.upsert_contact(contact),
            Event::ConversationsChanged(contacts) => self.stale_conversations.extend(contacts),
            Event::Conversation { contact, messages } => {
                self.conversations.insert(contact, messages);
            }
            Event::MessageAdded(message) => {
                let contact = message.contact;
                if self.selected != Some(contact) {
                    *self.unread.entry(contact).or_default() += 1;
                }
                self.conversations.entry(contact).or_default().push(message);
            }
            Event::MessageStatusChanged { contact, id, status } => {
                let message = self
                    .conversations
                    .get_mut(&contact)
                    .and_then(|messages| messages.iter_mut().rev().find(|message| message.id == id));
                if let Some(message) = message {
                    message.status = status;
                }
            }
            Event::MessagesDeleted { contact, ids } => {
                if let Some(messages) = self.conversations.get_mut(&contact) {
                    messages.retain(|message| !ids.contains(&message.id));
                }
                for id in &ids {
                    self.attachments.remove(&(contact, *id));
                }
            }
            Event::ReactionsChanged { contact, id, reactions } => {
                let message = self
                    .conversations
                    .get_mut(&contact)
                    .and_then(|messages| messages.iter_mut().rev().find(|message| message.id == id));
                if let Some(message) = message {
                    message.reactions = reactions;
                }
            }
            Event::AvatarChanged { user, image } => match image {
                Some(bytes) => {
                    let uri = format!("bytes://avatar/{}-{}", hex_prefix(&user), checksum(&bytes));
                    self.avatars.insert(user, AvatarImage { uri, bytes: bytes.into() });
                }
                None => {
                    self.avatars.remove(&user);
                }
            },
            Event::Attachment { contact, id, data } => {
                let image_size = image::ImageReader::new(Cursor::new(data.as_slice()))
                    .with_guessed_format()
                    .ok()
                    .and_then(|reader| reader.into_dimensions().ok());
                let bytes: Arc<[u8]> = data.as_slice().into();
                self.attachments.insert((contact, id), LoadedAttachment { bytes, image_size });
            }
            Event::Error(error) => {
                self.connecting = false;
                self.errors.push(error);
                let overflow = self.errors.len().saturating_sub(MAX_VISIBLE_ERRORS);
                self.errors.drain(..overflow);
            }
        }
    }
}

const HEX_PREFIX_BYTES: usize = 4;

fn hex_prefix(key: &UserKey) -> String {
    key[..HEX_PREFIX_BYTES].iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Cheap content fingerprint (FNV-1a) so a changed photo gets a fresh image-cache uri.
fn checksum(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;
    bytes.iter().fold(FNV_OFFSET, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME))
}
