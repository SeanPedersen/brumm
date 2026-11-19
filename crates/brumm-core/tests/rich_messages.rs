//! e2e: file messages, "delete for everyone" and profile photos between two online nodes.

mod common;

use brumm_core::{
    api::{Command, Direction, Event},
    wire::MAX_FILE_BYTES,
};
use common::TestNode;

async fn connected_pair(name: &str) -> (TestNode, TestNode, [u8; 32], [u8; 32]) {
    let mut alice = TestNode::spawn(&format!("{name}-alice"));
    let mut bob = TestNode::spawn(&format!("{name}-bob"));
    let alice_key = alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let link = alice.create_invite().await;
    bob.send(Command::AcceptInvite { link });
    bob.contact_added().await;
    alice.contact_added().await;
    (alice, bob, alice_key, bob_key)
}

#[tokio::test(flavor = "multi_thread")]
async fn file_round_trip_and_size_limit() {
    let (mut alice, mut bob, alice_key, bob_key) = connected_pair("files").await;
    let data = vec![7u8; 100_000];
    alice.send(Command::SendFile { contact: bob_key, name: "photo.png".into(), data: data.clone() });
    let id = bob
        .expect(|event| match event {
            Event::MessageAdded(message) if message.direction == Direction::Incoming => {
                let attachment = message.attachment.as_ref()?;
                assert_eq!((attachment.name.as_str(), attachment.size), ("photo.png", data.len()));
                Some(message.id)
            }
            _ => None,
        })
        .await;
    bob.send(Command::LoadAttachment { contact: alice_key, id });
    let received = bob
        .expect(|event| match event {
            Event::Attachment { data, .. } => Some(data.clone()),
            _ => None,
        })
        .await;
    assert_eq!(*received, data);

    alice.send(Command::SendFile { contact: bob_key, name: "huge.bin".into(), data: vec![1; MAX_FILE_BYTES + 1] });
    alice.expect_error().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_for_everyone_removes_on_both_sides() {
    let (mut alice, mut bob, alice_key, bob_key) = connected_pair("delete").await;
    let sent = alice.send_text(bob_key, "oops wrong chat").await;
    assert_eq!(bob.received_text().await, "oops wrong chat");

    // Bob cannot delete Alice's message for everyone (it is not his).
    bob.send(Command::DeleteMessages { contact: alice_key, ids: vec![sent], for_everyone: true });
    bob.expect_error().await;

    alice.send(Command::DeleteMessages { contact: bob_key, ids: vec![sent], for_everyone: true });
    let deleted = |event: &Event| match event {
        Event::MessagesDeleted { ids, .. } => Some(ids.clone()),
        _ => None,
    };
    assert_eq!(alice.expect(deleted).await, vec![sent]);
    assert_eq!(bob.expect(deleted).await, vec![sent]);
}

#[tokio::test(flavor = "multi_thread")]
async fn avatar_reaches_contact() {
    let (alice, mut bob, alice_key, _) = connected_pair("avatar").await;
    let image = vec![9u8; 2_000];
    alice.send(Command::SetAvatar { image: image.clone() });
    let seen = bob
        .expect(|event| match event {
            Event::AvatarChanged { user, image } if *user == alice_key => Some(image.clone()),
            _ => None,
        })
        .await;
    assert_eq!(seen, Some(image));
}

#[tokio::test(flavor = "multi_thread")]
async fn reactions_sync_both_ways_and_can_be_removed() {
    let (mut alice, mut bob, alice_key, bob_key) = connected_pair("reactions").await;
    let sent = alice.send_text(bob_key, "react to me").await;
    assert_eq!(bob.received_text().await, "react to me");

    bob.send(Command::React { contact: alice_key, id: sent, emoji: "👍".into() });
    let theirs = |event: &Event| match event {
        Event::ReactionsChanged { id, reactions, .. } if *id == sent => Some(reactions.clone()),
        _ => None,
    };
    let on_bob = bob.expect(theirs).await;
    assert_eq!((on_bob.mine.as_deref(), on_bob.theirs.as_deref()), (Some("👍"), None));
    let on_alice = alice.expect(theirs).await;
    assert_eq!((on_alice.mine.as_deref(), on_alice.theirs.as_deref()), (None, Some("👍")));

    // Alice reacts too, Bob removes his own.
    alice.send(Command::React { contact: bob_key, id: sent, emoji: "❤".into() });
    let both = bob.expect(theirs).await;
    assert_eq!((both.mine.as_deref(), both.theirs.as_deref()), (Some("👍"), Some("❤")));
    bob.send(Command::React { contact: alice_key, id: sent, emoji: String::new() });
    // Alice also gets the event for her own reaction first; wait for the removal.
    let removed = alice
        .expect(|event| theirs(event).filter(|reactions| reactions.theirs.is_none()))
        .await;
    assert_eq!((removed.mine.as_deref(), removed.theirs.as_deref()), (Some("❤"), None));

    bob.send(Command::React { contact: alice_key, id: sent, emoji: "two words".into() });
    bob.expect_error().await;
}
