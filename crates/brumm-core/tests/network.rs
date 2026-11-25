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
    let relay = invite
        .device_addr
        .relay_urls()
        .next()
        .cloned()
        .expect("alice has a home relay");
    println!("alice home relay: {relay}");
    invite.device_addr.addrs.retain(|addr| addr.is_relay());

    bob.send(Command::AcceptInvite {
        link: invite.to_link(),
    });
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

/// A 51 KB markdown note (like a pasted document) goes as letter parts to the public relays while
/// the reader is offline, and the reader assembles it alone after the writer is gone.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs internet"]
async fn long_text_in_parts_via_public_nostr_relays() {
    let mut alice = TestNode::spawn_with("public-parts-alice", default_relays());
    let mut bob = TestNode::spawn_with("public-parts-bob", default_relays());
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let link = alice.create_invite().await;
    bob.send(Command::AcceptInvite { link });
    bob.contact_added().await;
    alice.contact_added().await;

    let bob_offline = bob.stop().await;
    let line = "## Route\nA line of a long markdown document about a route.\n\n";
    let long = line.repeat(51 * 1024 / line.len()).trim_end().to_owned();
    let sent = alice.send_text(bob_key, &long).await;
    alice.status(sent, brumm_core::api::DeliveryStatus::Sent).await;
    let _alice_offline = alice.stop().await;

    let mut bob = bob_offline.start();
    bob.ready().await;
    assert_eq!(bob.received_text().await, long);
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
    alice
        .status(sent, brumm_core::api::DeliveryStatus::Sent)
        .await;
    let _alice_offline = alice.stop().await;

    let mut bob = bob_offline.start();
    bob.ready().await;
    assert_eq!(bob.received_text().await, "stored on public relays");
}

/// Groups over the real network: gossip and catch-up through n0's iroh relays and discovery,
/// then a group letter on the public Nostr relays for a member nobody could reach.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs internet"]
async fn group_over_relays_and_public_nostr() {
    let mut alice = TestNode::spawn_with("public-group-alice", default_relays());
    let mut bob = TestNode::spawn_with("public-group-bob", default_relays());
    let mut carol = TestNode::spawn_with("public-group-carol", default_relays());
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let carol_key = carol.create_identity("Carol").await;
    alice.befriend(&mut bob).await;
    alice.befriend(&mut carol).await;
    let group = alice
        .create_group("Out there", false, vec![bob_key, carol_key])
        .await;
    bob.join_group(group).await;
    carol.join_group(group).await;
    alice.group_members(group, 3).await;

    alice.send_group_text(group, "live").await;
    bob.wait_group_texts(group, &["live"]).await;
    carol.wait_group_texts(group, &["live"]).await;

    // Carol away: Bob's message reaches her later from Alice's log (catch-up via relays).
    let carol_offline = carol.stop().await;
    bob.send_group_text(group, "while you were out").await;
    alice.wait_group_texts(group, &["live", "while you were out"]).await;
    let mut carol = carol_offline.start();
    carol.ready().await;
    carol
        .wait_group_texts(group, &["live", "while you were out"])
        .await;

    // Everyone else gone: Carol's next message only exists as a Nostr letter.
    let _alice_offline = alice.stop().await;
    let bob_offline = bob.stop().await;
    let sent = carol.send_group_text(group, "on nostr").await;
    carol
        .status(sent, brumm_core::api::DeliveryStatus::Sent)
        .await;
    let _carol_offline = carol.stop().await;
    let mut bob = bob_offline.start();
    bob.ready().await;
    bob.wait_group_texts(group, &["live", "while you were out", "on nostr"])
        .await;
}
