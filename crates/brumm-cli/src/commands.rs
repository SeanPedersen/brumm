//! What each CLI subcommand does: turn its arguments into protocol calls, print the answers.
//! The calls go through a [`Transport`], so a command works the same in the daemon and
//! one-shot.

use std::{
    collections::HashMap,
    io::{IsTerminal, Read},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use brumm_core::wire::MAX_TEXT_BYTES;
use data_encoding::BASE64;
use serde_json::{Value, json};

use crate::{
    cli::{Cmd, GroupCmd, ListenArgs, PolicyArgs, SendArgs},
    output::{self, Output, message_line, str_field},
    protocol::RpcError,
    text::{duration_millis, part_id, split_text},
    transport::Transport,
};

/// File name for text over the message limit sent as one file.
const LONG_TEXT_FILE: &str = "message.txt";

pub struct Context<'a> {
    pub transport: &'a Transport,
    pub profile: &'a str,
    pub out: Output,
}

impl Context<'_> {
    async fn call(&self, method: &str, mut params: Value) -> Result<Value, RpcError> {
        params["profile"] = json!(self.profile);
        self.transport.call(method, params).await
    }
}

pub async fn run(cx: &Context<'_>, command: Cmd) -> Result<(), RpcError> {
    match command {
        Cmd::Init { name } => {
            let result = cx.call("identity.init", json!({ "name": name })).await?;
            cx.out.emit(&result, |r| {
                format!(
                    "created {} ({})\n\nseed phrase (write it down, it is shown once):\n{}",
                    str_field(&r["identity"], "name"),
                    str_field(&r["identity"], "fingerprint"),
                    str_field(r, "seed_phrase")
                )
            });
        }
        Cmd::Restore { name, phrase } => {
            let phrase = match phrase {
                Some(phrase) => phrase,
                None => read_stdin().map_err(RpcError::from)?,
            };
            let result = cx
                .call("identity.restore", json!({ "name": name, "phrase": phrase.trim() }))
                .await?;
            cx.out.emit(&result, |r| format!("restored {}", str_field(&r["identity"], "name")));
        }
        Cmd::Show => {
            let result = cx.call("identity.show", json!({})).await?;
            cx.out.emit(&result, |r| match r["identity"].as_object() {
                None => "no identity yet (brumm-cli init <name>)".to_owned(),
                Some(identity) => format!(
                    "{} ({}){}\nuser {}",
                    identity["name"].as_str().unwrap_or_default(),
                    identity["fingerprint"].as_str().unwrap_or_default(),
                    if r["locked"].as_bool() == Some(true) { ", locked" } else { "" },
                    identity["user"].as_str().unwrap_or_default(),
                ),
            });
        }
        Cmd::Invite {
            no_qr,
            no_wait,
            timeout,
        } => invite(cx, no_qr, no_wait, timeout).await?,
        Cmd::Accept { link, lock } => {
            let result = cx.call("invite.accept", json!({ "link": link, "lock": lock })).await?;
            cx.out.emit(&result, |r| {
                format!(
                    "added {}{}",
                    str_field(&r["contact"], "name"),
                    if lock { " (profile locked)" } else { "" }
                )
            });
        }
        Cmd::Contacts => {
            let result = cx.call("contacts.list", json!({})).await?;
            cx.out.emit(&result, |r| {
                lines(&r["contacts"], |c| {
                    format!("{}  {}  {}", str_field(c, "name"), str_field(c, "fingerprint"), str_field(c, "room"))
                })
            });
        }
        Cmd::Rooms => {
            let result = cx.call("rooms.list", json!({})).await?;
            cx.out.emit(&result, |r| {
                lines(&r["rooms"], |room| {
                    format!("{:<6} {}  {}", str_field(room, "kind"), str_field(room, "name"), str_field(room, "id"))
                })
            });
        }
        Cmd::Invitations => {
            let result = cx.call("invitations.list", json!({})).await?;
            cx.out.emit(&result, |r| {
                lines(&r["invitations"], |i| {
                    format!(
                        "{} from {} ({} members, {})  {}",
                        str_field(i, "name"),
                        str_field(i, "inviter_name"),
                        i["members"],
                        str_field(i, "stage"),
                        str_field(i, "room")
                    )
                })
            });
        }
        Cmd::Profiles => {
            let result = cx.transport.call("profiles.list", json!({})).await?;
            cx.out.emit(&result, |r| lines(&r["profiles"], |p| p.as_str().unwrap_or_default().to_owned()));
        }
        Cmd::Send(args) => send(cx, args).await?,
        Cmd::Read {
            room,
            since,
            limit,
            last,
        } => {
            let page = cx
                .call("messages.read", json!({ "room": room, "since": since, "limit": limit, "last": last }))
                .await?;
            let names = room_names(cx).await?;
            cx.out.emit(&page, |p| {
                let name = names.get(str_field(p, "room")).map_or(room.as_str(), String::as_str);
                lines(&p["messages"], |m| message_line(m, name))
            });
            if !cx.out.json {
                cx.out.note(&format!("cursor: {}", page["cursor"]));
            }
        }
        Cmd::Listen(args) => listen(cx, args).await?,
        Cmd::Wait {
            room,
            id,
            status,
            timeout,
        } => {
            let result = cx
                .call(
                    "message.wait",
                    json!({ "room": room, "id": id, "status": status.as_str(), "timeout_ms": duration_millis(timeout) }),
                )
                .await?;
            cx.out.emit(&result, |r| str_field(r, "status").to_owned());
        }
        Cmd::Group(group) => group_command(cx, group).await?,
        Cmd::Policy(args) => policy(cx, args).await?,
        Cmd::Daemon(_) => unreachable!("handled before a transport exists"),
    }
    Ok(())
}

fn lines(list: &Value, line: impl Fn(&Value) -> String) -> String {
    list.as_array()
        .map(|items| items.iter().map(line).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

fn read_stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("read stdin")?;
    Ok(text)
}

/// Room id → display name.
async fn room_names(cx: &Context<'_>) -> Result<HashMap<String, String>, RpcError> {
    let rooms = cx.call("rooms.list", json!({})).await?;
    Ok(rooms["rooms"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|room| (str_field(room, "id").to_owned(), str_field(room, "name").to_owned()))
        .collect())
}

async fn invite(cx: &Context<'_>, no_qr: bool, no_wait: bool, timeout: Duration) -> Result<(), RpcError> {
    // Subscribed first, so the new contact cannot slip in between.
    if !no_wait {
        cx.call("subscribe", json!({})).await?;
    }
    let result = cx.call("invite.create", json!({})).await?;
    let link = str_field(&result, "link").to_owned();
    cx.out.emit(&result, |_| link.clone());
    if !cx.out.json && !no_qr
        && let Some(qr) = output::qr(&link)
    {
        cx.out.note(&qr);
    }
    if no_wait {
        return Ok(());
    }
    if !cx.out.json {
        cx.out.note("waiting for the invite to be used (Ctrl+C to stop)…");
    }
    let contact = tokio::time::timeout(timeout, async {
        while let Some(event) = cx.transport.next_event().await {
            let notice = &event["event"];
            if str_field(notice, "type") == "room" && str_field(&notice["room"], "kind") == "direct" {
                return Some(notice["room"].clone());
            }
        }
        None
    })
    .await
    .map_err(|_| RpcError::new(crate::protocol::code::TIMEOUT, "nobody used the invite in time"))?
    .ok_or_else(|| RpcError::failed("the connection ended"))?;
    cx.out.emit(&json!({ "contact": contact }), |r| {
        format!("paired with {}", str_field(&r["contact"], "name"))
    });
    Ok(())
}

async fn send(cx: &Context<'_>, args: SendArgs) -> Result<(), RpcError> {
    let base = json!({
        "room": args.room,
        "wait": args.wait.as_str(),
        "timeout_ms": duration_millis(args.timeout),
    });
    let mut bodies: Vec<Value> = Vec::new();
    if let Some(path) = &args.file {
        let path = std::fs::canonicalize(path)
            .map_err(|err| RpcError::invalid_params(format!("{}: {err}", path.display())))?;
        bodies.push(json!({ "path": path }));
    } else {
        let text = match args.text.as_slice() {
            [] | [_] if args.text.first().is_none_or(|text| text == "-") => {
                if std::io::stdin().is_terminal() {
                    return Err(RpcError::invalid_params("nothing to send: give a text or pipe it in"));
                }
                read_stdin().map_err(RpcError::from)?
            }
            words => words.join(" "),
        };
        if text.trim().is_empty() {
            return Err(RpcError::invalid_params("nothing to send: the text is empty"));
        }
        if text.len() <= MAX_TEXT_BYTES {
            bodies.push(json!({ "text": text }));
        } else if args.split {
            bodies.extend(split_text(&text, MAX_TEXT_BYTES).into_iter().map(|part| json!({ "text": part })));
        } else {
            if !cx.out.json {
                cx.out.note(&format!(
                    "the text is over {MAX_TEXT_BYTES} bytes: sending it as {LONG_TEXT_FILE} (--split sends messages)"
                ));
            }
            bodies.push(json!({ "file": { "name": LONG_TEXT_FILE, "data": BASE64.encode(text.as_bytes()) } }));
        }
    }
    let id = args
        .id
        .as_deref()
        .map(|id| brumm_host::dto::parse_message_id(id).map_err(RpcError::invalid_params))
        .transpose()?;
    let parts = bodies.len();
    for (index, body) in bodies.into_iter().enumerate() {
        let mut params = base.clone();
        for (key, value) in body.as_object().expect("bodies are objects") {
            params[key] = value.clone();
        }
        if let Some(id) = &id {
            let part = if parts == 1 { *id } else { part_id(id, index) };
            params["id"] = json!(brumm_host::dto::encode_id(&part));
        }
        let result = cx.call("message.send", params).await?;
        cx.out.emit(&result, |r| format!("{} {}", str_field(r, "status"), str_field(r, "id")));
    }
    Ok(())
}

/// Follows rooms by cursor: a `message` notice triggers a read from the room's cursor, so
/// nothing is printed twice or skipped, also across `lagged` notices.
async fn listen(cx: &Context<'_>, args: ListenArgs) -> Result<(), RpcError> {
    if args.since.is_some() && args.rooms.len() != 1 {
        return Err(RpcError::invalid_params("--since needs exactly one room"));
    }
    let rooms = (!args.rooms.is_empty()).then(|| args.rooms.clone());
    cx.call("subscribe", json!({ "rooms": rooms })).await?;
    let mut names = room_names(cx).await?;
    let mut cursors: HashMap<String, u64> = HashMap::new();
    let followed: Vec<String> = match &rooms {
        Some(rooms) => rooms.clone(),
        None => names.keys().cloned().collect(),
    };
    for room in &followed {
        let start = match args.since {
            Some(since) => json!({ "room": room, "since": since, "limit": 0 }),
            None => json!({ "room": room, "last": 0 }),
        };
        let page = cx.call("messages.read", start).await?;
        let id = str_field(&page, "room").to_owned();
        cursors.insert(id, args.since.unwrap_or_else(|| page["cursor"].as_u64().unwrap_or_default()));
    }
    let mut printed = 0;
    let deadline = args.timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    // `--since`: what already arrived after the cursor comes first.
    if args.since.is_some() {
        let rooms: Vec<String> = cursors.keys().cloned().collect();
        printed += catch_up(cx, args.outgoing, args.count, &rooms, &mut cursors, &names).await?;
    }
    while args.count.is_none_or(|count| printed < count) {
        let next = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, cx.transport.next_event()).await {
                Ok(next) => next,
                Err(_) => break,
            },
            None => cx.transport.next_event().await,
        };
        let Some(event) = next else { break };
        let notice = &event["event"];
        let stale: Vec<String> = match str_field(notice, "type") {
            "message" => vec![str_field(&notice["message"], "room").to_owned()],
            "lagged" => cursors.keys().cloned().collect(),
            "changed" => notice["rooms"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|room| room.as_str().map(str::to_owned))
                .collect(),
            other => {
                if other == "room" {
                    let room = &notice["room"];
                    names.insert(str_field(room, "id").to_owned(), str_field(room, "name").to_owned());
                }
                if args.all_events {
                    cx.out.emit(notice, |n| n.to_string());
                }
                continue;
            }
        };
        let remaining = args.count.map(|count| count - printed);
        printed += catch_up(cx, args.outgoing, remaining, &stale, &mut cursors, &names).await?;
    }
    Ok(())
}

/// Prints what arrived in `rooms` after their cursors (at most `limit`); returns how many
/// messages it printed.
async fn catch_up(
    cx: &Context<'_>,
    outgoing: bool,
    limit: Option<usize>,
    rooms: &[String],
    cursors: &mut HashMap<String, u64>,
    names: &HashMap<String, String>,
) -> Result<usize, RpcError> {
    let mut printed = 0;
    for room in rooms {
        loop {
            // A room that appeared after we started (new contact or group) is read in full.
            let since = cursors.get(room).copied().unwrap_or_default();
            let page = cx.call("messages.read", json!({ "room": room, "since": since })).await?;
            cursors.insert(room.clone(), page["cursor"].as_u64().unwrap_or(since));
            for message in page["messages"].as_array().into_iter().flatten() {
                if !outgoing && str_field(message, "direction") == "out" {
                    continue;
                }
                if limit.is_some_and(|limit| printed >= limit) {
                    return Ok(printed);
                }
                let line = json!({ "room": room, "cursor": message["cursor"], "message": message });
                let name = names.get(room).map_or(room.as_str(), String::as_str);
                cx.out.emit(&line, |l| message_line(&l["message"], name));
                printed += 1;
            }
            if page["has_more"].as_bool() != Some(true) {
                break;
            }
        }
    }
    Ok(printed)
}

async fn group_command(cx: &Context<'_>, command: GroupCmd) -> Result<(), RpcError> {
    let (method, params) = match command {
        GroupCmd::Create {
            name,
            members,
            announcement,
        } => ("group.create", json!({ "name": name, "members": members, "announcement": announcement })),
        GroupCmd::Invite { room, users } => ("group.invite", json!({ "room": room, "users": users })),
        GroupCmd::Accept { room } => ("group.accept", json!({ "room": room })),
        GroupCmd::Decline { room } => ("group.decline", json!({ "room": room })),
        GroupCmd::Leave { room } => ("group.leave", json!({ "room": room })),
    };
    let result = cx.call(method, params).await?;
    cx.out.emit(&result, |r| match r["room"].as_object() {
        Some(room) => format!(
            "created {}  {}",
            room["name"].as_str().unwrap_or_default(),
            room["id"].as_str().unwrap_or_default()
        ),
        None => "ok".to_owned(),
    });
    Ok(())
}

async fn policy(cx: &Context<'_>, args: PolicyArgs) -> Result<(), RpcError> {
    let current = cx.call("policy.get", json!({})).await?;
    let mut policy = current["policy"].clone();
    let changes = args.lock || args.unlock || args.clear_allow || !args.allow.is_empty() || args.auto_accept.is_some();
    if changes {
        if args.lock || args.unlock {
            policy["locked"] = json!(args.lock);
        }
        if let Some(auto_accept) = args.auto_accept {
            policy["auto_accept"] = json!(auto_accept);
        }
        if args.clear_allow {
            policy["allow"] = json!([]);
        }
        if !args.allow.is_empty() {
            let contacts = cx.call("contacts.list", json!({})).await?;
            let mut allow: Vec<Value> = policy["allow"].as_array().cloned().unwrap_or_default();
            for user in &args.allow {
                let key = user_key(&contacts, user).map_err(RpcError::invalid_params)?;
                if !allow.contains(&json!(key)) {
                    allow.push(json!(key));
                }
            }
            policy["allow"] = json!(allow);
        }
        cx.call("policy.set", json!({ "policy": policy })).await?;
    }
    cx.out.emit(&json!({ "policy": policy }), |r| {
        let p = &r["policy"];
        let allow = p["allow"].as_array().map_or(0, Vec::len);
        format!(
            "locked: {}\nauto_accept: {}\nallow: {}",
            p["locked"],
            p["auto_accept"],
            if allow == 0 { "the first contact".to_owned() } else { format!("{allow} users") }
        )
    });
    Ok(())
}

/// A contact's user key from its name or the key itself.
fn user_key(contacts: &Value, user: &str) -> Result<String> {
    let contacts = contacts["contacts"].as_array().cloned().unwrap_or_default();
    let matching: Vec<&Value> = contacts
        .iter()
        .filter(|c| str_field(c, "name").eq_ignore_ascii_case(user) || str_field(c, "user").eq_ignore_ascii_case(user))
        .collect();
    match matching.as_slice() {
        [contact] => Ok(str_field(contact, "user").to_owned()),
        [] if brumm_host::dto::decode_id::<32>(user).is_ok() => Ok(user.to_ascii_lowercase()),
        [] => bail!("no contact {user:?}"),
        _ => bail!("several contacts are called {user:?}; give the user key"),
    }
}
