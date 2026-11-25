//! Offline delivery through a (local, in-memory) Nostr relay: sender and recipient are never
//! online at the same time, yet the message arrives and the sender later learns it was delivered.

mod common;

use brumm_core::{
    api::{Command, DeliveryStatus},
    crypto::Sealed,
    mailbox::{self, Mailbox},
};
use common::{TestNode, local_relay};

#[tokio::test(flavor = "multi_thread")]
async fn never_online_together() {
    let (_relay, relays) = local_relay().await;
    let mut alice = TestNode::spawn_with("mailbox-alice", relays.clone());
    let mut bob = TestNode::spawn_with("mailbox-bob", relays);
    let alice_key = alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let link = alice.create_invite().await;
    bob.send(Command::AcceptInvite { link });
    bob.contact_added().await;
    alice.contact_added().await;

    // Bob goes offline; Alice's message lands in his mailbox; then Alice goes offline too.
    let bob_offline = bob.stop().await;
    let sent = alice.send_text(bob_key, "waiting for you on nostr").await;
    alice.status(sent, DeliveryStatus::Sent).await;
    let alice_offline = alice.stop().await;

    // Bob comes back alone and reads it from the relay.
    let mut bob = bob_offline.start();
    bob.ready().await;
    assert_eq!(bob.received_text().await, "waiting for you on nostr");

    // Bob replies while Alice is still offline, then leaves.
    let reply = bob.send_text(alice_key, "got it!").await;
    bob.status(reply, DeliveryStatus::Sent).await;
    let _bob_offline = bob.stop().await;

    // Alice comes back alone: she gets Bob's reply and the receipt for her message.
    let mut alice = alice_offline.start();
    alice.ready().await;
    assert_eq!(alice.received_text().await, "got it!");
    alice.delivered(sent).await;
}

/// A text far over one relay event goes as letter parts and is reassembled by the offline reader.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_text_reaches_an_offline_contact_in_parts() {
    let (_relay, relays) = local_relay().await;
    let mut alice = TestNode::spawn_with("parts-alice", relays.clone());
    let mut bob = TestNode::spawn_with("parts-bob", relays);
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let link = alice.create_invite().await;
    bob.send(Command::AcceptInvite { link });
    bob.contact_added().await;
    alice.contact_added().await;

    let bob_offline = bob.stop().await;
    let line = "## Route\nA line of a long markdown document about a route.\n\n";
    let long = line.repeat(50 * 1024 / line.len()).trim_end().to_owned();
    assert!(long.len() > 40 * 1024 && long.len() <= brumm_core::wire::MAX_TEXT_BYTES);
    let sent = alice.send_text(bob_key, &long).await;
    alice.status(sent, DeliveryStatus::Sent).await;
    let _alice_offline = alice.stop().await;

    let mut bob = bob_offline.start();
    bob.ready().await;
    assert_eq!(bob.received_text().await, long);
}

/// Notes written on one device reach a linked device that was offline, through the relay alone:
/// the writer is gone again before the other device starts (desktop → phone, both directions).
#[tokio::test(flavor = "multi_thread")]
async fn notes_reach_an_offline_own_device_through_the_relay() {
    let (_relay, relays) = local_relay().await;
    let mut laptop = TestNode::spawn_with("notes-relay-laptop", relays.clone());
    let notes = laptop.create_identity("Alice").await;
    let mut phone = TestNode::spawn_with("notes-relay-phone", relays);
    laptop.link(&mut phone, 2).await;

    let phone_offline = phone.stop().await;
    laptop.send_text(notes, "from the laptop").await;
    let _laptop_offline = laptop.stop().await;

    let mut phone = phone_offline.start();
    phone.ready().await;
    phone.wait_texts(notes, &["from the laptop"]).await;
}

/// Quitting right after sending must not strand the message: the shutdown drain posts it to the
/// relay although the direct attempt has not failed yet.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_sent_just_before_quitting_still_reaches_the_offline_contact() {
    let (_relay, relays) = local_relay().await;
    let mut alice = TestNode::spawn_with("quit-alice", relays.clone());
    let mut bob = TestNode::spawn_with("quit-bob", relays);
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let link = alice.create_invite().await;
    bob.send(Command::AcceptInvite { link });
    bob.contact_added().await;
    alice.contact_added().await;

    let bob_offline = bob.stop().await;
    alice.send_text(bob_key, "sent on the way out").await;
    let _alice_offline = alice.stop().await;

    let mut bob = bob_offline.start();
    bob.ready().await;
    assert_eq!(bob.received_text().await, "sent on the way out");
}

/// Notes tick like every chat: ✓ once the relay holds it for the other device, ✓✓ once that
/// device has it. A single device has nobody to wait for.
#[tokio::test(flavor = "multi_thread")]
async fn notes_show_sent_then_delivered_like_any_chat() {
    let (_relay, relays) = local_relay().await;
    let mut laptop = TestNode::spawn_with("notes-ticks-laptop", relays.clone());
    let notes = laptop.create_identity("Alice").await;
    let alone = laptop.send_text(notes, "only device").await;
    let stored = laptop.conversation(notes).await;
    assert_eq!(stored[0].id, alone);
    assert_eq!(stored[0].status, DeliveryStatus::Delivered);

    let mut phone = TestNode::spawn_with("notes-ticks-phone", relays);
    laptop.link(&mut phone, 2).await;
    let phone_offline = phone.stop().await;
    let note = laptop.send_text(notes, "wait for the phone").await;
    laptop.status(note, DeliveryStatus::Sent).await;

    let mut phone = phone_offline.start();
    phone.ready().await;
    laptop.delivered(note).await;
}

/// A reconnect (what a browser tab does when it comes back) keeps the inbox subscription alive:
/// a letter posted afterwards still arrives.
#[tokio::test(flavor = "multi_thread")]
async fn inbox_still_receives_after_a_reconnect() {
    let (_relay, relays) = local_relay().await;
    let reader_secret = [21; 32];
    let (letters_tx, mut letters) = tokio::sync::mpsc::channel(4);
    let (group_tx, _group_letters) = tokio::sync::mpsc::channel(4);
    let reader = Mailbox::start(&reader_secret, &relays, None, letters_tx, group_tx)
        .await
        .unwrap();
    reader.reconnect(None).await.unwrap();

    let (writer_tx, _writer_letters) = tokio::sync::mpsc::channel(4);
    let (writer_group_tx, _writer_group) = tokio::sync::mpsc::channel(4);
    let writer = Mailbox::start(&[22; 32], &relays, None, writer_tx, writer_group_tx)
        .await
        .unwrap();
    let to = mailbox::address(&reader_secret, relays.clone());
    let from = iroh::SecretKey::generate().public();
    let sealed = Sealed {
        message_type: 1,
        ciphertext: vec![7; 60 * 1024],
    };
    writer.post(&to, from, sealed.clone()).await.unwrap();
    let delivery = tokio::time::timeout(std::time::Duration::from_secs(10), letters.recv())
        .await
        .expect("letter after reconnect")
        .expect("open channel");
    assert_eq!((delivery.from, delivery.sealed), (from, sealed));
}
