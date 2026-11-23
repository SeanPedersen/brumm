//! Stable JSON views for headless clients (CLI `--json`, the daemon protocol, Python).
//!
//! Core types keep changing; these do not, unless the protocol version is bumped. Keys and ids
//! are lowercase unpadded base32. A room id is `d:<user key>` for a 1:1 chat (our own key: the
//! Notes chat) and `g:<group id>` for a group. People name rooms by contact name, group name
//! or id ([`resolve_room`]); a name that fits several rooms is an error. `me` names the first
//! contact (whoever paired a bot), so `echo done | brumm-cli send me` reaches its owner.

use anyhow::{Context, Result, bail};
use brumm_core::api::{
    ContactView, ConversationId, DeliveryStatus, Direction as CoreDirection, Event,
    GroupInvitationView, GroupStatus, GroupView, InvitationStage, MessageView, ProfileView, Role,
};

use crate::mirror::Snapshot;
use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Serialize};

const DIRECT_PREFIX: &str = "d:";
const GROUP_PREFIX: &str = "g:";
/// Shown name and query word of the chat with ourselves.
pub const NOTES_NAME: &str = "Notes";
/// Query word for the first contact.
pub const OWNER_ALIAS: &str = "me";

/// Lowercase unpadded base32.
pub fn encode_id(bytes: &[u8]) -> String {
    BASE32_NOPAD.encode(bytes).to_ascii_lowercase()
}

pub fn decode_id<const LEN: usize>(text: &str) -> Result<[u8; LEN]> {
    let bytes = BASE32_NOPAD
        .decode(text.trim().to_ascii_uppercase().as_bytes())
        .context("not a base32 id")?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| anyhow::anyhow!("id has {} bytes, expected {LEN}", bytes.len()))
}

pub fn room_id(conversation: &ConversationId) -> String {
    match conversation {
        ConversationId::Direct(key) => format!("{DIRECT_PREFIX}{}", encode_id(key)),
        ConversationId::Group(id) => format!("{GROUP_PREFIX}{}", encode_id(id)),
    }
}

/// `None` if `text` is not in room id form (it may still be a name).
pub fn parse_room_id(text: &str) -> Option<Result<ConversationId>> {
    if let Some(key) = text.strip_prefix(DIRECT_PREFIX) {
        return Some(decode_id(key).map(ConversationId::Direct));
    }
    let id = text.strip_prefix(GROUP_PREFIX)?;
    Some(decode_id(id).map(ConversationId::Group))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomKind {
    Direct,
    Notes,
    Group,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    pub id: String,
    pub kind: RoomKind,
    pub name: String,
    /// Groups only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    pub members: u32,
    /// `None` once we left or were removed.
    pub role: Option<GroupRole>,
    pub active: bool,
    pub announcement: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupRole {
    Owner,
    Admin,
    Member,
}

impl From<Role> for GroupRole {
    fn from(role: Role) -> Self {
        match role {
            Role::Owner => Self::Owner,
            Role::Admin => Self::Admin,
            Role::Member => Self::Member,
        }
    }
}

impl From<&GroupView> for Room {
    fn from(group: &GroupView) -> Self {
        Self {
            id: room_id(&ConversationId::Group(group.id)),
            kind: RoomKind::Group,
            name: group.name.clone(),
            group: Some(GroupInfo {
                members: group.member_count,
                role: group.my_role.map(GroupRole::from),
                active: group.status == GroupStatus::Active,
                announcement: group.announcement,
            }),
        }
    }
}

impl From<&ContactView> for Room {
    fn from(contact: &ContactView) -> Self {
        Self {
            id: room_id(&ConversationId::Direct(contact.user_key)),
            kind: RoomKind::Direct,
            name: contact.display_name.clone(),
            group: None,
        }
    }
}

/// Every room we can name: Notes (with an identity), contacts, then groups.
pub fn rooms(me: Option<&ProfileView>, contacts: &[ContactView], groups: &[GroupView]) -> Vec<Room> {
    let notes = me.map(|me| Room {
        id: room_id(&ConversationId::Direct(me.user_key)),
        kind: RoomKind::Notes,
        name: NOTES_NAME.to_owned(),
        group: None,
    });
    let direct = contacts.iter().map(Room::from);
    let group = groups.iter().map(Room::from);
    notes.into_iter().chain(direct).chain(group).collect()
}

/// [`resolve_room`] over a profile's rooms, plus [`OWNER_ALIAS`] for the first contact.
pub fn resolve_in(snapshot: &Snapshot, query: &str) -> Result<ConversationId, RoomError> {
    if query.trim().eq_ignore_ascii_case(OWNER_ALIAS) {
        return snapshot
            .first_contact()
            .map(|owner| ConversationId::Direct(owner.user_key))
            .ok_or_else(|| RoomError::NotFound(OWNER_ALIAS.to_owned()));
    }
    let rooms = rooms(snapshot.profile.as_ref(), &snapshot.contacts, &snapshot.groups);
    resolve_room(query, &rooms)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RoomError {
    #[error("invalid room id: {0}")]
    InvalidId(String),
    #[error("no room is called {0:?}")]
    NotFound(String),
    #[error("{query:?} could mean several rooms, use an id: {}", .candidates.join(", "))]
    Ambiguous { query: String, candidates: Vec<String> },
}

/// A room id (`d:…` / `g:…`) or the name of exactly one room (case does not matter).
pub fn resolve_room(query: &str, rooms: &[Room]) -> Result<ConversationId, RoomError> {
    let query = query.trim();
    if let Some(parsed) = parse_room_id(query) {
        return parsed.map_err(|err| RoomError::InvalidId(format!("{query}: {err:#}")));
    }
    let matches: Vec<&Room> = rooms
        .iter()
        .filter(|room| room.name.trim().eq_ignore_ascii_case(query))
        .collect();
    match matches.as_slice() {
        [] => Err(RoomError::NotFound(query.to_owned())),
        [room] => Ok(parse_room_id(&room.id)
            .expect("rooms carry room ids")
            .expect("rooms carry valid ids")),
        several => Err(RoomError::Ambiguous {
            query: query.to_owned(),
            candidates: several.iter().map(|room| room.id.clone()).collect(),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    pub user: String,
    pub name: String,
    pub fingerprint: String,
    pub room: String,
}

impl From<&ContactView> for Contact {
    fn from(contact: &ContactView) -> Self {
        Self {
            user: encode_id(&contact.user_key),
            name: contact.display_name.clone(),
            fingerprint: contact.fingerprint.clone(),
            room: room_id(&ConversationId::Direct(contact.user_key)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub user: String,
    pub name: String,
    pub fingerprint: String,
}

impl From<&ProfileView> for Identity {
    fn from(profile: &ProfileView) -> Self {
        Self {
            user: encode_id(&profile.user_key),
            name: profile.display_name.clone(),
            fingerprint: profile.fingerprint.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Sent,
    Delivered,
    Failed,
}

impl From<DeliveryStatus> for Status {
    fn from(status: DeliveryStatus) -> Self {
        match status {
            DeliveryStatus::Pending => Self::Pending,
            DeliveryStatus::Sent => Self::Sent,
            DeliveryStatus::Delivered => Self::Delivered,
            DeliveryStatus::Failed => Self::Failed,
        }
    }
}

impl From<Status> for DeliveryStatus {
    fn from(status: Status) -> Self {
        match status {
            Status::Pending => Self::Pending,
            Status::Sent => Self::Sent,
            Status::Delivered => Self::Delivered,
            Status::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    In,
    Out,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    pub user: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub room: String,
    pub direction: Direction,
    /// Empty for files.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<File>,
    pub sent_at_ms: u64,
    pub status: Status,
    /// Groups: who wrote an incoming message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<Sender>,
    /// Groups: a line about the group itself ("Bob joined").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub system: bool,
    /// In reads: this message's arrival number, a cursor that continues right after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
}

impl From<&MessageView> for Message {
    fn from(message: &MessageView) -> Self {
        Self {
            id: encode_id(&message.id),
            room: room_id(&message.conversation),
            direction: match message.direction {
                CoreDirection::Incoming => Direction::In,
                CoreDirection::Outgoing => Direction::Out,
            },
            text: message.text.clone(),
            file: message.attachment.as_ref().map(|file| File {
                name: file.name.clone(),
                size: file.size as u64,
            }),
            sent_at_ms: message.sent_at_ms,
            status: message.status.into(),
            sender: message.sender.as_ref().map(|sender| Sender {
                user: encode_id(&sender.user_key),
                name: sender.display_name.clone(),
            }),
            system: message.system,
            cursor: None,
        }
    }
}

/// Messages after a read cursor; `cursor` continues the read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub messages: Vec<Message>,
    pub cursor: u64,
    pub has_more: bool,
}

impl From<&crate::client::Page> for Page {
    fn from(page: &crate::client::Page) -> Self {
        Self {
            messages: page
                .messages
                .iter()
                .zip(&page.arrivals)
                .map(|(message, arrival)| Message {
                    cursor: Some(*arrival),
                    ..Message::from(message)
                })
                .collect(),
            cursor: page.cursor,
            has_more: page.has_more,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Open,
    Accepted,
    Loading,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invitation {
    /// The group's room id (usable once joined, and to accept or decline).
    pub room: String,
    pub name: String,
    pub inviter: String,
    pub inviter_name: String,
    pub members: u32,
    pub stage: Stage,
}

impl From<&GroupInvitationView> for Invitation {
    fn from(invitation: &GroupInvitationView) -> Self {
        Self {
            room: room_id(&ConversationId::Group(invitation.group)),
            name: invitation.name.clone(),
            inviter: encode_id(&invitation.inviter),
            inviter_name: invitation.inviter_name.clone(),
            members: invitation.member_count,
            stage: match invitation.stage {
                InvitationStage::Open => Stage::Open,
                InvitationStage::Accepted => Stage::Accepted,
                InvitationStage::Loading => Stage::Loading,
            },
        }
    }
}

/// A live event for subscribers (`event` notifications of the daemon, `brumm-cli listen`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Notice {
    Message { message: Message },
    Status { room: String, id: String, status: Status },
    Deleted { room: String, ids: Vec<String> },
    Invitation { invitations: Vec<Invitation> },
    /// A contact was added or a group changed.
    Room { room: Room },
    /// Many messages changed at once (history sync): read these rooms again.
    Changed { rooms: Vec<String> },
    /// Events were dropped because the subscriber was too slow: read again from the cursor.
    Lagged,
}

impl Notice {
    /// The subscriber-facing form of a core event, if it has one.
    pub fn from_event(event: &Event) -> Option<Self> {
        Some(match event {
            Event::MessageAdded(message) => Self::Message {
                message: message.into(),
            },
            Event::MessageStatusChanged {
                conversation,
                id,
                status,
            } => Self::Status {
                room: room_id(conversation),
                id: encode_id(id),
                status: (*status).into(),
            },
            Event::MessagesDeleted { conversation, ids } => Self::Deleted {
                room: room_id(conversation),
                ids: ids.iter().map(|id| encode_id(id)).collect(),
            },
            Event::GroupInvitations(invitations) => Self::Invitation {
                invitations: invitations.iter().map(Invitation::from).collect(),
            },
            Event::ContactAdded(contact) => Self::Room {
                room: contact.into(),
            },
            Event::GroupChanged(group) => Self::Room {
                room: group.into(),
            },
            Event::ConversationsChanged(conversations) => Self::Changed {
                rooms: conversations.iter().map(room_id).collect(),
            },
            _ => return None,
        })
    }

    /// Whether a subscriber to `rooms` (all if `None`) gets this notice. Room-less notices
    /// (invitations, lagged) go to everyone.
    pub fn concerns(&self, rooms: Option<&[String]>) -> bool {
        let Some(rooms) = rooms else {
            return true;
        };
        match self {
            Self::Message { message } => rooms.contains(&message.room),
            Self::Status { room, .. } | Self::Deleted { room, .. } => rooms.contains(room),
            Self::Room { room } => rooms.contains(&room.id),
            Self::Changed { rooms: changed } => changed.iter().any(|room| rooms.contains(room)),
            Self::Invitation { .. } | Self::Lagged => true,
        }
    }
}

/// A message id given by a caller (base32 of 16 bytes).
pub fn parse_message_id(text: &str) -> Result<brumm_core::wire::MessageId> {
    if text.trim().is_empty() {
        bail!("empty message id");
    }
    decode_id(text)
}

#[cfg(test)]
mod tests {
    use brumm_core::api::{GroupStatus, ReactionsView, Trust};

    use super::*;

    fn profile(byte: u8, name: &str) -> ProfileView {
        ProfileView {
            user_key: [byte; 32],
            display_name: name.into(),
            fingerprint: "FP".into(),
        }
    }

    fn contact(byte: u8, name: &str) -> ContactView {
        ContactView {
            user_key: [byte; 32],
            display_name: name.into(),
            fingerprint: "FP".into(),
            trust: Trust::Verified,
            added_at_ms: 1,
        }
    }

    fn group(byte: u8, name: &str) -> GroupView {
        GroupView {
            id: [byte; 32],
            name: name.into(),
            status: GroupStatus::Active,
            my_role: None,
            announcement: false,
            history_sharing: false,
            member_count: 2,
            owner: None,
        }
    }

    #[test]
    fn ids_round_trip() {
        for conversation in [ConversationId::Direct([7; 32]), ConversationId::Group([9; 32])] {
            let id = room_id(&conversation);
            assert_eq!(parse_room_id(&id).unwrap().unwrap(), conversation);
            let (prefix, key) = id.split_at(DIRECT_PREFIX.len());
            let shouted = format!("{prefix}{}", key.to_uppercase());
            assert_eq!(parse_room_id(&shouted).unwrap().unwrap(), conversation);
        }
        assert!(parse_room_id("d:tooshort").unwrap().is_err());
        assert!(parse_room_id("alice").is_none());
        let message: [u8; 16] = [3; 16];
        assert_eq!(parse_message_id(&encode_id(&message)).unwrap(), message);
        assert!(parse_message_id("").is_err());
    }

    #[test]
    fn rooms_resolve_by_name_or_id_and_refuse_ambiguity() {
        let me = profile(1, "Me");
        let rooms = rooms(
            Some(&me),
            &[contact(2, "Alice"), contact(3, "Bob"), contact(4, "ops")],
            &[group(5, "Ops"), group(6, "Agents")],
        );
        assert_eq!(rooms[0].kind, RoomKind::Notes);
        assert_eq!(resolve_room("notes", &rooms), Ok(ConversationId::Direct([1; 32])));
        assert_eq!(resolve_room(" alice ", &rooms), Ok(ConversationId::Direct([2; 32])));
        assert_eq!(resolve_room("Agents", &rooms), Ok(ConversationId::Group([6; 32])));
        let group_id = room_id(&ConversationId::Group([5; 32]));
        assert_eq!(resolve_room(&group_id, &rooms), Ok(ConversationId::Group([5; 32])));
        assert!(matches!(resolve_room("OPS", &rooms), Err(RoomError::Ambiguous { candidates, .. }) if candidates.len() == 2));
        assert_eq!(resolve_room("carol", &rooms), Err(RoomError::NotFound("carol".into())));
        assert!(matches!(resolve_room("g:xyz", &rooms), Err(RoomError::InvalidId(_))));
    }

    #[test]
    fn me_is_the_first_contact() {
        let mut snapshot = Snapshot {
            profile: Some(profile(1, "Bot")),
            contacts: vec![contact(3, "Later"), contact(2, "Owner")],
            ..Snapshot::default()
        };
        snapshot.contacts[0].added_at_ms = 50;
        assert_eq!(resolve_in(&snapshot, "ME"), Ok(ConversationId::Direct([2; 32])));
        assert_eq!(resolve_in(&snapshot, "later"), Ok(ConversationId::Direct([3; 32])));
        assert!(resolve_in(&Snapshot::default(), "me").is_err());
    }

    #[test]
    fn notices_filter_by_room() {
        let room = room_id(&ConversationId::Direct([2; 32]));
        let notice = Notice::from_event(&Event::MessagesDeleted {
            conversation: ConversationId::Direct([2; 32]),
            ids: vec![[1; 16]],
        })
        .unwrap();
        assert!(notice.concerns(None));
        assert!(notice.concerns(Some(std::slice::from_ref(&room))));
        assert!(!notice.concerns(Some(&["g:x".to_owned()])));
        assert!(Notice::Lagged.concerns(Some(&[])));
        let json = serde_json::to_value(&notice).unwrap();
        assert_eq!(json["type"], "deleted");
        assert!(Notice::from_event(&Event::LoggedOut).is_none());
    }

    #[test]
    fn message_json_is_stable() {
        let view = MessageView {
            id: [0; 16],
            conversation: ConversationId::Direct([0; 32]),
            direction: CoreDirection::Outgoing,
            text: "hi".into(),
            attachment: None,
            reactions: ReactionsView::default(),
            sent_at_ms: 5,
            status: DeliveryStatus::Delivered,
            sender: None,
            system: false,
        };
        let json = serde_json::to_value(Message::from(&view)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
                "room": format!("d:{}", "a".repeat(52)),
                "direction": "out",
                "text": "hi",
                "sent_at_ms": 5,
                "status": "delivered",
            })
        );
    }
}
