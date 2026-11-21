//! Own devices converge on the same messages, in the same order, whatever got lost on the way:
//! dropped envelopes (both directions), a lost delete, a receiver that lost its Olm sessions,
//! and notes that only reach the other device through the Nostr mailbox.
//!
//! Losses are simulated by editing a stopped node's database (`Stopped::sql`).

mod common;

use std::time::Duration;

use brumm_core::api::{Command, Direction, Event};
use common::{TEST_RECONCILE_INTERVAL, TestNode, local_relay, texts};

/// Enough consistency rounds for a wrongly re-inserted message to show up.
const SETTLE_ROUNDS: u32 = 4;
const MAILBOX_POLL: Duration = Duration::from_millis(200);

/// Laptop + phone of one user, linked and both online.
async fn linked_pair(name: &str, relays: Option<Vec<String>>) -> (TestNode, TestNode, [u8; 32]) {
    let spawn = |device: &str| match &relays {
        Some(relays) => TestNode::spawn_with(&format!("{name}-{device}"), relays.clone()),
        None => TestNode::spawn(&format!("{name}-{device}")),
    };
    let mut laptop = spawn("laptop");
    let notes = laptop.create_identity("Alice").await;
    let mut phone = spawn("phone");
    laptop.link(&mut phone, 2).await;
    (laptop, phone, notes)
}

#[tokio::test(flavor = "multi_thread")]
async fn notes_lost_in_both_directions_are_restored_in_order() {
    let (mut laptop, mut phone, notes) = linked_pair("lost-notes", None).await;
    let one = laptop.send_text(notes, "one").await;
    assert_eq!(phone.message_added(Direction::Outgoing).await, "one");
    // Acked before the phone stops, or "one" would still be queued below.
    laptop.delivered(one).await;

    // Each device writes a note while the other is offline...
    let phone_offline = phone.stop().await;
    laptop.send_text(notes, "two").await;
    let laptop_offline = laptop.stop().await;
    let mut phone = phone_offline.start();
    phone.ready().await;
    phone.send_text(notes, "three").await;
    let phone_offline = phone.stop().await;

    // ...and the queued envelopes are lost (the old bug: a rejection dropped them).
    assert_eq!(laptop_offline.sql("DELETE FROM outbox"), 1);
    assert_eq!(phone_offline.sql("DELETE FROM outbox"), 1);

    let mut laptop = laptop_offline.start();
    let mut phone = phone_offline.start();
    laptop.wait_texts(notes, &["one", "two", "three"]).await;
    phone.wait_texts(notes, &["one", "two", "three"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_delete_is_repaired_and_never_undone() {
    let (mut laptop, mut phone, notes) = linked_pair("lost-delete", None).await;
    let gone = laptop.send_text(notes, "delete me").await;
    laptop.send_text(notes, "keep me").await;
    phone.wait_texts(notes, &["delete me", "keep me"]).await;

    let phone_offline = phone.stop().await;
    laptop.send(Command::DeleteMessages {
        contact: notes,
        ids: vec![gone],
        for_everyone: false,
    });
    laptop
        .expect(|event| matches!(event, Event::MessagesDeleted { .. }).then_some(()))
        .await;
    let laptop_offline = laptop.stop().await;
    assert_eq!(
        laptop_offline.sql("DELETE FROM outbox"),
        1,
        "the DeleteSync"
    );

    // The phone still has the note: the check must delete it there, not bring it back here.
    let mut laptop = laptop_offline.start();
    let mut phone = phone_offline.start();
    let deleted = phone
        .expect(|event| match event {
            Event::MessagesDeleted { ids, .. } => Some(ids.clone()),
            _ => None,
        })
        .await;
    assert_eq!(deleted, vec![gone]);
    tokio::time::sleep(TEST_RECONCILE_INTERVAL * SETTLE_ROUNDS).await;
    assert_eq!(texts(&laptop.conversation(notes).await), ["keep me"]);
    assert_eq!(texts(&phone.conversation(notes).await), ["keep me"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_lost_its_sessions_still_gets_queued_notes() {
    let (mut laptop, mut phone, notes) = linked_pair("lost-session", None).await;
    // Messages both ways, so the laptop encrypts with normal (not pre-key) Olm messages.
    laptop.send_text(notes, "one").await;
    assert_eq!(phone.message_added(Direction::Outgoing).await, "one");
    phone.send_text(notes, "two").await;
    assert_eq!(laptop.message_added(Direction::Outgoing).await, "two");
    laptop.send_text(notes, "three").await;
    assert_eq!(phone.message_added(Direction::Outgoing).await, "three");

    // Sealed for the phone's current session while it is offline; then the phone loses it.
    let phone_offline = phone.stop().await;
    laptop.send_text(notes, "four").await;
    assert!(phone_offline.sql("DELETE FROM olm_sessions") > 0);

    let mut phone = phone_offline.start();
    phone
        .wait_texts(notes, &["one", "two", "three", "four"])
        .await;
    // And the devices keep talking afterwards, in both directions.
    phone.send_text(notes, "five").await;
    laptop
        .wait_texts(notes, &["one", "two", "three", "four", "five"])
        .await;
    laptop.send_text(notes, "six").await;
    phone
        .wait_texts(notes, &["one", "two", "three", "four", "five", "six"])
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_contact_that_lost_its_sessions_still_gets_the_message() {
    let mut alice = TestNode::spawn("lost-session-alice");
    let mut bob = TestNode::spawn("lost-session-bob");
    let alice_key = alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let link = alice.create_invite().await;
    bob.send(Command::AcceptInvite { link });
    bob.contact_added().await;
    alice.contact_added().await;
    bob.send_text(alice_key, "hi").await;
    assert_eq!(alice.received_text().await, "hi");
    let first = alice.send_text(bob_key, "hello").await;
    alice.delivered(first).await;

    let bob_offline = bob.stop().await;
    let queued = alice.send_text(bob_key, "while you were away").await;
    assert!(bob_offline.sql("DELETE FROM olm_sessions") > 0);

    let mut bob = bob_offline.start();
    assert_eq!(bob.received_text().await, "while you were away");
    alice.delivered(queued).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn notes_through_the_mailbox_arrive_once_and_in_order() {
    let (_relay, relays) = local_relay().await;
    let (mut laptop, phone, notes) = linked_pair("mailbox-notes", Some(relays)).await;
    let expected = ["a", "b", "c", "d", "e"];

    // The phone is offline: every note goes to its mailbox; then the laptop leaves too.
    let phone_offline = phone.stop().await;
    for text in expected {
        laptop.send_text(notes, text).await;
    }
    // Linking may have left envelopes for the phone queued as well: wait until all are posted.
    let all_mailboxed = format!(
        "SELECT count(*) >= {} AND min(mailboxed) = 1 FROM outbox",
        expected.len()
    );
    let deadline = tokio::time::Instant::now() + common::EVENT_TIMEOUT;
    while laptop.sql_number(&all_mailboxed) == 0 {
        let posted = laptop.sql_number("SELECT count(*) FROM outbox WHERE mailboxed = 1");
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {posted} queued envelopes (with {} notes) reached the mailbox",
            expected.len()
        );
        tokio::time::sleep(MAILBOX_POLL).await;
    }
    let laptop_offline = laptop.stop().await;

    // The phone reads them from the relay alone...
    let mut phone = phone_offline.start();
    phone.wait_texts(notes, &expected).await;

    // ...and with the laptop back (direct retries + consistency rounds), nothing is doubled.
    let mut laptop = laptop_offline.start();
    laptop.ready().await;
    tokio::time::sleep(TEST_RECONCILE_INTERVAL * SETTLE_ROUNDS).await;
    phone.wait_texts(notes, &expected).await;
    laptop.wait_texts(notes, &expected).await;
}
