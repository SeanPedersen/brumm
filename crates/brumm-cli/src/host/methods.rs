//! The protocol's methods (v1, see `protocol.md`), one function each. The daemon and one-shot
//! CLI runs both come through [`call`], so both behave the same.
//!
//! Every profile method takes `profile` (default `"default"`). Rooms are given as a contact
//! name, a group name, `notes`, `me` (the first contact) or a room id (`d:…` / `g:…`).

use std::{path::PathBuf, time::Duration};

use brumm_core::{
    api::{Command, ConversationId, DeliveryStatus},
    identity::UserKey,
    wire::{MAX_FILE_BYTES, MessageId},
};
use brumm_host::{
    Outgoing,
    dto::{self, Contact, Identity, Invitation, Message, Room, RoomError, Status},
    policy::{self, Action, Policy},
};
use data_encoding::BASE64;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::{InUse, session::Session};
use crate::{
    paths::DEFAULT_PROFILE,
    protocol::{PROTOCOL_VERSION, RpcError, code},
};

/// `message.wait` / `message.send` without `timeout_ms`.
const DEFAULT_WAIT: Duration = Duration::from_secs(60);

pub async fn call(session: &Session, method: &str, params: Value) -> Result<Value, RpcError> {
    match method {
        "daemon.status" => daemon_status(session),
        "daemon.stop" => daemon_stop(session),
        "profiles.list" => Ok(json!({
            "profiles": session.host().known_profiles(),
            "open": session.host().open_profiles(),
        })),
        "identity.init" => identity_init(session, parse(params)?).await,
        "identity.restore" => identity_restore(session, parse(params)?).await,
        "identity.show" => identity_show(session, parse(params)?).await,
        "invite.create" => invite_create(session, parse(params)?).await,
        "invite.accept" => invite_accept(session, parse(params)?).await,
        "contacts.list" => contacts_list(session, parse(params)?).await,
        "rooms.list" => rooms_list(session, parse(params)?).await,
        "invitations.list" => invitations_list(session, parse(params)?).await,
        "message.send" => message_send(session, parse(params)?).await,
        "message.wait" => message_wait(session, parse(params)?).await,
        "message.get" => message_get(session, parse(params)?).await,
        "messages.read" => messages_read(session, parse(params)?).await,
        "subscribe" => subscribe(session, parse(params)?).await,
        "unsubscribe" => unsubscribe(session, parse(params)?),
        "group.create" => group_create(session, parse(params)?).await,
        "group.invite" => group_invite(session, parse(params)?).await,
        "group.accept" => group_answer(session, parse(params)?, true).await,
        "group.decline" => group_answer(session, parse(params)?, false).await,
        "group.leave" => group_leave(session, parse(params)?).await,
        "policy.get" => policy_get(session, parse(params)?).await,
        "policy.set" => policy_set(session, parse(params)?).await,
        _ => Err(RpcError::new(
            code::METHOD_NOT_FOUND,
            format!("unknown method {method:?}"),
        )),
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(RpcError::invalid_params)
}

fn default_profile() -> String {
    DEFAULT_PROFILE.to_owned()
}

fn to_value(value: impl serde::Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(RpcError::failed)
}

#[derive(Deserialize)]
struct ProfileParams {
    #[serde(default = "default_profile")]
    profile: String,
}

#[derive(Deserialize)]
struct RoomParams {
    #[serde(default = "default_profile")]
    profile: String,
    room: String,
}

async fn open(session: &Session, name: &str) -> Result<InUse, RpcError> {
    session.host().profile(name).await
}

/// The profile, refused if it has no identity yet.
async fn open_identity(session: &Session, name: &str) -> Result<InUse, RpcError> {
    let profile = open(session, name).await?;
    if profile.handle.profile().is_none() {
        return Err(RpcError::not_found(format!(
            "profile {name:?} has no identity yet (run `brumm-cli --profile {name} init <name>`)"
        )));
    }
    Ok(profile)
}

fn room_error(err: RoomError) -> RpcError {
    let code = match err {
        RoomError::InvalidId(_) => code::INVALID_PARAMS,
        RoomError::NotFound(_) => code::NOT_FOUND,
        RoomError::Ambiguous { .. } => code::AMBIGUOUS,
    };
    RpcError::new(code, err.to_string())
}

fn resolve(profile: &InUse, room: &str) -> Result<ConversationId, RpcError> {
    dto::resolve_in(&profile.handle.snapshot(), room).map_err(room_error)
}

/// A contact, named like a room (or by its bare base32 user key).
fn resolve_user(profile: &InUse, user: &str) -> Result<UserKey, RpcError> {
    if let Ok(key) = dto::decode_id::<32>(user) {
        return Ok(key);
    }
    let snapshot = profile.handle.snapshot();
    match dto::resolve_in(&snapshot, user).map_err(room_error)? {
        ConversationId::Direct(key) if snapshot.contacts.iter().any(|c| c.user_key == key) => Ok(key),
        _ => Err(RpcError::invalid_params(format!("{user:?} is not a contact"))),
    }
}

fn refused(err: policy::Refused) -> RpcError {
    RpcError::new(code::REFUSED, err.to_string())
}

fn daemon_status(session: &Session) -> Result<Value, RpcError> {
    let daemon = session
        .daemon()
        .ok_or_else(|| RpcError::failed("not running in a daemon"))?;
    let mut status = daemon.status();
    status["protocol"] = json!(PROTOCOL_VERSION);
    status["version"] = json!(env!("CARGO_PKG_VERSION"));
    status["root"] = json!(session.host().root());
    status["open"] = json!(session.host().open_profiles());
    Ok(status)
}

fn daemon_stop(session: &Session) -> Result<Value, RpcError> {
    let daemon = session
        .daemon()
        .ok_or_else(|| RpcError::failed("not running in a daemon"))?;
    daemon.stop();
    Ok(json!({}))
}

#[derive(Deserialize)]
struct InitParams {
    #[serde(default = "default_profile")]
    profile: String,
    name: String,
}

async fn identity_init(session: &Session, params: InitParams) -> Result<Value, RpcError> {
    let profile = open(session, &params.profile).await?;
    let (identity, phrase) = profile.handle.create_identity(&params.name).await?;
    Ok(json!({ "identity": Identity::from(&identity), "seed_phrase": phrase }))
}

#[derive(Deserialize)]
struct RestoreParams {
    #[serde(default = "default_profile")]
    profile: String,
    phrase: String,
    name: String,
}

async fn identity_restore(session: &Session, params: RestoreParams) -> Result<Value, RpcError> {
    let profile = open(session, &params.profile).await?;
    let identity = profile
        .handle
        .restore_identity(&params.phrase, &params.name)
        .await?;
    Ok(json!({ "identity": Identity::from(&identity) }))
}

async fn identity_show(session: &Session, params: ProfileParams) -> Result<Value, RpcError> {
    let profile = open(session, &params.profile).await?;
    let identity = profile.handle.profile().as_ref().map(Identity::from);
    Ok(json!({ "identity": identity, "locked": profile.policy().locked }))
}

async fn invite_create(session: &Session, params: ProfileParams) -> Result<Value, RpcError> {
    let profile = open_identity(session, &params.profile).await?;
    policy::check(&profile.policy(), Action::CreateInvite).map_err(refused)?;
    let link = profile.handle.create_invite().await?;
    Ok(json!({ "link": link }))
}

#[derive(Deserialize)]
struct AcceptParams {
    #[serde(default = "default_profile")]
    profile: String,
    link: String,
    /// Lock the profile right after (bots).
    #[serde(default)]
    lock: bool,
}

async fn invite_accept(session: &Session, params: AcceptParams) -> Result<Value, RpcError> {
    let profile = open_identity(session, &params.profile).await?;
    policy::check(&profile.policy(), Action::AcceptInvite).map_err(refused)?;
    let contact = profile.handle.accept_invite(params.link.trim()).await?;
    if params.lock {
        let locked = Policy {
            locked: true,
            ..profile.policy()
        };
        store_policy(&profile, locked)?;
    }
    Ok(json!({ "contact": Contact::from(&contact) }))
}

async fn contacts_list(session: &Session, params: ProfileParams) -> Result<Value, RpcError> {
    let profile = open(session, &params.profile).await?;
    let contacts: Vec<Contact> = profile.handle.snapshot().contacts.iter().map(Contact::from).collect();
    Ok(json!({ "contacts": contacts }))
}

async fn rooms_list(session: &Session, params: ProfileParams) -> Result<Value, RpcError> {
    let profile = open(session, &params.profile).await?;
    let snapshot = profile.handle.snapshot();
    let rooms: Vec<Room> = dto::rooms(snapshot.profile.as_ref(), &snapshot.contacts, &snapshot.groups);
    Ok(json!({ "rooms": rooms }))
}

async fn invitations_list(session: &Session, params: ProfileParams) -> Result<Value, RpcError> {
    let profile = open(session, &params.profile).await?;
    let invitations: Vec<Invitation> = profile
        .handle
        .snapshot()
        .invitations
        .iter()
        .map(Invitation::from)
        .collect();
    Ok(json!({ "invitations": invitations }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WaitFor {
    None,
    Sent,
    Delivered,
}

#[derive(Deserialize)]
struct FileParam {
    name: String,
    /// Base64.
    data: String,
}

#[derive(Deserialize)]
struct SendParams {
    #[serde(default = "default_profile")]
    profile: String,
    room: String,
    text: Option<String>,
    file: Option<FileParam>,
    /// A file on this machine, read by the host.
    path: Option<PathBuf>,
    /// Caller-chosen message id (base32): a retry with the same id sends nothing new.
    id: Option<String>,
    wait: Option<WaitFor>,
    timeout_ms: Option<u64>,
}

async fn outgoing(params: &mut SendParams) -> Result<Outgoing, RpcError> {
    let given = [params.text.is_some(), params.file.is_some(), params.path.is_some()];
    if given.iter().filter(|given| **given).count() != 1 {
        return Err(RpcError::invalid_params("give exactly one of text, file or path"));
    }
    if let Some(text) = params.text.take() {
        return Ok(Outgoing::Text(text));
    }
    let (name, data) = match (params.file.take(), params.path.take()) {
        (Some(file), _) => {
            let data = BASE64
                .decode(file.data.as_bytes())
                .map_err(|err| RpcError::invalid_params(format!("file data is not base64: {err}")))?;
            (file.name, data)
        }
        (None, Some(path)) => {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| RpcError::invalid_params("the path has no file name"))?
                .to_owned();
            let data = tokio::fs::read(&path)
                .await
                .map_err(|err| RpcError::invalid_params(format!("read {}: {err}", path.display())))?;
            (name, data)
        }
        (None, None) => unreachable!("checked above"),
    };
    if data.len() > MAX_FILE_BYTES {
        return Err(RpcError::invalid_params(format!(
            "the file has {} bytes; the limit is {MAX_FILE_BYTES}",
            data.len()
        )));
    }
    Ok(Outgoing::File { name, data })
}

fn message_id(text: Option<&str>) -> Result<Option<MessageId>, RpcError> {
    text.map(|id| dto::parse_message_id(id).map_err(RpcError::invalid_params))
        .transpose()
}

async fn message_send(session: &Session, mut params: SendParams) -> Result<Value, RpcError> {
    let profile = open_identity(session, &params.profile).await?;
    let conversation = resolve(&profile, &params.room)?;
    let outgoing = outgoing(&mut params).await?;
    let id = profile
        .handle
        .send(conversation, outgoing, message_id(params.id.as_deref())?)
        .await?;
    let target = match params.wait.unwrap_or(WaitFor::None) {
        WaitFor::None => None,
        WaitFor::Sent => Some(DeliveryStatus::Sent),
        WaitFor::Delivered => Some(DeliveryStatus::Delivered),
    };
    let timeout = params.timeout_ms.map_or(DEFAULT_WAIT, Duration::from_millis);
    let status = wait_or_current(&profile, conversation, id, target, timeout).await?;
    Ok(json!({ "id": dto::encode_id(&id), "room": dto::room_id(&conversation), "status": status }))
}

/// Waits for `target` (if any); a timeout is a `TIMEOUT` error that tells how far it got.
async fn wait_or_current(
    profile: &InUse,
    conversation: ConversationId,
    id: MessageId,
    target: Option<DeliveryStatus>,
    timeout: Duration,
) -> Result<Status, RpcError> {
    let current = || async {
        profile
            .handle
            .message(conversation, id)
            .await
            .map_err(RpcError::from)?
            .map(|message| Status::from(message.status))
            .ok_or_else(|| RpcError::not_found("no such message"))
    };
    let Some(target) = target else {
        return current().await;
    };
    match profile
        .handle
        .wait_status(conversation, id, target, timeout)
        .await
    {
        Ok(status) => Ok(status.into()),
        Err(err) => {
            let status = current().await?;
            if matches!(status, Status::Pending | Status::Sent) {
                let data = json!({ "id": dto::encode_id(&id), "room": dto::room_id(&conversation), "status": status });
                return Err(RpcError::new(code::TIMEOUT, format!("{err:#}")).with_data(data));
            }
            Err(RpcError::from(err))
        }
    }
}

#[derive(Deserialize)]
struct WaitParams {
    #[serde(default = "default_profile")]
    profile: String,
    room: String,
    id: String,
    #[serde(default = "default_wait_status")]
    status: WaitFor,
    timeout_ms: Option<u64>,
}

fn default_wait_status() -> WaitFor {
    WaitFor::Delivered
}

async fn message_wait(session: &Session, params: WaitParams) -> Result<Value, RpcError> {
    let profile = open_identity(session, &params.profile).await?;
    let conversation = resolve(&profile, &params.room)?;
    let id = dto::parse_message_id(&params.id).map_err(RpcError::invalid_params)?;
    let target = match params.status {
        WaitFor::None => None,
        WaitFor::Sent => Some(DeliveryStatus::Sent),
        WaitFor::Delivered => Some(DeliveryStatus::Delivered),
    };
    let timeout = params.timeout_ms.map_or(DEFAULT_WAIT, Duration::from_millis);
    let status = wait_or_current(&profile, conversation, id, target, timeout).await?;
    Ok(json!({ "status": status }))
}

#[derive(Deserialize)]
struct GetParams {
    #[serde(default = "default_profile")]
    profile: String,
    room: String,
    id: String,
}

async fn message_get(session: &Session, params: GetParams) -> Result<Value, RpcError> {
    let profile = open_identity(session, &params.profile).await?;
    let conversation = resolve(&profile, &params.room)?;
    let id = dto::parse_message_id(&params.id).map_err(RpcError::invalid_params)?;
    let message = profile.handle.message(conversation, id).await?;
    Ok(json!({ "message": message.as_ref().map(Message::from) }))
}

#[derive(Deserialize)]
struct ReadParams {
    #[serde(default = "default_profile")]
    profile: String,
    room: String,
    /// Cursor from an earlier read (from the first message if absent).
    since: Option<u64>,
    limit: Option<u32>,
    /// Instead: the `last` messages that arrived last and the cursor after them (0: only the
    /// cursor, to follow the room from now on).
    last: Option<u32>,
}

async fn messages_read(session: &Session, params: ReadParams) -> Result<Value, RpcError> {
    let profile = open_identity(session, &params.profile).await?;
    let conversation = resolve(&profile, &params.room)?;
    let page = match params.last {
        Some(last) => profile.handle.read_latest(conversation, last).await?,
        None => {
            profile
                .handle
                .read_since(conversation, params.since, params.limit)
                .await?
        }
    };
    let mut page = to_value(dto::Page::from(&page))?;
    page["room"] = json!(dto::room_id(&conversation));
    Ok(page)
}

#[derive(Deserialize)]
struct SubscribeParams {
    #[serde(default = "default_profile")]
    profile: String,
    /// Room names or ids; all rooms if absent.
    rooms: Option<Vec<String>>,
}

async fn subscribe(session: &Session, params: SubscribeParams) -> Result<Value, RpcError> {
    let profile = open(session, &params.profile).await?;
    let rooms = params
        .rooms
        .map(|rooms| {
            rooms
                .iter()
                .map(|room| resolve(&profile, room).map(|conversation| dto::room_id(&conversation)))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    let subscription = session.subscribe(profile, rooms);
    Ok(json!({ "subscription": subscription }))
}

#[derive(Deserialize)]
struct UnsubscribeParams {
    subscription: u64,
}

fn unsubscribe(session: &Session, params: UnsubscribeParams) -> Result<Value, RpcError> {
    if !session.unsubscribe(params.subscription) {
        return Err(RpcError::not_found("no such subscription"));
    }
    Ok(json!({}))
}

#[derive(Deserialize)]
struct GroupCreateParams {
    #[serde(default = "default_profile")]
    profile: String,
    name: String,
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    announcement: bool,
}

async fn group_create(session: &Session, params: GroupCreateParams) -> Result<Value, RpcError> {
    let profile = open_identity(session, &params.profile).await?;
    let members = params
        .members
        .iter()
        .map(|member| resolve_user(&profile, member))
        .collect::<Result<Vec<_>, _>>()?;
    let group = profile
        .handle
        .create_group(&params.name, params.announcement, members)
        .await?;
    Ok(json!({ "room": Room::from(&group) }))
}

#[derive(Deserialize)]
struct GroupInviteParams {
    #[serde(default = "default_profile")]
    profile: String,
    room: String,
    users: Vec<String>,
}

fn group_of(profile: &InUse, room: &str) -> Result<brumm_core::api::GroupId, RpcError> {
    match resolve(profile, room)? {
        ConversationId::Group(group) => Ok(group),
        ConversationId::Direct(_) => Err(RpcError::invalid_params(format!("{room:?} is not a group"))),
    }
}

async fn group_invite(session: &Session, params: GroupInviteParams) -> Result<Value, RpcError> {
    let profile = open_identity(session, &params.profile).await?;
    let group = group_of(&profile, &params.room)?;
    let users = params
        .users
        .iter()
        .map(|user| resolve_user(&profile, user))
        .collect::<Result<Vec<_>, _>>()?;
    profile
        .handle
        .execute(Command::InviteToGroup { group, users })
        .await?;
    Ok(json!({}))
}

/// Accepting or declining by the group's room id (`g:…`, from `invitations.list`) or name.
async fn group_answer(session: &Session, params: RoomParams, accept: bool) -> Result<Value, RpcError> {
    let profile = open_identity(session, &params.profile).await?;
    let invitations = profile.handle.snapshot().invitations;
    let group = match dto::parse_room_id(params.room.trim()) {
        Some(Ok(ConversationId::Group(group))) => group,
        Some(_) => return Err(RpcError::invalid_params("not a group room id")),
        None => {
            let named: Vec<_> = invitations
                .iter()
                .filter(|invitation| invitation.name.eq_ignore_ascii_case(params.room.trim()))
                .collect();
            match named.as_slice() {
                [invitation] => invitation.group,
                [] => return Err(RpcError::not_found(format!("no invitation to {:?}", params.room))),
                _ => {
                    return Err(RpcError::new(
                        code::AMBIGUOUS,
                        format!("several invitations to {:?}, use the room id", params.room),
                    ));
                }
            }
        }
    };
    let command = if accept {
        Command::AcceptGroupInvitation { group }
    } else {
        Command::DeclineGroupInvitation { group }
    };
    profile.handle.execute(command).await?;
    Ok(json!({}))
}

async fn group_leave(session: &Session, params: RoomParams) -> Result<Value, RpcError> {
    let profile = open_identity(session, &params.profile).await?;
    let group = group_of(&profile, &params.room)?;
    profile.handle.execute(Command::LeaveGroup { group }).await?;
    Ok(json!({}))
}

async fn policy_get(session: &Session, params: ProfileParams) -> Result<Value, RpcError> {
    let profile = open(session, &params.profile).await?;
    Ok(json!({ "policy": profile.policy() }))
}

#[derive(Deserialize)]
struct PolicySetParams {
    #[serde(default = "default_profile")]
    profile: String,
    policy: Policy,
}

async fn policy_set(session: &Session, params: PolicySetParams) -> Result<Value, RpcError> {
    let profile = open(session, &params.profile).await?;
    store_policy(&profile, params.policy.clone())?;
    Ok(json!({ "policy": params.policy }))
}

/// Writes `policy.toml` and applies it at once.
fn store_policy(profile: &InUse, policy: Policy) -> Result<(), RpcError> {
    policy::save(profile.handle.data_dir(), &policy)?;
    profile.policy.send_replace(policy);
    Ok(())
}
