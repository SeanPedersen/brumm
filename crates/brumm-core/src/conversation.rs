//! Which conversation a message, reaction or preview belongs to: a 1:1 chat (or Notes, keyed by
//! our own user key) or a group (SPEC.md §14). Stored in SQLite as one tag byte followed by the
//! 32-byte key, so both kinds share the message tables.

use serde::{Deserialize, Serialize};

use crate::identity::UserKey;

/// 32 random bytes chosen by the group's creator (SPEC.md §14.2).
pub type GroupId = [u8; 32];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ConversationId {
    /// A contact's chat, or Notes when the key is our own.
    Direct(UserKey),
    Group(GroupId),
}

const DIRECT_TAG: u8 = 0;
const GROUP_TAG: u8 = 1;
const KEY_LEN: usize = 32;
/// Tag byte plus key.
pub const STORED_LEN: usize = 1 + KEY_LEN;

impl ConversationId {
    /// Database form: tag byte ‖ key.
    pub fn to_stored(&self) -> [u8; STORED_LEN] {
        let (tag, key) = match self {
            Self::Direct(key) => (DIRECT_TAG, key),
            Self::Group(key) => (GROUP_TAG, key),
        };
        let mut stored = [0; STORED_LEN];
        stored[0] = tag;
        stored[1..].copy_from_slice(key);
        stored
    }

    pub fn from_stored(bytes: &[u8]) -> Option<Self> {
        let (&tag, key) = bytes.split_first()?;
        let key: [u8; KEY_LEN] = key.try_into().ok()?;
        match tag {
            DIRECT_TAG => Some(Self::Direct(key)),
            GROUP_TAG => Some(Self::Group(key)),
            _ => None,
        }
    }

    /// The contact (or own key for Notes) of a 1:1 conversation.
    pub fn direct(&self) -> Option<UserKey> {
        match self {
            Self::Direct(key) => Some(*key),
            Self::Group(_) => None,
        }
    }

    pub fn is_direct(&self) -> bool {
        matches!(self, Self::Direct(_))
    }
}

/// Tag byte of [`ConversationId::Direct`] in the database, for the v9 migration.
pub(crate) const STORED_DIRECT_TAG: u8 = DIRECT_TAG;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_form_round_trips_and_rejects_garbage() {
        for id in [
            ConversationId::Direct([3; 32]),
            ConversationId::Group([4; 32]),
        ] {
            let stored = id.to_stored();
            assert_eq!(ConversationId::from_stored(&stored), Some(id));
        }
        assert_eq!(
            ConversationId::Direct([3; 32]).to_stored()[0],
            STORED_DIRECT_TAG
        );
        assert_eq!(ConversationId::from_stored(&[3; 32]), None);
        assert_eq!(ConversationId::from_stored(&[9; STORED_LEN]), None);
    }
}
