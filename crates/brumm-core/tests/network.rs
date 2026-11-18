//! Internet-dependent end-to-end tests (n0 relays + n0 DNS discovery). Ignored by default:
//! `cargo test -p brumm-core --test network -- --ignored --nocapture`

mod common;

use brumm_core::{api::Command, invite::Invite};
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

    bob.send(Command::SendText { contact: alice_key, text: "via relay".into() });
    assert_eq!(alice.received_text().await, "via relay");
    bob.delivered().await;
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

    bob.send(Command::SendText { contact: alice_key, text: "found you again".into() });
    assert_eq!(alice.received_text().await, "found you again");
    bob.delivered().await;

    alice.send(Command::SendText { contact: bob_key, text: "me too".into() });
    assert_eq!(bob.received_text().await, "me too");
    alice.delivered().await;
}
