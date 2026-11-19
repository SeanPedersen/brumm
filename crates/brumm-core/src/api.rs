//! Public boundary between the core and any UI: commands in, events out.
//! Views are plain data so a UI can mirror state without touching storage or networking.

use std::{path::PathBuf, sync::Arc};

use tokio::sync::mpsc;

use crate::{identity::UserKey, wire::MessageId};

#[derive(Debug, Clone)]
pub enum Command {
    CreateIdentity {
        display_name: String,
    },
    RestoreIdentity {
        phrase: String,
        display_name: String,
    },
    /// Fresh install: become a new device of an existing identity (shows a link ticket).
    StartLinking,
    CreateInvite,
    AcceptInvite {
        link: String,
    },
    /// Add the device that shows `link` to our identity.
    LinkDevice {
        link: String,
    },
    RemoveDevice {
        device_id: DeviceId,
    },
    LoadConversation {
        contact: UserKey,
    },
    /// Remembers which chat was open on this device. A single meta-row write; it does not load history.
    RememberOpenChat {
        contact: UserKey,
    },
    SendText {
        contact: UserKey,
        text: String,
    },
    SendFile {
        contact: UserKey,
        name: String,
        data: Vec<u8>,
    },
    /// `for_everyone` additionally asks the contact's devices to remove our own messages.
    DeleteMessages {
        contact: UserKey,
        ids: Vec<MessageId>,
        for_everyone: bool,
    },
    /// Replaces our profile photo (empty `image` removes it) and shares it with contacts.
    SetAvatar {
        image: Vec<u8>,
    },
    /// Fetches a file message so the UI can show a preview. A stored preview is returned when
    /// it was built for at least `max_edge` physical pixels; otherwise the original bytes are
    /// sent and the UI scales them down off the UI thread.
    LoadAttachment {
        contact: UserKey,
        id: MessageId,
        max_edge: u32,
    },
    /// Remembers a bubble-sized preview so the next open does not decode the original again.
    StoreImagePreview {
        contact: UserKey,
        id: MessageId,
        width: u32,
        height: u32,
        max_edge: u32,
        bytes: Vec<u8>,
    },
    /// Writes a file message to `path` from the database. The bytes are dropped after the write.
    SaveAttachment {
        contact: UserKey,
        id: MessageId,
        path: PathBuf,
    },
    /// Sets our reaction to a message (empty `emoji` removes it).
    React {
        contact: UserKey,
        id: MessageId,
        emoji: String,
    },
}

#[derive(Debug, Clone)]
pub enum Event {
    /// First event after start. `profile` is `None` until an identity exists.
    Ready {
        profile: Option<ProfileView>,
        contacts: Vec<ContactView>,
        /// Latest message per conversation, for chat-list ordering and previews; full
        /// conversations are loaded on demand with [`Command::LoadConversation`].
        previews: Vec<ConversationPreview>,
        /// Chat this device had open last time. One meta-row read; the history itself is not loaded yet.
        last_opened: Option<UserKey>,
    },
    /// `seed_phrase` is set only for brand-new identities (to show once for backup).
    IdentityCreated {
        profile: ProfileView,
        seed_phrase: Option<String>,
    },
    /// Shown on a device waiting to be linked.
    LinkTicket {
        link: String,
    },
    Devices(Vec<DeviceView>),
    /// Another device of ours removed this one; it no longer sends or receives.
    DeviceRevoked,
    InviteCreated {
        link: String,
    },
    ContactAdded(ContactView),
    Conversation {
        contact: UserKey,
        messages: Vec<MessageView>,
    },
    /// The last message of a conversation changed (e.g. after a bulk delete or history sync).
    PreviewChanged {
        contact: UserKey,
        last: Option<MessageView>,
    },
    MessageAdded(MessageView),
    /// Bulk change (history sync): reload these conversations if shown.
    ConversationsChanged(Vec<UserKey>),
    MessageStatusChanged {
        contact: UserKey,
        id: MessageId,
        status: DeliveryStatus,
    },
    MessagesDeleted {
        contact: UserKey,
        ids: Vec<MessageId>,
    },
    /// A profile photo changed (own or a contact's); `None` means it was removed.
    AvatarChanged {
        user: UserKey,
        image: Option<Vec<u8>>,
    },
    Attachment {
        contact: UserKey,
        id: MessageId,
        data: Arc<Vec<u8>>,
    },
    /// Bubble preview already encoded. The UI shows it without decoding the original.
    ImagePreview {
        contact: UserKey,
        id: MessageId,
        /// Source pixel size, used for the bubble's aspect ratio.
        width: u32,
        height: u32,
        bytes: Arc<Vec<u8>>,
    },
    ReactionsChanged {
        contact: UserKey,
        id: MessageId,
        reactions: ReactionsView,
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
    /// Empty for file messages.
    pub text: String,
    pub attachment: Option<AttachmentView>,
    pub reactions: ReactionsView,
    pub sent_at_ms: u64,
    pub status: DeliveryStatus,
}

/// Latest message of one conversation, used for the chat list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationPreview {
    pub contact: UserKey,
    pub last: Option<MessageView>,
}

/// Who reacted with what; each user has at most one reaction per message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReactionsView {
    pub mine: Option<String>,
    pub theirs: Option<String>,
}

/// Metadata of a file message. A preview is fetched with `Command::LoadAttachment`.
/// Saving the original uses `Command::SaveAttachment`, which reads the database and does not
/// keep the bytes.
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
