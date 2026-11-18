//! End-to-end on one machine: invite, chat both ways, invite reuse rejection.
//! Nodes typically connect over local direct addresses here.

mod common;

use brumm_core::api::Command;
use common::TestNode;

#[tokio::test(flavor = "multi_thread")]
async fn invite_then_chat() {
    let mut alice = TestNode::spawn("alice");
    let mut bob = TestNode::spawn("bob");
    let alice_key = alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;

    let link = alice.create_invite().await;
    bob.send(Command::AcceptInvite { link: link.clone() });
    let contact = bob.contact_added().await;
    assert_eq!((contact.user_key, contact.display_name.as_str()), (alice_key, "Alice"));
    assert_eq!(alice.contact_added().await.user_key, bob_key);

    bob.send(Command::SendText { contact: alice_key, text: "hi alice".into() });
    assert_eq!(alice.received_text().await, "hi alice");
    bob.delivered().await;

    alice.send(Command::SendText { contact: bob_key, text: "hi bob".into() });
    assert_eq!(bob.received_text().await, "hi bob");
    alice.delivered().await;

    bob.send(Command::AcceptInvite { link });
    let error = bob.expect_error().await;
    assert!(error.contains("already used"), "unexpected error: {error}");
}
