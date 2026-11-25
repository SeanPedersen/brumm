//! Offline delivery through public Nostr relays (SPEC.md §8).
//!
//! Every device has an **inbox key** (secp256k1, derived from its device secret) and a relay
//! list, published to contacts inside its signed [`crate::identity::DeviceEntry`]. When a
//! device is unreachable, the sender posts the already Olm-sealed envelope as a "letter":
//!
//! - event kind 1059 (gift wrap), signed by a **fresh random key** per letter,
//! - `p` tag = recipient inbox key (relays route by it; it is not a user identity),
//! - content = NIP-44(ephemeral → inbox key) of `{ sender device, sealed }`,
//! - NIP-40 expiration after [`LETTER_TTL`].
//!
//! Relays therefore see neither sender nor recipient identity, only an inbox key and timing.
//!
//! A payload larger than one event ([`MAX_LETTER_BYTES`]) goes as up to [`parts::MAX_PARTS`]
//! **letter parts** under the same tag (`part:` + base64 of a [`parts::LetterPart`]); the receiver
//! reassembles and checks them before handling the letter, so a long text or a photo still
//! reaches an offline device. Each part is sealed to a fresh key like any letter.
//!
//! **Group letters** (SPEC.md §14.4, item 3) carry one encoded group unit for every member at
//! once: the `p` tag is a key derived from the epoch secret ([`group_tag_keys`]), so it changes
//! with every epoch and relays cannot link epochs; members subscribe to the tags of the epochs
//! they hold. Control units (rekeys, key requests) use a tag derived from the group id.
//! **Log chunks** (a small group's op log for a newcomer, `group::join::ChunkSet`) use a tag
//! from a random seed in the welcome, on a subscription of their own.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use data_encoding::BASE64;
use iroh::EndpointId;
use nostr::nips::nip44;
use nostr_sdk::prelude::{
    Client, ClientNotification, Event, EventBuilder, Filter, FinalizeEvent, Keys, Kind, PublicKey,
    SecretKey, SubscriptionId, Tag, Timestamp,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{crypto::Sealed, wire};

mod parts;

use parts::{Assembler, LetterPart};

pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
];
pub const MAX_RELAYS: usize = 3;
pub const MAX_RELAY_URL_CHARS: usize = 128;
const INBOX_KEY_CONTEXT: &str = "brumm 2026 nostr inbox key v1";
const GROUP_TAG_CONTEXT: &str = "brumm 2026 group letter tag v1";
/// One REQ for all group tags; sending it again replaces the previous filter on the relays.
const GROUP_SUBSCRIPTION_ID: &str = "brumm-groups";
const LOG_SUBSCRIPTION_ID: &str = "brumm-group-logs";
/// Fixed so a [`Mailbox::reconnect`] replaces the inbox filter instead of adding a second one.
const INBOX_SUBSCRIPTION_ID: &str = "brumm-inbox";
const LETTER_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// Payload of one letter. Base64, NIP-44 padding and base64 again make the event about 2.2×
/// larger (≈ 55 KB here), under the 64 KiB event cap of common relays (strfry's default).
const MAX_LETTER_BYTES: usize = 24 * 1024 + 512;
/// Slice of a larger payload per part; the part's own framing stays under [`MAX_LETTER_BYTES`].
const PART_BYTES: usize = 24 * 1024;
/// Marks a part's plaintext. `:` is not in the base64 alphabet, so a whole letter never starts so.
const PART_PREFIX: &str = "part:";
/// Largest payload the mailbox carries (direct letter or group unit); larger only goes direct.
pub const MAX_MAILBOX_BYTES: usize = PART_BYTES * parts::MAX_PARTS;
const CONNECT_WAIT: Duration = Duration::from_secs(5);
/// Re-fetch a little before the last seen letter to tolerate relay clock skew.
const SINCE_MARGIN_SECS: u64 = 10 * 60;

/// Where a device receives letters: its inbox public key and relays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxAddress {
    pub inbox_key: [u8; 32],
    pub relays: Vec<String>,
}

impl MailboxAddress {
    pub fn is_valid(&self) -> bool {
        !self.relays.is_empty()
            && self.relays.len() <= MAX_RELAYS
            && self.relays.iter().all(|relay| {
                relay.len() <= MAX_RELAY_URL_CHARS
                    && (relay.starts_with("wss://") || relay.starts_with("ws://"))
            })
            && PublicKey::from_slice(&self.inbox_key).is_ok()
    }
}

pub fn default_relays() -> Vec<String> {
    DEFAULT_RELAYS
        .iter()
        .map(|relay| (*relay).to_owned())
        .collect()
}

/// The inbox key pair of the device with `device_secret`.
pub fn inbox_keys(device_secret: &[u8; 32]) -> Keys {
    let secret = blake3::derive_key(INBOX_KEY_CONTEXT, device_secret);
    Keys::new(
        SecretKey::from_slice(&secret)
            .expect("blake3 output is a valid secp256k1 key with overwhelming probability"),
    )
}

pub fn address(device_secret: &[u8; 32], relays: Vec<String>) -> MailboxAddress {
    MailboxAddress {
        inbox_key: inbox_keys(device_secret).public_key().to_bytes(),
        relays,
    }
}

/// Plaintext of a letter (inside NIP-44).
#[derive(Debug, Serialize, Deserialize)]
struct Letter {
    sender: EndpointId,
    sealed: Sealed,
}

/// The tag key pair of a group letter, from an epoch secret or (control units) the group id.
pub fn group_tag_keys(seed: &[u8; 32]) -> Keys {
    let secret = blake3::derive_key(GROUP_TAG_CONTEXT, seed);
    Keys::new(
        SecretKey::from_slice(&secret)
            .expect("blake3 output is a valid secp256k1 key with overwhelming probability"),
    )
}

/// A letter fetched from our inbox.
pub struct MailboxDelivery {
    pub from: EndpointId,
    pub sealed: Sealed,
    pub posted_at_secs: u64,
}

/// A group letter: one encoded group unit, still to be verified by the node.
pub struct GroupLetter {
    pub unit: Vec<u8>,
    pub posted_at_secs: u64,
    /// An encoded `group::join::LogChunk`, not a group unit.
    pub log_chunk: bool,
}

/// Group tag secrets by their public key, shared with the notification loop.
type GroupTags = Arc<Mutex<HashMap<PublicKey, Keys>>>;

pub struct Mailbox {
    client: Client,
    inbox: PublicKey,
    relays: Vec<String>,
    group_tags: GroupTags,
    log_tags: GroupTags,
}

impl Mailbox {
    /// Connects to our relays and streams letters for our inbox (posted after `since_secs`)
    /// into `deliveries` until shut down.
    pub async fn start(
        device_secret: &[u8; 32],
        relays: &[String],
        since_secs: Option<u64>,
        deliveries: mpsc::Sender<MailboxDelivery>,
        group_letters: mpsc::Sender<GroupLetter>,
    ) -> Result<Arc<Self>> {
        let inbox = inbox_keys(device_secret);
        let client = Client::default();
        for relay in relays {
            client
                .add_relay(relay.as_str())
                .await
                .with_context(|| format!("add relay {relay}"))?;
        }
        client.connect().and_wait(CONNECT_WAIT).await;

        let mut notifications = client.notifications();
        let inbox_public = inbox.public_key();
        subscribe_inbox(&client, inbox_public, since_secs).await?;

        let group_tags = GroupTags::default();
        let tags = group_tags.clone();
        let log_tags = GroupTags::default();
        let chunk_tags = log_tags.clone();
        n0_future::task::spawn(async move {
            use nostr_sdk::prelude::StreamExt;
            // One per channel: a part only completes a payload of its own kind.
            let mut inbox_parts = Assembler::default();
            let mut group_parts = Assembler::default();
            let mut log_parts = Assembler::default();
            while let Some(notification) = notifications.next().await {
                let ClientNotification::Event { event, .. } = notification else {
                    continue;
                };
                let tag = group_tag_of(&chunk_tags, &event)
                    .map(|tag| (tag, true))
                    .or_else(|| group_tag_of(&tags, &event).map(|tag| (tag, false)));
                if let Some((tag, log_chunk)) = tag {
                    let assembler = if log_chunk {
                        &mut log_parts
                    } else {
                        &mut group_parts
                    };
                    match open_payload(&tag, &event, assembler) {
                        Ok(Some((unit, posted_at_secs))) => {
                            let letter = GroupLetter {
                                unit,
                                posted_at_secs,
                                log_chunk,
                            };
                            if group_letters.send(letter).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(err) => tracing::debug!(?err, "ignoring unreadable group letter"),
                    }
                    continue;
                }
                match open_letter(&inbox, &event, &mut inbox_parts) {
                    Ok(Some(delivery)) => {
                        if deliveries.send(delivery).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(err) => tracing::debug!(?err, "ignoring unreadable letter"),
                }
            }
        });
        Ok(Arc::new(Self {
            client,
            inbox: inbox_public,
            relays: relays.to_vec(),
            group_tags,
            log_tags,
        }))
    }

    /// Listens for group letters under exactly these tags (posted after `since_secs`).
    pub async fn set_group_tags(&self, tags: Vec<Keys>, since_secs: Option<u64>) -> Result<()> {
        let publics: Vec<PublicKey> = tags.iter().map(Keys::public_key).collect();
        *self.group_tags.lock().expect("lock poisoned") = tags
            .into_iter()
            .map(|keys| (keys.public_key(), keys))
            .collect();
        let id = SubscriptionId::new(GROUP_SUBSCRIPTION_ID);
        if publics.is_empty() {
            let _ = self.client.unsubscribe(&id).await;
            return Ok(());
        }
        let mut filter = Filter::new().kind(Kind::GiftWrap).pubkeys(publics);
        if let Some(since) = since_secs {
            filter = filter.since(Timestamp::from(since.saturating_sub(SINCE_MARGIN_SECS)));
        }
        self.client
            .subscribe(filter)
            .with_id(id)
            .await
            .context("subscribe to group tags")?;
        Ok(())
    }

    /// Listens for log chunk letters under these tags (posted after `since_secs`).
    pub async fn set_log_tags(&self, tags: Vec<Keys>, since_secs: u64) -> Result<()> {
        let publics: Vec<PublicKey> = tags.iter().map(Keys::public_key).collect();
        *self.log_tags.lock().expect("lock poisoned") = tags
            .into_iter()
            .map(|keys| (keys.public_key(), keys))
            .collect();
        let id = SubscriptionId::new(LOG_SUBSCRIPTION_ID);
        if publics.is_empty() {
            let _ = self.client.unsubscribe(&id).await;
            return Ok(());
        }
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkeys(publics)
            .since(Timestamp::from(since_secs.saturating_sub(SINCE_MARGIN_SECS)));
        self.client
            .subscribe(filter)
            .with_id(id)
            .await
            .context("subscribe to log chunks")?;
        Ok(())
    }

    /// Posts one group unit under `tag` to our relays and `extra_relays` (other members').
    pub async fn post_group(&self, tag: &Keys, unit: &[u8], extra_relays: &[String]) -> Result<()> {
        let events = seal_payload(tag.public_key(), unit).context("group letter")?;
        let mut targets: Vec<&str> = self.relays.iter().map(String::as_str).collect();
        for relay in extra_relays {
            if !targets.contains(&relay.as_str()) {
                targets.push(relay);
            }
        }
        for relay in &targets {
            self.client
                .add_relay(*relay)
                .await
                .with_context(|| format!("add relay {relay}"))?;
        }
        self.client.connect().and_wait(CONNECT_WAIT).await;
        self.send_all(&events, &targets).await
    }

    /// Sends every event (a letter or all its parts); each must reach at least one relay.
    async fn send_all(&self, events: &[Event], targets: &[&str]) -> Result<()> {
        for event in events {
            let output = self
                .client
                .send_event(event)
                .to(targets.iter().copied())
                .await?;
            if output.success.is_empty() {
                bail!("no relay accepted the letter: {:?}", output.failed);
            }
        }
        Ok(())
    }

    /// Posts `sealed` to the recipient's inbox; succeeds if at least one relay accepted it.
    pub async fn post(&self, to: &MailboxAddress, from: EndpointId, sealed: Sealed) -> Result<()> {
        let events = seal_letter(
            to,
            Letter {
                sender: from,
                sealed,
            },
        )?;
        for relay in &to.relays {
            self.client
                .add_relay(relay.as_str())
                .await
                .with_context(|| format!("add relay {relay}"))?;
        }
        self.client.connect().and_wait(CONNECT_WAIT).await;
        let targets: Vec<&str> = to.relays.iter().map(String::as_str).collect();
        self.send_all(&events, &targets).await
    }

    /// Drops and reopens every relay connection, then asks again for letters posted after
    /// `since_secs`. The browser cannot ping its WebSockets, so a tab that was suspended or
    /// changed networks may hold a dead connection that never reports an error; this is how it
    /// catches up. The group and log filters are sent again on reconnect as they were.
    pub async fn reconnect(&self, since_secs: Option<u64>) -> Result<()> {
        self.client.disconnect().await;
        self.client.connect().and_wait(CONNECT_WAIT).await;
        subscribe_inbox(&self.client, self.inbox, since_secs).await
    }

    pub async fn shutdown(&self) {
        self.client.shutdown().await;
    }
}

async fn subscribe_inbox(client: &Client, inbox: PublicKey, since_secs: Option<u64>) -> Result<()> {
    let mut filter = Filter::new().kind(Kind::GiftWrap).pubkey(inbox);
    if let Some(since) = since_secs {
        filter = filter.since(Timestamp::from(since.saturating_sub(SINCE_MARGIN_SECS)));
    }
    client
        .subscribe(filter)
        .with_id(SubscriptionId::new(INBOX_SUBSCRIPTION_ID))
        .await
        .context("subscribe to inbox")?;
    Ok(())
}

fn seal_letter(to: &MailboxAddress, letter: Letter) -> Result<Vec<Event>> {
    let recipient = PublicKey::from_slice(&to.inbox_key)?;
    seal_payload(recipient, &wire::encode(&letter))
}

/// One letter for `payload`, or its parts when it is larger than one event.
fn seal_payload(recipient: PublicKey, payload: &[u8]) -> Result<Vec<Event>> {
    if payload.len() <= MAX_LETTER_BYTES {
        return Ok(vec![seal_event(recipient, BASE64.encode(payload))?]);
    }
    let parts = parts::split(payload, PART_BYTES).with_context(|| {
        format!(
            "{} bytes is too large for the mailbox (limit {MAX_MAILBOX_BYTES})",
            payload.len()
        )
    })?;
    parts
        .iter()
        .map(|part| seal_event(recipient, format!("{PART_PREFIX}{}", BASE64.encode(&wire::encode(part)))))
        .collect()
}

/// A gift wrap of `plaintext` to `recipient`, signed by a fresh key.
fn seal_event(recipient: PublicKey, plaintext: String) -> Result<Event> {
    let ephemeral = Keys::generate();
    let content = nip44::encrypt(ephemeral.secret_key(), &recipient, plaintext, nip44::Version::V2)?;
    let expires = Timestamp::now() + LETTER_TTL;
    Ok(EventBuilder::new(Kind::GiftWrap, content)
        .tags([Tag::public_key(recipient), Tag::expiration(expires)])
        .finalize(&ephemeral)?)
}

fn group_tag_of(tags: &GroupTags, event: &Event) -> Option<Keys> {
    let tags = tags.lock().expect("lock poisoned");
    event
        .tags
        .public_keys()
        .find_map(|public| tags.get(&public).cloned())
}

/// The payload of a letter and its post time; `None` while it is a part of an unfinished one.
fn open_payload(
    keys: &Keys,
    event: &Event,
    assembler: &mut Assembler,
) -> Result<Option<(Vec<u8>, u64)>> {
    event.verify()?;
    let plaintext = nip44::decrypt(keys.secret_key(), &event.pubkey, &event.content)?;
    let posted_at_secs = event.created_at.as_secs();
    let Some(encoded) = plaintext.strip_prefix(PART_PREFIX) else {
        return Ok(Some((BASE64.decode(plaintext.as_bytes())?, posted_at_secs)));
    };
    let part: LetterPart = wire::decode(&BASE64.decode(encoded.as_bytes())?)?;
    Ok(assembler.insert(part, PART_BYTES, posted_at_secs))
}

fn open_letter(
    inbox: &Keys,
    event: &Event,
    assembler: &mut Assembler,
) -> Result<Option<MailboxDelivery>> {
    let Some((payload, posted_at_secs)) = open_payload(inbox, event, assembler)? else {
        return Ok(None);
    };
    let letter: Letter = wire::decode(&payload)?;
    Ok(Some(MailboxDelivery {
        from: letter.sender,
        sealed: letter.sealed,
        posted_at_secs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed(len: usize) -> Sealed {
        Sealed {
            message_type: 1,
            ciphertext: (0..len).map(|index| (index % 251) as u8).collect(),
        }
    }

    #[test]
    fn letter_roundtrip_and_privacy() {
        let device_secret = [7; 32];
        let to = address(&device_secret, default_relays());
        assert!(to.is_valid());
        let sender = iroh::SecretKey::generate().public();
        let sealed = Sealed {
            message_type: 1,
            ciphertext: b"olm-ciphertext".to_vec(),
        };
        let events = seal_letter(
            &to,
            Letter {
                sender,
                sealed: sealed.clone(),
            },
        )
        .unwrap();
        let [event] = events.as_slice() else {
            panic!("a small letter is one event");
        };

        assert!(!event.content.contains("olm-ciphertext"));
        assert_ne!(event.pubkey.to_bytes(), to.inbox_key, "signed by an ephemeral key");
        let opened = open_letter(&inbox_keys(&device_secret), event, &mut Assembler::default())
            .unwrap()
            .expect("whole letter");
        assert_eq!((opened.from, opened.sealed), (sender, sealed));
        assert!(open_letter(&inbox_keys(&[8; 32]), event, &mut Assembler::default()).is_err());
    }

    #[test]
    fn a_large_letter_goes_in_parts_and_reassembles_in_any_order() {
        let device_secret = [9; 32];
        let to = address(&device_secret, default_relays());
        let sender = iroh::SecretKey::generate().public();
        let large = sealed(100 * 1024);
        let mut events = seal_letter(
            &to,
            Letter {
                sender,
                sealed: large.clone(),
            },
        )
        .unwrap();
        assert_eq!(events.len(), 5);
        let signers: std::collections::HashSet<_> = events.iter().map(|event| event.pubkey).collect();
        assert_eq!(signers.len(), events.len(), "every part has its own signing key");
        events.rotate_left(2);
        let inbox = inbox_keys(&device_secret);
        let mut assembler = Assembler::default();
        let mut opened = Vec::new();
        for event in &events {
            if let Some(delivery) = open_letter(&inbox, event, &mut assembler).unwrap() {
                opened.push(delivery);
            }
        }
        let [delivery] = opened.as_slice() else {
            panic!("exactly one letter after the last part, got {}", opened.len());
        };
        assert_eq!((delivery.from, &delivery.sealed), (sender, &large));
    }

    #[test]
    fn every_event_fits_a_strict_relay() {
        const STRICT_RELAY_EVENT_BYTES: usize = 64 * 1024;
        let to = address(&[2; 32], default_relays());
        let sender = iroh::SecretKey::generate().public();
        for len in [MAX_LETTER_BYTES - 100, 200 * 1024] {
            let letter = Letter {
                sender,
                sealed: sealed(len),
            };
            for event in seal_letter(&to, letter).unwrap() {
                let size = event.as_json().len();
                assert!(size <= STRICT_RELAY_EVENT_BYTES, "{len}: event of {size} bytes");
            }
        }
    }

    #[test]
    fn letters_over_the_mailbox_limit_are_refused() {
        let to = address(&[1; 32], default_relays());
        let sender = iroh::SecretKey::generate().public();
        let letter = Letter {
            sender,
            sealed: sealed(MAX_MAILBOX_BYTES + 1),
        };
        assert!(seal_letter(&to, letter).is_err());
    }

    #[test]
    fn group_letters_open_with_the_tag_only() {
        let tag = group_tag_keys(&[3; 32]);
        let events = seal_payload(tag.public_key(), b"unit").unwrap();
        let [event] = events.as_slice() else {
            panic!("one event");
        };
        assert!(event.tags.public_keys().any(|key| key == tag.public_key()));
        let opened = open_payload(&tag, event, &mut Assembler::default()).unwrap();
        assert_eq!(opened.map(|(unit, _)| unit), Some(b"unit".to_vec()));
        assert!(open_payload(&group_tag_keys(&[4; 32]), event, &mut Assembler::default()).is_err());
    }

    #[test]
    fn address_validation() {
        let mut address = address(&[1; 32], vec!["https://not-a-websocket".into()]);
        assert!(!address.is_valid());
        address.relays = vec![];
        assert!(!address.is_valid());
    }
}
