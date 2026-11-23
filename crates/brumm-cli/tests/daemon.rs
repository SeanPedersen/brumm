//! H3: the daemon. Several clients on one profile, a cursor across a daemon restart, the
//! policy (auto-accept from the allowlist, decline in lock mode) and lock mode refusing invites.

mod common;

use std::time::{Duration, Instant};

use brumm_cli::protocol::code;
use brumm_core::api::ConversationId;
use brumm_host::dto;
use common::*;
use serde_json::{Value, json};

const EVENT_WAIT: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_share_one_profile() {
    let env = Env::new("daemon-two", vec![NO_RELAY.into()]);
    let writer = env.daemon().await;
    let reader = env.connect().await;
    call(&writer, "p", "identity.init", json!({ "name": "P" })).await;
    let subscription = call(&reader, "p", "subscribe", json!({ "rooms": ["notes"] })).await;
    assert!(subscription["subscription"].as_u64().is_some());

    let sent = call(&writer, "p", "message.send", json!({ "room": "notes", "text": "from the writer" })).await;
    let event = tokio::time::timeout(EVENT_WAIT, reader.next_event()).await.unwrap().unwrap();
    assert_eq!(event["profile"], "p");
    assert_eq!(event["event"]["type"], "message");
    assert_eq!(event["event"]["message"]["id"], sent["id"]);
    assert_eq!(event["event"]["message"]["text"], "from the writer");

    for client in [&writer, &reader] {
        let page = call(client, "p", "messages.read", json!({ "room": "notes" })).await;
        assert_eq!(texts(&page), vec!["from the writer"]);
    }
    let status = writer.call("daemon.status", json!({})).await.unwrap();
    assert_eq!(status["connections"], 2);
    assert_eq!(status["open"], json!(["p"]));
    // Unknown methods and bad params are protocol errors, not crashes.
    assert_eq!(writer.call("nope", json!({})).await.unwrap_err().code, code::METHOD_NOT_FOUND);
    let bad = writer.call("message.send", json!({ "profile": "p", "room": 5 })).await;
    assert_eq!(bad.unwrap_err().code, code::INVALID_PARAMS);
    drop((writer, reader));
    env.stop_daemon();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_survives_a_daemon_restart() {
    let env = Env::new("daemon-restart", vec![NO_RELAY.into()]);
    let remote = env.daemon().await;
    call(&remote, "p", "identity.init", json!({ "name": "P" })).await;
    for text in ["one", "two"] {
        call(&remote, "p", "message.send", json!({ "room": "notes", "text": text })).await;
    }
    let first = call(&remote, "p", "messages.read", json!({ "room": "notes" })).await;
    let cursor = first["cursor"].as_u64().unwrap();
    drop(remote);
    env.stop_daemon();

    let remote = env.daemon().await;
    for text in ["three", "four"] {
        call(&remote, "p", "message.send", json!({ "room": "notes", "text": text })).await;
    }
    let after = call(&remote, "p", "messages.read", json!({ "room": "notes", "since": cursor })).await;
    assert_eq!(texts(&after), vec!["three", "four"]);
    let paged = call(&remote, "p", "messages.read", json!({ "room": "notes", "since": cursor, "limit": 1 })).await;
    assert_eq!(texts(&paged), vec!["three"]);
    assert_eq!(paged["has_more"], true);
    drop(remote);
    env.stop_daemon();
    assert_db_intact(&env.profile_db("p"));
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_accepts_the_owner_and_declines_others_when_locked() {
    let env = Env::new("daemon-policy", vec![NO_RELAY.into()]);
    let owner = env.app("owner").await;
    owner.create_identity("Owner").await.unwrap();
    let carol = env.app("carol").await;
    carol.create_identity("Carol").await.unwrap();
    let remote = env.daemon().await;
    let bot = call(&remote, "bot", "identity.init", json!({ "name": "Bot" })).await;
    let bot_key = dto::decode_id::<32>(bot["identity"]["user"].as_str().unwrap()).unwrap();
    // The owner pairs first, so the default allowlist (the first contact) is the owner.
    call(&remote, "bot", "invite.accept", json!({ "link": owner.create_invite().await.unwrap() })).await;
    call(&remote, "bot", "invite.accept", json!({ "link": carol.create_invite().await.unwrap() })).await;

    // Unlocked: Carol's invitation waits for an answer.
    let carols = carol.create_group("from carol", false, vec![bot_key]).await.unwrap();
    let carols_room = dto::room_id(&ConversationId::Group(carols.id));
    wait_for(&remote, "invitations.list", |result| has_room(&result["invitations"], "room", &carols_room)).await;
    // Locking declines it right away.
    let mut policy = call(&remote, "bot", "policy.get", json!({})).await["policy"].clone();
    policy["locked"] = json!(true);
    call(&remote, "bot", "policy.set", json!({ "policy": policy })).await;
    wait_for(&remote, "invitations.list", |result| !has_room(&result["invitations"], "room", &carols_room)).await;

    // The owner's invitation is accepted without a click.
    let owners = owner.create_group("agents", false, vec![bot_key]).await.unwrap();
    let owners_room = dto::room_id(&ConversationId::Group(owners.id));
    wait_for(&remote, "rooms.list", |result| has_room(&result["rooms"], "id", &owners_room)).await;
    let rooms = call(&remote, "bot", "rooms.list", json!({})).await;
    assert!(!has_room(&rooms["rooms"], "id", &carols_room));
    let posted = call(&remote, "bot", "message.send", json!({ "room": "agents", "text": "bot here" })).await;
    assert_eq!(posted["room"], json!(owners_room));

    // Lock mode refuses invites both ways.
    let refused = remote.call("invite.create", json!({ "profile": "bot" })).await.unwrap_err();
    assert_eq!(refused.code, code::REFUSED);
    let link = owner.create_invite().await.unwrap();
    let refused = remote.call("invite.accept", json!({ "profile": "bot", "link": link })).await.unwrap_err();
    assert_eq!(refused.code, code::REFUSED);
    assert!(std::fs::read_to_string(env.root.join("profiles/bot/policy.toml")).unwrap().contains("locked = true"));

    drop(remote);
    env.stop_daemon();
    owner.shutdown().await.unwrap();
    carol.shutdown().await.unwrap();
}

fn has_room(list: &Value, key: &str, room: &str) -> bool {
    list.as_array().unwrap().iter().any(|item| item[key] == room)
}

async fn wait_for(remote: &brumm_cli::remote::Remote, method: &str, done: impl Fn(&Value) -> bool) {
    let started = Instant::now();
    loop {
        let result = call(remote, "bot", method, json!({})).await;
        if done(&result) {
            return;
        }
        assert!(started.elapsed() < PATIENCE, "{method} never got there: {result}");
        tokio::time::sleep(POLL).await;
    }
}
