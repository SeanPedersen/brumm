//! Multi-device end-to-end: linking with contact + history sync, fan-out to all devices of a
//! contact, own-device transcripts, and device removal (revocation).

mod common;

use brumm_core::api::{Command, Direction, Event};
use common::TestNode;

#[tokio::test(flavor = "multi_thread")]
async fn link_sync_fanout_and_revoke() {
    let mut laptop = TestNode::spawn("alice-laptop");
    let mut bob = TestNode::spawn("bob");
    let alice_key = laptop.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;

    // Alice (one device) and Bob become contacts and chat.
    let invite = laptop.create_invite().await;
    bob.send(Command::AcceptInvite { link: invite });
    bob.contact_added().await;
    laptop.contact_added().await;
    bob.send_text(alice_key, "before linking").await;
    assert_eq!(laptop.received_text().await, "before linking");

    // Alice links her phone: identity, contact and history arrive on the phone.
    let mut phone = TestNode::spawn("alice-phone");
    let (linked_key, phone_id) = laptop.link(&mut phone, 2).await;
    assert_eq!(linked_key, alice_key);
    assert_eq!(phone.contact_added().await.user_key, bob_key);
    let synced = phone
        .expect(|event| match event {
            Event::ConversationsChanged(contacts) => Some(contacts.clone()),
            _ => None,
        })
        .await;
    assert_eq!(synced, vec![bob_key]);
    // The manifest lets the phone tell when the initial history is complete.
    phone
        .expect(|event| match event {
            Event::HistorySync(pending) if pending.is_empty() => Some(()),
            _ => None,
        })
        .await;
    phone.send(Command::LoadConversation {
        contact: bob_key,
        limit: None,
    });
    let history = phone
        .expect(|event| match event {
            Event::Conversation { messages, .. } => Some(messages.clone()),
            _ => None,
        })
        .await;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].text, "before linking");

    // Bob learns Alice's new device list, then reaches both of her devices.
    bob.contact_added().await;
    bob.send_text(alice_key, "hi both").await;
    assert_eq!(laptop.received_text().await, "hi both");
    assert_eq!(phone.received_text().await, "hi both");

    // The phone replies; Bob gets it, the laptop gets a transcript of the sent message.
    let reply = phone.send_text(bob_key, "from my phone").await;
    assert_eq!(bob.received_text().await, "from my phone");
    assert_eq!(
        laptop.message_added(Direction::Outgoing).await,
        "from my phone"
    );
    phone.delivered(reply).await;

    // The laptop removes the phone: the phone is revoked, Bob stops delivering to it.
    laptop.send(Command::RemoveDevice {
        device_id: phone_id,
    });
    laptop.wait_devices(1).await;
    phone
        .expect(|event| matches!(event, Event::DeviceRevoked).then_some(()))
        .await;

    bob.contact_added().await;
    let last = bob.send_text(alice_key, "laptop only").await;
    assert_eq!(laptop.received_text().await, "laptop only");
    bob.delivered(last).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn notes_chat_syncs_between_own_devices_only() {
    let mut laptop = TestNode::spawn("notes-laptop");
    let alice_key = laptop.create_identity("Alice").await;
    let mut phone = TestNode::spawn("notes-phone");
    laptop.link(&mut phone, 2).await;

    let id = laptop.send_text(alice_key, "buy milk").await;
    assert_eq!(phone.message_added(Direction::Outgoing).await, "buy milk");

    phone.send(Command::DeleteMessages {
        contact: alice_key,
        ids: vec![id],
        for_everyone: false,
    });
    laptop
        .expect(|event| matches!(event, Event::MessagesDeleted { .. }).then_some(()))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn logout_removes_the_device_and_erases_it() {
    let mut laptop = TestNode::spawn("logout-laptop");
    let alice_key = laptop.create_identity("Alice").await;
    let mut phone = TestNode::spawn("logout-phone");
    let (_, phone_id) = laptop.link(&mut phone, 2).await;

    // The phone announces its own removal, then erases itself.
    phone.send(Command::LogOut);
    phone
        .expect(|event| matches!(event, Event::LoggedOut).then_some(()))
        .await;
    laptop
        .expect(|event| match event {
            Event::Devices(devices) if devices.iter().all(|d| d.device_id != phone_id) => Some(()),
            _ => None,
        })
        .await;

    // Nothing survives a restart: no identity, so onboarding again.
    let mut phone = phone.restart().await;
    let profile = phone
        .expect(|event| match event {
            Event::Ready { profile, .. } => Some(profile.clone()),
            _ => None,
        })
        .await;
    assert!(profile.is_none());

    // The erased device can be linked again (`Ready` was consumed above, so no `link` helper).
    phone.send(Command::StartLinking);
    let link = phone
        .expect(|event| match event {
            Event::LinkTicket { link } => Some(link.clone()),
            _ => None,
        })
        .await;
    laptop.send(Command::LinkDevice { link });
    let relinked_key = phone
        .expect(|event| match event {
            Event::IdentityCreated { profile, .. } => Some(profile.user_key),
            _ => None,
        })
        .await;
    assert_eq!(relinked_key, alice_key);
}
