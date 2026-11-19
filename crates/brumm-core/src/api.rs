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
    SendFile { contact: UserKey, name: String, data: Vec<u8> },
    /// `for_everyone` additionally asks the contact's devices to remove our own messages.
    DeleteMessages { contact: UserKey, ids: Vec<MessageId>, for_everyone: bool },
    /// Replaces our profile photo (empty `image` removes it) and shares it with contacts.
    SetAvatar { image: Vec<u8> },
    /// Fetches the bytes of a file message; answered with [`Event::Attachment`].
    LoadAttachment { contact: UserKey, id: MessageId },
    /// Sets our reaction to a message (empty `emoji` removes it).
    React { contact: UserKey, id: MessageId, emoji: String },
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
    MessagesDeleted { contact: UserKey, ids: Vec<MessageId> },
    /// A profile photo changed (own or a contact's); `None` means it was removed.
    AvatarChanged { user: UserKey, image: Option<Vec<u8>> },
    Attachment { contact: UserKey, id: MessageId, data: Arc<Vec<u8>> },
    ReactionsChanged { contact: UserKey, id: MessageId, reactions: ReactionsView },
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
    /// Empty for file messages.
    pub text: String,
    pub attachment: Option<AttachmentView>,
    pub reactions: ReactionsView,
    pub sent_at_ms: u64,
    pub status: DeliveryStatus,
}

/// Who reacted with what; each user has at most one reaction per message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReactionsView {
    pub mine: Option<String>,
    pub theirs: Option<String>,
}

/// Metadata of a file message; the bytes are fetched on demand with `Command::LoadAttachment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentView {
    pub name: String,
    pub size: usize,
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
