//! H2: the CLI without a daemon. A bot identity and "the app" are never online at the same time
//! after pairing; everything goes through the (in-memory) Nostr mailbox.

mod common;

use std::time::{Duration, Instant};

use brumm_core::api::{ConversationId, DeliveryStatus};
use brumm_host::{Outgoing, dto};
use common::*;

/// A one-shot send that does not wait still has to drain to the relays before it exits.
const QUICK_EXIT: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread")]
async fn bot_and_app_never_online_together() {
    let (_relay, relays) = local_relay().await;
    let env = Env::new("oneshot", relays);
    let app = env.app("owner").await;
    app.create_identity("Owner").await.unwrap();

    env.ok(&["--no-daemon", "-p", "bot", "init", "Bot"]);
    let link = app.create_invite().await.unwrap();
    let accepted = env.ok(&["--no-daemon", "-p", "bot", "accept", &link, "--lock"]);
    assert!(accepted.contains("added Owner"), "{accepted}");
    let start = &env.json(&["--no-daemon", "-p", "bot", "read", "me", "--last", "0"])[0];
    let cursor = start["cursor"].as_u64().unwrap();
    let bot_key = dto::decode_id::<32>(
        env.json(&["--no-daemon", "-p", "bot", "show"])[0]["identity"]["user"].as_str().unwrap(),
    )
    .unwrap();
    let app_dir = app.data_dir().to_owned();
    app.shutdown().await.unwrap();

    // The app is offline from here on: stdin, exit right after the send, the default wait (Sent).
    let started = Instant::now();
    let piped = env.run_with_stdin(&["--no-daemon", "-p", "bot", "send", "me", "--wait", "none"], b"build 1 finished\n");
    assert_ok(&piped, &["send (stdin)"]);
    assert!(started.elapsed() < QUICK_EXIT, "took {:?}", started.elapsed());
    let sent = env.json(&["--no-daemon", "-p", "bot", "send", "me", "build 2 finished"]);
    assert_eq!(sent[0]["status"], "sent");
    // Over wire::MAX_TEXT_BYTES (56 KiB): one file, or two parts with --split.
    let long = "line of a long log\n".repeat(3200);
    env.run_with_stdin(&["--no-daemon", "-p", "bot", "send", "me", "--wait", "none"], long.as_bytes());
    let split = env.json(&["--no-daemon", "-p", "bot", "send", "me", "--split", "--wait", "none", &long]);
    assert_eq!(split.len(), 2);
    let id = dto::encode_id(&[7; 16]);
    for _ in 0..2 {
        env.ok(&["--no-daemon", "-p", "bot", "send", "me", "--id", &id, "--wait", "none", "only once"]);
    }

    let app = brumm_host::Client::open(app_config(&app_dir, &env.relays)).await.unwrap();
    let bot = ConversationId::Direct(bot_key);
    let mut page = wait_for_messages(&app, bot, 6).await;
    // Mailbox letters arrive in any order; the read is in arrival order.
    page.messages.sort_by_key(|message| message.sent_at_ms);
    let texts: Vec<String> = page.messages.iter().map(|m| m.text.clone()).collect();
    assert_eq!(texts[0], "build 1 finished");
    assert_eq!(texts[1], "build 2 finished");
    assert_eq!(page.messages[2].attachment.as_ref().unwrap().name, "message.txt");
    // Each part is a message of its own; the core trims the line break at the cut.
    assert_eq!(format!("{}\n{}", texts[3], texts[4]), long.trim_end());
    assert_eq!(texts[5], "only once");

    // The app answers while the bot is offline; the bot picks it up from its cursor later.
    let reply = app.send(bot, Outgoing::Text("ack".into()), None).await.unwrap();
    app.wait_status(bot, reply, DeliveryStatus::Sent, PATIENCE).await.unwrap();
    app.shutdown().await.unwrap();
    let cursor_arg = cursor.to_string();
    let heard = env.json(&[
        "--no-daemon", "-p", "bot", "listen", "me", "--since", &cursor_arg, "--count", "1", "--timeout", "40s",
    ]);
    assert_eq!(heard.len(), 1, "{heard:?}");
    assert_eq!(heard[0]["message"]["text"], "ack");

    // Locked after pairing: no new invites.
    let refused = env.run(&["--no-daemon", "-p", "bot", "invite", "--no-wait"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("locked"));
    assert_db_intact(&env.profile_db("bot"));
}

async fn wait_for_messages(app: &brumm_host::Client, room: ConversationId, count: usize) -> brumm_host::Page {
    let started = Instant::now();
    loop {
        let page = app.read_since(room, None, None).await.unwrap();
        if page.messages.len() >= count {
            assert_eq!(page.messages.len(), count, "{:?}", page.messages);
            return page;
        }
        assert!(started.elapsed() < PATIENCE, "only {} of {count} arrived", page.messages.len());
        tokio::time::sleep(POLL).await;
    }
}

#[test]
fn usage_errors_are_clear() {
    let env = Env::new("usage", vec![NO_RELAY.into()]);
    let output = env.run(&["--no-daemon", "-p", "../escape", "show"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid profile name"));
    let output = env.run(&["--no-daemon", "-p", "fresh", "send", "notes", "hi"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("no identity yet"));
    env.ok(&["--no-daemon", "-p", "fresh", "init", "Fresh"]);
    let output = env.run(&["--no-daemon", "-p", "fresh", "send", "nobody", "hi"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("no room is called"));
}
