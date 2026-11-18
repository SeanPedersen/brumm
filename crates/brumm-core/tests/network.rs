//! Internet-dependent end-to-end tests (n0 relays + n0 DNS discovery). Ignored by default:
//! `cargo test -p brumm-core --test network -- --ignored --nocapture`

mod common;

use brumm_core::{api::Command, invite::Invite, mailbox::default_relays};
use common::TestNode;

/// The invite carries only a relay URL (no IPs), so the first connection must go via the relay.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs internet"]
async fn invite_via_relay_only() {
    let mut alice = TestNode::spawn("relay-alice");
    let mut bob = TestNode::spawn("relay-bob");
    let alice_key = alice.create_identity("Alice").await;
    bob.create_identity("Bob").await;

    let mut invite = Invite::from_link(&alice.create_invite().await).unwrap();
    let relay = invite.device_addr.relay_urls().next().cloned().expect("alice has a home relay");
    println!("alice home relay: {relay}");
    invite.device_addr.addrs.retain(|addr| addr.is_relay());

    bob.send(Command::AcceptInvite { link: invite.to_link() });
    bob.contact_added().await;
    alice.contact_added().await;

    let sent = bob.send_text(alice_key, "via relay").await;
    assert_eq!(alice.received_text().await, "via relay");
    bob.delivered(sent).await;
}

/// After both restart, nodes know only each other's EndpointId; addresses must come from
/// pkarr records published to and resolved from n0's DNS server.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs internet"]
async fn reconnect_after_restart_via_dns_discovery() {
    let mut alice = TestNode::spawn("dns-alice");
    let mut bob = TestNode::spawn("dns-bob");
    let alice_key = alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;

    let link = alice.create_invite().await;
    bob.send(Command::AcceptInvite { link });
    bob.contact_added().await;
    alice.contact_added().await;

    let mut alice = alice.restart().await;
    let mut bob = bob.restart().await;
    alice.ready().await;
    bob.ready().await;

    let sent = bob.send_text(alice_key, "found you again").await;
    assert_eq!(alice.received_text().await, "found you again");
    bob.delivered(sent).await;

    let reply = alice.send_text(bob_key, "me too").await;
    assert_eq!(bob.received_text().await, "me too");
    alice.delivered(reply).await;
}

/// Bob is offline while Alice sends; Alice stays online and her outbox delivers once Bob is back.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs internet"]
async fn recipient_offline_then_back_while_sender_online() {
    let mut alice = TestNode::spawn("offline-alice");
    let mut bob = TestNode::spawn("offline-bob");
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let link = alice.create_invite().await;
    bob.send(Command::AcceptInvite { link });
    bob.contact_added().await;
    alice.contact_added().await;

    let bob_dir = bob.stop().await;
    let sent = alice.send_text(bob_key, "sent while you were away").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let mut bob = bob_dir.start();
    bob.ready().await;
    assert_eq!(bob.received_text().await, "sent while you were away");
    alice.delivered(sent).await;
}

/// Alice sends and goes offline *before* Bob comes back: the letter waits on public Nostr relays.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs internet"]
async fn offline_delivery_via_public_nostr_relays() {
    let mut alice = TestNode::spawn_with("public-mailbox-alice", default_relays());
    let mut bob = TestNode::spawn_with("public-mailbox-bob", default_relays());
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let link = alice.create_invite().await;
    bob.send(Command::AcceptInvite { link });
    bob.contact_added().await;
    alice.contact_added().await;

    let bob_offline = bob.stop().await;
    let sent = alice.send_text(bob_key, "stored on public relays").await;
    alice.status(sent, brumm_core::api::DeliveryStatus::Sent).await;
    let _alice_offline = alice.stop().await;

    let mut bob = bob_offline.start();
    bob.ready().await;
    assert_eq!(bob.received_text().await, "stored on public relays");
}
