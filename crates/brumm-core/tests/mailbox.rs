//! Offline delivery through a (local, in-memory) Nostr relay: sender and recipient are never
//! online at the same time, yet the message arrives and the sender later learns it was delivered.

mod common;

use std::time::Duration;

use brumm_core::api::{Command, DeliveryStatus};
use common::{TestNode, local_relay};

/// Time for Bob's node to post its receipt letter after processing the message.
const RECEIPT_POST_WAIT: Duration = Duration::from_secs(8);

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
    tokio::time::sleep(RECEIPT_POST_WAIT).await;
    let _bob_offline = bob.stop().await;

    // Alice comes back alone: she gets Bob's reply and the receipt for her message.
    let mut alice = alice_offline.start();
    alice.ready().await;
    assert_eq!(alice.received_text().await, "got it!");
    alice.delivered(sent).await;
}
