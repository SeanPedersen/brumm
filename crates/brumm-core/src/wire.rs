//! Wire protocol `brumm/2`: request/response frames exchanged over one QUIC bi-stream each,
//! serialized with postcard. Everything except acks and invite secrets travels inside Olm
//! ciphertext ([`Sealed`]). See SPEC.md §6.1.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::crypto::Sealed;

pub const ALPN: &[u8] = b"brumm/2";
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_TEXT_BYTES: usize = 16 * 1024;

pub type MessageId = [u8; 16];
pub type InviteSecret = [u8; 16];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    /// Pre-key message opening the session; plaintext is the sender's `Profile`.
    Hello {
        invite_secret: InviteSecret,
        sealed: Sealed,
    },
    /// Plaintext is a [`Payload`].
    Sealed(Sealed),
}

/// Plaintext inside [`Frame::Sealed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Payload {
    Chat(ChatMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Reply {
    /// Plaintext is the responder's `Profile`.
    HelloAck { sealed: Sealed },
    Ack { id: MessageId },
    Rejected(RejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    UnknownInvite,
    InvalidProfile,
    UnknownSender,
    InvalidMessage,
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
}

impl Body {
    pub fn is_valid(&self) -> bool {
        match self {
            Body::Text(text) => !text.trim().is_empty() && text.len() <= MAX_TEXT_BYTES,
        }
    }
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_stdvec(value).expect("in-memory serialization cannot fail")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}
