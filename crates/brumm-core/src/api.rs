//! Public boundary between the core and any UI: commands in, events out.
//! Views are plain data so a UI can mirror state without touching storage or networking.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::{identity::UserKey, wire::MessageId};

#[derive(Debug, Clone)]
pub enum Command {
    CreateIdentity { display_name: String },
    RestoreIdentity { phrase: String, display_name: String },
    /// Fresh install: become a new device of an existing identity (shows a link ticket).
    StartLinking,
    CreateInvite,
    AcceptInvite { link: String },
    /// Add the device that shows `link` to our identity.
    LinkDevice { link: String },
    RemoveDevice { device_id: DeviceId },
    LoadConversation { contact: UserKey },
    SendText { contact: UserKey, text: String },
}

#[derive(Debug, Clone)]
pub enum Event {
    /// First event after start. `profile` is `None` until an identity exists.
    Ready {
        profile: Option<ProfileView>,
        contacts: Vec<ContactView>,
    },
    /// `seed_phrase` is set only for brand-new identities (to show once for backup).
    IdentityCreated {
        profile: ProfileView,
        seed_phrase: Option<String>,
    },
    /// Shown on a device waiting to be linked.
    LinkTicket { link: String },
    Devices(Vec<DeviceView>),
    /// Another device of ours removed this one; it no longer sends or receives.
    DeviceRevoked,
    InviteCreated { link: String },
    ContactAdded(ContactView),
    Conversation {
        contact: UserKey,
        messages: Vec<MessageView>,
    },
    MessageAdded(MessageView),
    /// Bulk change (history sync): reload these conversations if shown.
    ConversationsChanged(Vec<UserKey>),
    MessageStatusChanged {
        contact: UserKey,
        id: MessageId,
        status: DeliveryStatus,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileView {
    pub user_key: UserKey,
    pub display_name: String,
    pub fingerprint: String,
}

pub type DeviceId = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceView {
    pub device_id: DeviceId,
    pub label: String,
    pub added_at_ms: u64,
    pub this_device: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactView {
    pub user_key: UserKey,
    pub display_name: String,
    pub fingerprint: String,
    pub trust: Trust,
    pub added_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub id: MessageId,
    pub contact: UserKey,
    pub direction: Direction,
    pub text: String,
    pub sent_at_ms: u64,
    pub status: DeliveryStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Keys exchanged in person or via invite link.
    Verified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    /// Not yet accepted anywhere.
    Pending,
    /// Stored in the recipient's Nostr mailbox (recipient offline).
    Sent,
    /// Acknowledged by at least one device of the recipient.
    Delivered,
    Failed,
}

/// Event channel plus a wake-up hook so immediate-mode UIs repaint when something arrives.
#[derive(Clone)]
pub struct EventSink {
    tx: mpsc::UnboundedSender<Event>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl EventSink {
    pub fn new(tx: mpsc::UnboundedSender<Event>, wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            tx,
            wake: Arc::new(wake),
        }
    }

    pub fn emit(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            (self.wake)();
        }
    }
}
