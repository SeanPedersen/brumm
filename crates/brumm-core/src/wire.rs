//! Wire protocol `brumm/3`: request/response frames exchanged over one QUIC bi-stream each,
//! serialized with postcard. Everything except acks and one-time secrets travels inside Olm
//! ciphertext ([`Sealed`]); the plaintext of `Frame::Sealed` is an [`Envelope`]. SPEC.md §6.1.

use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    crypto::Sealed,
    identity::{DeviceList, Profile, UserKey},
};

pub const ALPN: &[u8] = b"brumm/3";
/// Largest sealed frame. Sized for a maximal file plus Olm and framing overhead.
pub const MAX_FRAME_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// Files travel inline in one sealed envelope (larger transfers wait for iroh-blobs, M4).
/// Anything above the mailbox letter limit is only delivered while both devices are online.
pub const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_FILE_NAME_CHARS: usize = 255;
/// Small enough to also fit into a Nostr mailbox letter.
pub const MAX_AVATAR_BYTES: usize = 24 * 1024;
pub const MAX_DELETE_IDS: usize = 256;
pub const MAX_REACTION_BYTES: usize = 32;
/// Plaintext budget per history chunk, leaving room for Olm + framing overhead.
pub const HISTORY_CHUNK_BYTES: usize = 32 * 1024;

pub type MessageId = [u8; 16];
pub type EnvelopeId = [u8; 16];
pub type InviteSecret = [u8; 16];
pub type LinkSecret = [u8; 16];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    /// Pre-key message opening a contact session; plaintext is the sender's [`Profile`].
    Hello {
        invite_secret: InviteSecret,
        sealed: Sealed,
    },
    /// Pre-key message to a device being linked; plaintext is a [`crate::link::LinkBundle`].
    Link {
        link_secret: LinkSecret,
        sealed: Sealed,
    },
    /// Plaintext is an [`Envelope`].
    Sealed(Sealed),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Reply {
    /// Plaintext is the responder's [`Profile`].
    HelloAck {
        sealed: Sealed,
    },
    /// Acknowledges an envelope (or a link, with the link secret as id).
    Ack {
        id: EnvelopeId,
    },
    Rejected(RejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    UnknownInvite,
    InvalidProfile,
    UnknownSender,
    InvalidMessage,
}

/// Unit of reliable delivery to one device; acked by `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: EnvelopeId,
    pub payload: Payload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Payload {
    /// A message from the sender's user to the recipient's user.
    Chat(ChatMessage),
    /// Own-device sync: "I sent `message` to `contact`".
    SentTranscript {
        contact: UserKey,
        message: ChatMessage,
    },
    /// The sender's user published a new device list.
    DeviceList(DeviceList),
    /// Own-device sync: a contact was added or updated.
    ContactSync(ContactRecord),
    /// Own-device sync: a chunk of message history for a freshly linked device.
    History(Vec<HistoryItem>),
    /// "I processed these envelopes" — the ack for envelopes that arrived via the mailbox.
    Receipt(Vec<EnvelopeId>),
    /// The sender's user deleted these messages they had sent ("delete for everyone").
    Delete(Vec<MessageId>),
    /// Own-device sync: remove these messages from the conversation with `contact`.
    DeleteSync {
        contact: UserKey,
        ids: Vec<MessageId>,
    },
    /// A profile photo. From a contact it must describe the contact itself; own devices also
    /// use it to sync contacts' photos to a freshly linked device.
    Avatar(Avatar),
    /// The sender's user reacts to one of the messages in this conversation.
    Reaction(Reaction),
    /// Own-device sync: "I reacted in the conversation with `contact`".
    ReactionSync {
        contact: UserKey,
        reaction: Reaction,
    },
    /// Own-device sync, sent before the [`Payload::History`] chunks for a freshly linked
    /// device: how many history items follow per conversation, so it can show progress.
    HistoryManifest(Vec<HistoryCount>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCount {
    pub contact: UserKey,
    pub messages: u32,
}

/// One user's reaction to a message; at most one per user and message (last write wins).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub message_id: MessageId,
    /// A single emoji; empty removes the reaction.
    pub emoji: String,
    pub at_ms: u64,
}

impl Reaction {
    pub fn is_valid(&self) -> bool {
        self.emoji.len() <= MAX_REACTION_BYTES
            && !self
                .emoji
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Avatar {
    pub subject: UserKey,
    /// Encoded image (PNG/JPEG/WebP); empty removes the photo.
    pub image: Vec<u8>,
}

impl Avatar {
    pub fn is_valid(&self) -> bool {
        self.image.len() <= MAX_AVATAR_BYTES
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: MessageId,
    pub sent_at_ms: u64,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Body {
    Text(String),
    File { name: String, data: Vec<u8> },
}

impl Body {
    pub fn is_valid(&self) -> bool {
        match self {
            Body::Text(text) => !text.trim().is_empty() && text.len() <= MAX_TEXT_BYTES,
            Body::File { name, data } => {
                !name.trim().is_empty()
                    && name.chars().count() <= MAX_FILE_NAME_CHARS
                    && !data.is_empty()
                    && data.len() <= MAX_FILE_BYTES
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactRecord {
    pub profile: Profile,
    pub added_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub contact: UserKey,
    pub outgoing: bool,
    pub message: ChatMessage,
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_stdvec(value).expect("in-memory serialization cannot fail")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

/// `<prefix><lowercase base32(postcard(value))>`, used for invite and link URIs / QR codes.
pub fn to_uri<T: Serialize>(prefix: &str, value: &T) -> String {
    format!(
        "{prefix}{}",
        BASE32_NOPAD.encode(&encode(value)).to_ascii_lowercase()
    )
}

#[derive(Debug, thiserror::Error)]
pub enum UriError {
    #[error("this is not the expected kind of brumm link")]
    WrongPrefix,
    #[error("link is corrupted")]
    Malformed,
}

pub fn from_uri<T: DeserializeOwned>(prefix: &str, uri: &str) -> Result<T, UriError> {
    let payload = uri
        .trim()
        .strip_prefix(prefix)
        .ok_or(UriError::WrongPrefix)?;
    let bytes = BASE32_NOPAD
        .decode(payload.to_ascii_uppercase().as_bytes())
        .map_err(|_| UriError::Malformed)?;
    decode(&bytes).map_err(|_| UriError::Malformed)
}

/// Splits history into chunks whose encoded size stays within [`HISTORY_CHUNK_BYTES`].
pub fn chunk_history(items: Vec<HistoryItem>) -> Vec<Vec<HistoryItem>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_bytes = 0;
    for item in items {
        let item_bytes = encode(&item).len();
        if !current.is_empty() && current_bytes + item_bytes > HISTORY_CHUNK_BYTES {
            chunks.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current_bytes += item_bytes;
        current.push(item);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_chunks_respect_budget() {
        let item = |index: u8| HistoryItem {
            contact: [index; 32],
            outgoing: true,
            message: ChatMessage {
                id: [index; 16],
                sent_at_ms: 1,
                body: Body::Text("x".repeat(10_000)),
            },
        };
        let chunks = chunk_history((0..10).map(item).collect());
        assert!(chunks.len() > 1);
        assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), 10);
        assert!(
            chunks
                .iter()
                .all(|chunk| encode(chunk).len() <= HISTORY_CHUNK_BYTES + 16)
        );
    }
}
