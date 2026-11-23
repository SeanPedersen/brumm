//! End-to-end tests of the async `Client`: real cores on local data dirs, talking over iroh.

use std::{path::PathBuf, time::Duration};

use brumm_core::{
    api::{ConversationId, DeliveryStatus},
    node::NodeConfig,
};
use brumm_host::{Client, Outgoing};

/// Nothing listens here: these tests deliver directly, never through a mailbox.
const NO_RELAY: &str = "ws://127.0.0.1:9";
const TEST_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(45);

fn config(name: &str) -> NodeConfig {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let data_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("host-{name}-{unique}"));
    NodeConfig {
        retry_interval: TEST_RETRY_INTERVAL,
        mailbox_relays: vec![NO_RELAY.to_owned()],
        ..NodeConfig::new(data_dir)
    }
}

async fn identity(name: &str) -> Client {
    let client = Client::open(config(name)).await.unwrap();
    client.create_identity(name).await.unwrap();
    client
}

fn text(body: &str) -> Outgoing {
    Outgoing::Text(body.to_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn send_then_wait_for_delivered() {
    let alice = identity("alice").await;
    let bob = identity("bob").await;
    let link = alice.create_invite().await.unwrap();
    let contact = bob.accept_invite(&link).await.unwrap();
    assert_eq!(contact.display_name, "alice");
    let chat = ConversationId::Direct(contact.user_key);

    let id = bob.send(chat, text("hello from a bot"), None).await.unwrap();
    let status = bob
        .wait_status(chat, id, DeliveryStatus::Delivered, DELIVERY_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(status, DeliveryStatus::Delivered);
    // Already there: answered from the stored status, without a new event.
    let again = bob
        .wait_status(chat, id, DeliveryStatus::Sent, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(again, DeliveryStatus::Delivered);

    let bob_chat = ConversationId::Direct(bob.profile().unwrap().user_key);
    let page = alice.read_since(bob_chat, None, None).await.unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].id, id);
    assert_eq!(page.messages[0].text, "hello from a bot");

    let unknown = bob
        .wait_status(chat, [9; 16], DeliveryStatus::Sent, Duration::from_secs(5))
        .await;
    assert!(unknown.is_err());
    alice.shutdown().await.unwrap();
    bob.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_reads_across_pages() {
    let client = identity("reader").await;
    let notes = client.notes().unwrap();
    for index in 0..5 {
        client.send(notes, text(&format!("note {index}")), None).await.unwrap();
    }

    let mut cursor = None;
    let mut texts = Vec::new();
    let mut pages = 0;
    loop {
        let page = client.read_since(notes, cursor, Some(2)).await.unwrap();
        pages += 1;
        texts.extend(page.messages.iter().map(|message| message.text.clone()));
        cursor = Some(page.cursor);
        if !page.has_more {
            break;
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(texts, (0..5).map(|index| format!("note {index}")).collect::<Vec<_>>());

    // Nothing new: the cursor stays put; a later message continues from it.
    let idle = client.read_since(notes, cursor, Some(2)).await.unwrap();
    assert!(idle.messages.is_empty());
    assert_eq!(Some(idle.cursor), cursor);
    client.send(notes, text("note 5"), None).await.unwrap();
    let next = client.read_since(notes, cursor, None).await.unwrap();
    assert_eq!(next.messages.len(), 1);
    assert_eq!(next.messages[0].text, "note 5");

    // The cursor outlives the process.
    let config = NodeConfig {
        retry_interval: TEST_RETRY_INTERVAL,
        mailbox_relays: vec![NO_RELAY.to_owned()],
        ..NodeConfig::new(client.data_dir().to_owned())
    };
    client.shutdown().await.unwrap();
    let reopened = Client::open(config).await.unwrap();
    let after_restart = reopened.read_since(notes, Some(next.cursor), None).await.unwrap();
    assert!(after_restart.messages.is_empty());
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_with_the_same_id_stores_once() {
    let client = identity("retry").await;
    let notes = client.notes().unwrap();
    let id = [42; 16];
    assert_eq!(client.send(notes, text("once"), Some(id)).await.unwrap(), id);
    assert_eq!(client.send(notes, text("once"), Some(id)).await.unwrap(), id);
    // A retry carrying a different body still does nothing: the id decides.
    assert_eq!(client.send(notes, text("twice?"), Some(id)).await.unwrap(), id);
    let page = client.read_since(notes, None, None).await.unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].text, "once");

    // A failed send says why.
    let stranger = ConversationId::Direct([7; 32]);
    let error = client.send(stranger, text("hi"), None).await.unwrap_err();
    assert!(format!("{error:#}").contains("unknown contact"), "{error:#}");
    client.shutdown().await.unwrap();
}
