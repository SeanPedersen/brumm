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
    assert_eq!(
        (contact.user_key, contact.display_name.as_str()),
        (alice_key, "Alice")
    );
    assert_eq!(alice.contact_added().await.user_key, bob_key);

    let to_alice = bob.send_text(alice_key, "hi alice").await;
    assert_eq!(alice.received_text().await, "hi alice");
    bob.delivered(to_alice).await;

    let to_bob = alice.send_text(bob_key, "hi bob").await;
    assert_eq!(bob.received_text().await, "hi bob");
    alice.delivered(to_bob).await;

    bob.send(Command::AcceptInvite { link });
    let error = bob.expect_error().await;
    assert!(error.contains("already used"), "unexpected error: {error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn last_opened_chat_is_restored_on_startup() {
    let node = TestNode::spawn("last-chat");
    let mut node = node;
    let me = node.create_identity("Ada").await;
    // Remember, then load: the actor applies commands in order, so the meta write is committed
    // before Conversation is emitted and the restart begins.
    node.send(Command::RememberOpenChat { contact: me });
    node.send(Command::LoadConversation {
        contact: me,
        limit: None,
    });
    node.expect(|event| {
        matches!(event, brumm_core::api::Event::Conversation { contact, .. } if *contact == me)
            .then_some(())
    })
    .await;

    let mut node = node.restart().await;
    assert_eq!(node.ready_last_opened().await, Some(me));
}
