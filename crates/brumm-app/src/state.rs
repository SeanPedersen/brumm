//! UI-side mirror of core state, updated only by applying core [`Event`]s.

use std::collections::HashMap;

use brumm_core::{
    api::{ContactView, Event, MessageView, ProfileView},
    identity::UserKey,
};

const MAX_VISIBLE_ERRORS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Onboarding,
    Main,
}

#[derive(Default)]
pub struct AppState {
    pub ready: bool,
    pub profile: Option<ProfileView>,
    pub contacts: Vec<ContactView>,
    pub selected: Option<UserKey>,
    pub conversations: HashMap<UserKey, Vec<MessageView>>,
    pub unread: HashMap<UserKey, usize>,
    /// Shown once right after identity creation so the user can write it down.
    pub seed_phrase_to_show: Option<String>,
    pub invite_link: Option<String>,
    pub connecting: bool,
    pub errors: Vec<String>,
}

impl AppState {
    pub fn phase(&self) -> Phase {
        match (self.ready, &self.profile) {
            (false, _) => Phase::Starting,
            (true, None) => Phase::Onboarding,
            (true, Some(_)) => Phase::Main,
        }
    }

    pub fn contact(&self, key: &UserKey) -> Option<&ContactView> {
        self.contacts.iter().find(|contact| &contact.user_key == key)
    }

    pub fn select(&mut self, key: UserKey) {
        self.selected = Some(key);
        self.unread.remove(&key);
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Ready { profile, contacts } => {
                self.ready = true;
                self.profile = profile;
                self.contacts = contacts;
            }
            Event::IdentityCreated { profile, seed_phrase } => {
                self.profile = Some(profile);
                self.seed_phrase_to_show = Some(seed_phrase);
            }
            Event::InviteCreated { link } => self.invite_link = Some(link),
            Event::ContactAdded(contact) => {
                self.connecting = false;
                let key = contact.user_key;
                match self.contacts.iter_mut().find(|known| known.user_key == key) {
                    Some(known) => *known = contact,
                    None => self.contacts.push(contact),
                }
                self.contacts.sort_by_key(|contact| contact.display_name.to_lowercase());
                self.select(key);
            }
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
            Event::Error(error) => {
                self.connecting = false;
                self.errors.push(error);
                let overflow = self.errors.len().saturating_sub(MAX_VISIBLE_ERRORS);
                self.errors.drain(..overflow);
            }
        }
    }
}
