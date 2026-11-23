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

use anyhow::{Context, Result, bail, ensure};
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
const LETTER_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// Relays commonly cap events around 64–128 KiB; base64 + NIP-44 inflate roughly 1.8×.
const MAX_LETTER_SEALED_BYTES: usize = 32 * 1024;
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
        let mut filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(inbox.public_key());
        if let Some(since) = since_secs {
            filter = filter.since(Timestamp::from(since.saturating_sub(SINCE_MARGIN_SECS)));
        }
        client
            .subscribe(filter)
            .await
            .context("subscribe to inbox")?;

        let group_tags = GroupTags::default();
        let tags = group_tags.clone();
        let log_tags = GroupTags::default();
        let chunk_tags = log_tags.clone();
        n0_future::task::spawn(async move {
            use nostr_sdk::prelude::StreamExt;
            while let Some(notification) = notifications.next().await {
                let ClientNotification::Event { event, .. } = notification else {
                    continue;
                };
                let tag = group_tag_of(&chunk_tags, &event)
                    .map(|tag| (tag, true))
                    .or_else(|| group_tag_of(&tags, &event).map(|tag| (tag, false)));
                if let Some((tag, log_chunk)) = tag {
                    match open_group_letter(&tag, &event, log_chunk) {
                        Ok(letter) => {
                            if group_letters.send(letter).await.is_err() {
                                return;
                            }
                        }
                        Err(err) => tracing::debug!(?err, "ignoring unreadable group letter"),
                    }
                    continue;
                }
                match open_letter(&inbox, &event) {
                    Ok(delivery) => {
                        if deliveries.send(delivery).await.is_err() {
                            return;
                        }
                    }
                    Err(err) => tracing::debug!(?err, "ignoring unreadable letter"),
                }
            }
        });
        Ok(Arc::new(Self {
            client,
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
        ensure!(
            unit.len() <= MAX_LETTER_SEALED_BYTES,
            "too large for a group letter"
        );
        let event = seal_group_letter(tag, unit)?;
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
        let output = self.client.send_event(&event).to(targets).await?;
        if output.success.is_empty() {
            bail!("no relay accepted the group letter: {:?}", output.failed);
        }
        Ok(())
    }

    /// Posts `sealed` to the recipient's inbox; succeeds if at least one relay accepted it.
    pub async fn post(&self, to: &MailboxAddress, from: EndpointId, sealed: Sealed) -> Result<()> {
        ensure!(
            sealed.ciphertext.len() <= MAX_LETTER_SEALED_BYTES,
            "too large for a mailbox letter"
        );
        let event = seal_letter(
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
        let output = self
            .client
            .send_event(&event)
            .to(to.relays.iter().map(String::as_str))
            .await?;
        if output.success.is_empty() {
            bail!("no relay accepted the letter: {:?}", output.failed);
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.client.shutdown().await;
    }
}

fn seal_letter(to: &MailboxAddress, letter: Letter) -> Result<Event> {
    let recipient = PublicKey::from_slice(&to.inbox_key)?;
    let ephemeral = Keys::generate();
    let plaintext = BASE64.encode(&wire::encode(&letter));
    let content = nip44::encrypt(
        ephemeral.secret_key(),
        &recipient,
        plaintext,
        nip44::Version::V2,
    )?;
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

fn seal_group_letter(tag: &Keys, unit: &[u8]) -> Result<Event> {
    let ephemeral = Keys::generate();
    let content = nip44::encrypt(
        ephemeral.secret_key(),
        &tag.public_key(),
        BASE64.encode(unit),
        nip44::Version::V2,
    )?;
    let expires = Timestamp::now() + LETTER_TTL;
    Ok(EventBuilder::new(Kind::GiftWrap, content)
        .tags([Tag::public_key(tag.public_key()), Tag::expiration(expires)])
        .finalize(&ephemeral)?)
}

fn open_group_letter(tag: &Keys, event: &Event, log_chunk: bool) -> Result<GroupLetter> {
    event.verify()?;
    let plaintext = nip44::decrypt(tag.secret_key(), &event.pubkey, &event.content)?;
    Ok(GroupLetter {
        unit: BASE64.decode(plaintext.as_bytes())?,
        posted_at_secs: event.created_at.as_secs(),
        log_chunk,
    })
}

fn open_letter(inbox: &Keys, event: &Event) -> Result<MailboxDelivery> {
    event.verify()?;
    let plaintext = nip44::decrypt(inbox.secret_key(), &event.pubkey, &event.content)?;
    let letter: Letter = wire::decode(&BASE64.decode(plaintext.as_bytes())?)?;
    Ok(MailboxDelivery {
        from: letter.sender,
        sealed: letter.sealed,
        posted_at_secs: event.created_at.as_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let event = seal_letter(
            &to,
            Letter {
                sender,
                sealed: sealed.clone(),
            },
        )
        .unwrap();

        assert!(!event.content.contains("olm-ciphertext"));
        assert_ne!(
            event.pubkey.to_bytes(),
            to.inbox_key,
            "signed by an ephemeral key"
        );
        let opened = open_letter(&inbox_keys(&device_secret), &event).unwrap();
        assert_eq!((opened.from, opened.sealed), (sender, sealed));
        assert!(open_letter(&inbox_keys(&[8; 32]), &event).is_err());
    }

    #[test]
    fn group_letters_open_with_the_tag_only() {
        let tag = group_tag_keys(&[3; 32]);
        let event = seal_group_letter(&tag, b"unit").unwrap();
        assert!(event.tags.public_keys().any(|key| key == tag.public_key()));
        assert_eq!(open_group_letter(&tag, &event, false).unwrap().unit, b"unit");
        assert!(open_group_letter(&group_tag_keys(&[4; 32]), &event, false).is_err());
    }

    #[test]
    fn address_validation() {
        let mut address = address(&[1; 32], vec!["https://not-a-websocket".into()]);
        assert!(!address.is_valid());
        address.relays = vec![];
        assert!(!address.is_valid());
    }
}
