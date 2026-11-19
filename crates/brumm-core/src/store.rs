//! Local persistence (SQLite): own identity + Olm account, contacts, the devices of every known
//! user (including our own other devices), Olm sessions, message history, pending invites and
//! the per-device delivery outbox. Olm pickles are encrypted with the device's pickle key; the
//! database itself relies on OS disk encryption for now (SPEC.md §10).

use std::path::Path;

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, SecretKey};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    api::{
        AttachmentView, ContactView, ConversationPreview, DeliveryStatus, Direction, MessageView,
        ReactionsView, Trust,
    },
    crypto::Sealed,
    identity::{DeviceEntry, DeviceList, Profile, UserKey, fingerprint},
    invite::INVITE_TTL_MS,
    wire::{self, Body, ChatMessage, EnvelopeId, HistoryItem, InviteSecret, MessageId},
};

const SCHEMA_VERSION: i64 = 6;
/// Oldest schema that [`SCHEMA`] upgrades in place (v5 and v6 only added tables).
const MIN_UPGRADABLE_SCHEMA_VERSION: i64 = 4;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS contacts (
    user_key        BLOB PRIMARY KEY,
    display_name    TEXT NOT NULL,
    trust           INTEGER NOT NULL,
    added_at_ms     INTEGER NOT NULL,
    device_list     BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS devices (
    device_id BLOB PRIMARY KEY,
    user_key  BLOB NOT NULL,
    entry     BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS devices_by_user ON devices (user_key);
CREATE TABLE IF NOT EXISTS olm_sessions (
    device_id    BLOB NOT NULL,
    session_id   TEXT NOT NULL,
    pickle       TEXT NOT NULL,
    last_used_ms INTEGER NOT NULL,
    PRIMARY KEY (device_id, session_id)
);
CREATE TABLE IF NOT EXISTS messages (
    contact        BLOB NOT NULL,
    id             BLOB NOT NULL,
    direction      INTEGER NOT NULL,
    body           BLOB NOT NULL,
    sent_at_ms     INTEGER NOT NULL,
    received_at_ms INTEGER,
    status         INTEGER NOT NULL,
    PRIMARY KEY (contact, id)
);
CREATE INDEX IF NOT EXISTS messages_by_time ON messages (contact, sent_at_ms);
CREATE TABLE IF NOT EXISTS invites (
    secret        BLOB PRIMARY KEY,
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS outbox (
    device_id     BLOB NOT NULL,
    envelope_id   BLOB NOT NULL,
    sealed        BLOB NOT NULL,
    chat_contact  BLOB,
    is_receipt    INTEGER NOT NULL,
    mailboxed     INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (device_id, envelope_id)
);
CREATE TABLE IF NOT EXISTS avatars (
    user_key BLOB PRIMARY KEY,
    image    BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS reactions (
    contact    BLOB NOT NULL,
    message_id BLOB NOT NULL,
    mine       INTEGER NOT NULL,
    emoji      TEXT NOT NULL,
    at_ms      INTEGER NOT NULL,
    PRIMARY KEY (contact, message_id, mine)
);
CREATE TABLE IF NOT EXISTS seen_ciphertexts (
    hash        BLOB PRIMARY KEY,
    envelope_id BLOB NOT NULL,
    seen_at_ms  INTEGER NOT NULL
);
";

const META_SEED_PHRASE: &str = "seed_phrase";
const META_DEVICE_SECRET: &str = "device_secret";
const META_PROFILE: &str = "profile";
const META_OLM_ACCOUNT: &str = "olm_account";
const META_REVOKED: &str = "revoked";
const META_MAILBOX_SINCE: &str = "mailbox_since_secs";
const META_LAST_OPENED: &str = "last_opened";

/// Own identity as persisted on this device.
pub struct StoredIdentity {
    pub seed_phrase: String,
    pub device_secret: SecretKey,
    /// Own display name and current device list.
    pub profile: Profile,
    /// Encrypted vodozemac account pickle.
    pub olm_account: String,
}

/// One envelope, encrypted once at enqueue time, waiting for an ack from one device.
pub struct OutboxItem {
    pub device: EndpointId,
    pub envelope_id: EnvelopeId,
    pub sealed: Sealed,
    /// Receipts are fire-and-forget once in the mailbox (receipts are never receipted).
    pub is_receipt: bool,
    /// Already posted to the device's Nostr mailbox.
    pub mailboxed: bool,
}

pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::init(db)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> Result<Self> {
        db.execute_batch("PRAGMA journal_mode = WAL;")?;
        // WAL + NORMAL: no fsync per commit; a crash can lose the last commits but never
        // corrupt the database. Startup and every message insert get much cheaper.
        db.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            bail!("database schema v{version} is newer than this brumm build");
        }
        if version != 0 && version < MIN_UPGRADABLE_SCHEMA_VERSION {
            bail!(
                "this profile was created by an incompatible pre-release build (schema v{version}); delete its data folder to start fresh"
            );
        }
        if version != SCHEMA_VERSION {
            db.execute_batch(SCHEMA)?;
            db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self { db })
    }

    // ── own identity ────────────────────────────────────────────────────────

    pub fn save_identity(&self, identity: &StoredIdentity) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        set_meta(&tx, META_SEED_PHRASE, identity.seed_phrase.as_bytes())?;
        set_meta(&tx, META_DEVICE_SECRET, &identity.device_secret.to_bytes())?;
        set_meta(&tx, META_PROFILE, &wire::encode(&identity.profile))?;
        set_meta(&tx, META_OLM_ACCOUNT, identity.olm_account.as_bytes())?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_identity(&self) -> Result<Option<StoredIdentity>> {
        let Some(phrase) = get_meta(&self.db, META_SEED_PHRASE)? else {
            return Ok(None);
        };
        let secret = get_meta(&self.db, META_DEVICE_SECRET)?.context("device secret missing")?;
        let profile = get_meta(&self.db, META_PROFILE)?.context("profile missing")?;
        let olm_account = get_meta(&self.db, META_OLM_ACCOUNT)?.context("olm account missing")?;
        let secret: [u8; 32] = secret.try_into().ok().context("device secret corrupt")?;
        Ok(Some(StoredIdentity {
            seed_phrase: String::from_utf8(phrase)?,
            device_secret: SecretKey::from_bytes(&secret),
            profile: wire::decode(&profile)?,
            olm_account: String::from_utf8(olm_account)?,
        }))
    }

    pub fn save_own_profile(&self, profile: &Profile) -> Result<()> {
        set_meta(&self.db, META_PROFILE, &wire::encode(profile))
    }

    pub fn save_olm_account(&self, pickle: &str) -> Result<()> {
        set_meta(&self.db, META_OLM_ACCOUNT, pickle.as_bytes())
    }

    pub fn set_revoked(&self) -> Result<()> {
        set_meta(&self.db, META_REVOKED, &[1])
    }

    pub fn is_revoked(&self) -> Result<bool> {
        Ok(get_meta(&self.db, META_REVOKED)?.is_some())
    }

    /// Creation time of the newest processed mailbox letter (Unix seconds).
    pub fn mailbox_since(&self) -> Result<Option<u64>> {
        Ok(get_meta(&self.db, META_MAILBOX_SINCE)?
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_le_bytes))
    }

    pub fn advance_mailbox_since(&self, posted_at_secs: u64) -> Result<()> {
        if self
            .mailbox_since()?
            .is_some_and(|since| since >= posted_at_secs)
        {
            return Ok(());
        }
        set_meta(&self.db, META_MAILBOX_SINCE, &posted_at_secs.to_le_bytes())
    }

    /// User key of the chat open on this device, if one was saved. A short or corrupt value is ignored.
    pub fn last_opened(&self) -> Result<Option<UserKey>> {
        Ok(get_meta(&self.db, META_LAST_OPENED)?.and_then(|bytes| bytes.try_into().ok()))
    }

    pub fn set_last_opened(&self, contact: &UserKey) -> Result<()> {
        set_meta(&self.db, META_LAST_OPENED, contact.as_slice())
    }

    // ── invites ─────────────────────────────────────────────────────────────

    pub fn add_invite(&self, secret: &InviteSecret, now_ms: u64) -> Result<()> {
        self.db.execute(
            "INSERT INTO invites (secret, created_at_ms) VALUES (?1, ?2)",
            params![secret.as_slice(), now_ms as i64],
        )?;
        Ok(())
    }

    /// Consumes a single-use invite. Returns `false` if unknown or expired.
    pub fn take_invite(&self, secret: &InviteSecret, now_ms: u64) -> Result<bool> {
        let created: Option<i64> = self
            .db
            .query_row(
                "DELETE FROM invites WHERE secret = ?1 RETURNING created_at_ms",
                params![secret.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(created.is_some_and(|created| now_ms.saturating_sub(created as u64) <= INVITE_TTL_MS))
    }

    // ── contacts ────────────────────────────────────────────────────────────

    /// Inserts or updates a contact and replaces its devices with `profile.devices`.
    /// Returns the contact and the device ids that are no longer part of the list.
    pub fn upsert_contact(
        &self,
        profile: &Profile,
        trust: Trust,
        added_at_ms: u64,
    ) -> Result<(ContactView, Vec<EndpointId>)> {
        let tx = self.db.unchecked_transaction()?;
        let user_key = profile.user_key();
        tx.execute(
            "INSERT INTO contacts (user_key, display_name, trust, added_at_ms, device_list) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (user_key) DO UPDATE SET display_name = excluded.display_name, device_list = excluded.device_list",
            params![user_key.as_slice(), profile.display_name, trust_code(trust), added_at_ms as i64, wire::encode(&profile.devices)],
        )?;
        let removed = replace_devices(&tx, &user_key, &profile.devices.entries)?;
        tx.commit()?;
        let contact = self
            .contact(&user_key)?
            .context("contact vanished after insert")?;
        Ok((contact, removed))
    }

    pub fn contact(&self, user_key: &UserKey) -> Result<Option<ContactView>> {
        Ok(self
            .db
            .query_row(
                "SELECT user_key, display_name, trust, added_at_ms FROM contacts WHERE user_key = ?1",
                params![user_key.as_slice()],
                contact_from_row,
            )
            .optional()?)
    }

    pub fn contacts(&self) -> Result<Vec<ContactView>> {
        let mut stmt = self.db.prepare(
            "SELECT user_key, display_name, trust, added_at_ms FROM contacts ORDER BY display_name",
        )?;
        let rows = stmt.query_map([], contact_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The contact's current signed device list.
    pub fn contact_device_list(&self, user_key: &UserKey) -> Result<Option<DeviceList>> {
        let list: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT device_list FROM contacts WHERE user_key = ?1",
                params![user_key.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        list.map(|list| Ok(wire::decode(&list)?)).transpose()
    }

    /// Display name + signed device list, e.g. to sync the contact to our other devices.
    pub fn contact_profile(&self, user_key: &UserKey) -> Result<Option<Profile>> {
        let row: Option<(String, Vec<u8>)> = self
            .db
            .query_row(
                "SELECT display_name, device_list FROM contacts WHERE user_key = ?1",
                params![user_key.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(display_name, list)| {
            Ok(Profile {
                display_name,
                devices: wire::decode(&list)?,
            })
        })
        .transpose()
    }

    /// Stores a newer device list for a contact; returns removed device ids.
    pub fn set_contact_device_list(&self, list: &DeviceList) -> Result<Vec<EndpointId>> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute(
            "UPDATE contacts SET device_list = ?2 WHERE user_key = ?1",
            params![list.user_key.as_slice(), wire::encode(list)],
        )?;
        let removed = replace_devices(&tx, &list.user_key, &list.entries)?;
        tx.commit()?;
        Ok(removed)
    }

    // ── devices (any user, including our own other devices) ─────────────────

    /// Replaces the stored devices of `user_key`; returns removed device ids.
    pub fn replace_devices(
        &self,
        user_key: &UserKey,
        entries: &[DeviceEntry],
    ) -> Result<Vec<EndpointId>> {
        let tx = self.db.unchecked_transaction()?;
        let removed = replace_devices(&tx, user_key, entries)?;
        tx.commit()?;
        Ok(removed)
    }

    pub fn device_owner(&self, device_id: &EndpointId) -> Result<Option<UserKey>> {
        let key: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT user_key FROM devices WHERE device_id = ?1",
                params![device_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(key.and_then(|key| key.try_into().ok()))
    }

    pub fn device_entry(&self, device_id: &EndpointId) -> Result<Option<DeviceEntry>> {
        let entry: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT entry FROM devices WHERE device_id = ?1",
                params![device_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        entry.map(|entry| Ok(wire::decode(&entry)?)).transpose()
    }

    pub fn devices_of(&self, user_key: &UserKey) -> Result<Vec<DeviceEntry>> {
        let mut stmt = self
            .db
            .prepare("SELECT entry FROM devices WHERE user_key = ?1")?;
        let entries =
            stmt.query_map(params![user_key.as_slice()], |row| row.get::<_, Vec<u8>>(0))?;
        let mut devices = Vec::new();
        for entry in entries {
            devices.push(wire::decode(&entry?)?);
        }
        Ok(devices)
    }

    // ── olm sessions ────────────────────────────────────────────────────────

    /// `(session_id, pickle, last_used_ms)` for every session with `device_id`.
    pub fn sessions_of(&self, device_id: &EndpointId) -> Result<Vec<(String, String, u64)>> {
        let mut stmt = self.db.prepare(
            "SELECT session_id, pickle, last_used_ms FROM olm_sessions WHERE device_id = ?1",
        )?;
        let rows = stmt.query_map(params![device_id.as_bytes().as_slice()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u64))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn save_session(
        &self,
        device_id: &EndpointId,
        session_id: &str,
        pickle: &str,
        last_used_ms: u64,
    ) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO olm_sessions (device_id, session_id, pickle, last_used_ms) VALUES (?1, ?2, ?3, ?4)",
            params![device_id.as_bytes().as_slice(), session_id, pickle, last_used_ms as i64],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, device_id: &EndpointId, session_id: &str) -> Result<()> {
        self.db.execute(
            "DELETE FROM olm_sessions WHERE device_id = ?1 AND session_id = ?2",
            params![device_id.as_bytes().as_slice(), session_id],
        )?;
        Ok(())
    }

    pub fn delete_sessions_of(&self, device_id: &EndpointId) -> Result<()> {
        self.db.execute(
            "DELETE FROM olm_sessions WHERE device_id = ?1",
            params![device_id.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    // ── outbox ──────────────────────────────────────────────────────────────

    pub fn enqueue(
        &self,
        device: &EndpointId,
        envelope_id: &EnvelopeId,
        sealed: &Sealed,
        chat_contact: Option<&UserKey>,
        is_receipt: bool,
        now_ms: u64,
    ) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO outbox (device_id, envelope_id, sealed, chat_contact, is_receipt, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                device.as_bytes().as_slice(),
                envelope_id.as_slice(),
                wire::encode(sealed),
                chat_contact.map(|contact| contact.as_slice()),
                is_receipt,
                now_ms as i64
            ],
        )?;
        Ok(())
    }

    pub fn outbox_item(
        &self,
        device: &EndpointId,
        envelope_id: &EnvelopeId,
    ) -> Result<Option<OutboxItem>> {
        let row = self
            .db
            .query_row(
                "SELECT sealed, is_receipt, mailboxed FROM outbox WHERE device_id = ?1 AND envelope_id = ?2",
                params![device.as_bytes().as_slice(), envelope_id.as_slice()],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, bool>(1)?, row.get::<_, bool>(2)?)),
            )
            .optional()?;
        row.map(|(sealed, is_receipt, mailboxed)| {
            Ok(OutboxItem {
                device: *device,
                envelope_id: *envelope_id,
                sealed: wire::decode(&sealed)?,
                is_receipt,
                mailboxed,
            })
        })
        .transpose()
    }

    pub fn mark_mailboxed(&self, device: &EndpointId, envelope_id: &EnvelopeId) -> Result<()> {
        self.db.execute(
            "UPDATE outbox SET mailboxed = 1 WHERE device_id = ?1 AND envelope_id = ?2",
            params![device.as_bytes().as_slice(), envelope_id.as_slice()],
        )?;
        Ok(())
    }

    /// Chat contact of a queued envelope (if it is a chat message).
    pub fn outbox_chat_contact(
        &self,
        device: &EndpointId,
        envelope_id: &EnvelopeId,
    ) -> Result<Option<UserKey>> {
        let contact: Option<Option<Vec<u8>>> = self
            .db
            .query_row(
                "SELECT chat_contact FROM outbox WHERE device_id = ?1 AND envelope_id = ?2",
                params![device.as_bytes().as_slice(), envelope_id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(contact
            .flatten()
            .and_then(|contact| contact.try_into().ok()))
    }

    pub fn outbox(&self) -> Result<Vec<OutboxItem>> {
        let mut stmt = self
            .db
            .prepare("SELECT device_id, envelope_id, sealed, is_receipt, mailboxed FROM outbox ORDER BY created_at_ms, rowid")?;
        let rows = stmt.query_map([], |row| {
            let device: [u8; 32] = blob_array(row, 0)?;
            let envelope_id: EnvelopeId = blob_array(row, 1)?;
            let sealed: Vec<u8> = row.get(2)?;
            Ok((
                device,
                envelope_id,
                sealed,
                row.get::<_, bool>(3)?,
                row.get::<_, bool>(4)?,
            ))
        })?;
        let mut items = Vec::new();
        for row in rows {
            let (device, envelope_id, sealed, is_receipt, mailboxed) = row?;
            items.push(OutboxItem {
                device: EndpointId::from_bytes(&device)?,
                envelope_id,
                sealed: wire::decode(&sealed)?,
                is_receipt,
                mailboxed,
            });
        }
        Ok(items)
    }

    /// Removes an acked envelope. `None` if it was not queued; otherwise its chat contact.
    pub fn dequeue(
        &self,
        device: &EndpointId,
        envelope_id: &EnvelopeId,
    ) -> Result<Option<Option<UserKey>>> {
        let removed: Option<Option<Vec<u8>>> = self
            .db
            .query_row(
                "DELETE FROM outbox WHERE device_id = ?1 AND envelope_id = ?2 RETURNING chat_contact",
                params![device.as_bytes().as_slice(), envelope_id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(removed.map(|contact| contact.and_then(|contact| contact.try_into().ok())))
    }

    pub fn drop_outbox_for(&self, device: &EndpointId) -> Result<()> {
        self.db.execute(
            "DELETE FROM outbox WHERE device_id = ?1",
            params![device.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    // ── inbound dedupe ──────────────────────────────────────────────────────

    /// Envelope id of an already processed ciphertext (a retry whose ack got lost).
    pub fn seen_ciphertext(&self, hash: &[u8; 32]) -> Result<Option<EnvelopeId>> {
        let id: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT envelope_id FROM seen_ciphertexts WHERE hash = ?1",
                params![hash.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id.and_then(|id| id.try_into().ok()))
    }

    pub fn remember_ciphertext(
        &self,
        hash: &[u8; 32],
        envelope_id: &EnvelopeId,
        now_ms: u64,
    ) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO seen_ciphertexts (hash, envelope_id, seen_at_ms) VALUES (?1, ?2, ?3)",
            params![hash.as_slice(), envelope_id.as_slice(), now_ms as i64],
        )?;
        Ok(())
    }

    // ── messages ────────────────────────────────────────────────────────────

    /// Returns `false` if the message was already stored (duplicate delivery or sync).
    pub fn insert_message(
        &self,
        contact: &UserKey,
        message: &ChatMessage,
        direction: Direction,
        status: DeliveryStatus,
        received_at_ms: Option<u64>,
    ) -> Result<bool> {
        let inserted = self.db.execute(
            "INSERT OR IGNORE INTO messages (contact, id, direction, body, sent_at_ms, received_at_ms, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                contact.as_slice(),
                message.id.as_slice(),
                direction_code(direction),
                wire::encode(&message.body),
                message.sent_at_ms as i64,
                received_at_ms.map(|at| at as i64),
                status_code(status)
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn set_status(
        &self,
        contact: &UserKey,
        id: &MessageId,
        status: DeliveryStatus,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE messages SET status = ?3 WHERE contact = ?1 AND id = ?2",
            params![contact.as_slice(), id.as_slice(), status_code(status)],
        )?;
        Ok(())
    }

    pub fn message_status(
        &self,
        contact: &UserKey,
        id: &MessageId,
    ) -> Result<Option<DeliveryStatus>> {
        let status: Option<i64> = self
            .db
            .query_row(
                "SELECT status FROM messages WHERE contact = ?1 AND id = ?2",
                params![contact.as_slice(), id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(status.map(status_from_code))
    }

    pub fn conversation(&self, contact: &UserKey) -> Result<Vec<MessageView>> {
        let mut stmt = self.db.prepare(
            "SELECT contact, id, direction, body, sent_at_ms, status FROM messages
             WHERE contact = ?1 ORDER BY sent_at_ms, rowid",
        )?;
        let rows = stmt.query_map(params![contact.as_slice()], message_from_row)?;
        let mut messages: Vec<MessageView> = rows.collect::<Result<_, _>>()?;
        let mut reactions = self.conversation_reactions(contact)?;
        for message in &mut messages {
            message.reactions = reactions.remove(&message.id).unwrap_or_default();
        }
        Ok(messages)
    }

    /// Latest message per conversation, for the chat list. Avoids loading full histories at
    /// startup; the sidebar only needs previews and the last activity time.
    pub fn conversation_previews(&self) -> Result<Vec<ConversationPreview>> {
        let mut stmt = self.db.prepare(
            "SELECT contact, id, direction, body, sent_at_ms, status FROM messages
             WHERE rowid IN (
                 SELECT rowid FROM (
                     SELECT rowid, ROW_NUMBER() OVER (
                         PARTITION BY contact ORDER BY sent_at_ms DESC, rowid DESC
                     ) AS newest FROM messages
                 ) WHERE newest = 1
             )",
        )?;
        let rows = stmt.query_map([], message_from_row)?;
        Ok(rows
            .map(|row| {
                row.map(|last| ConversationPreview {
                    contact: last.contact,
                    last: Some(last),
                })
            })
            .collect::<Result<_, _>>()?)
    }

    /// The newest message of one conversation, if it has any.
    pub fn last_message(&self, contact: &UserKey) -> Result<Option<MessageView>> {
        Ok(self
            .db
            .query_row(
                "SELECT contact, id, direction, body, sent_at_ms, status FROM messages
                 WHERE contact = ?1 ORDER BY sent_at_ms DESC, rowid DESC LIMIT 1",
                params![contact.as_slice()],
                message_from_row,
            )
            .optional()?)
    }

    fn conversation_reactions(
        &self,
        contact: &UserKey,
    ) -> Result<std::collections::HashMap<MessageId, ReactionsView>> {
        let mut stmt = self.db.prepare(
            "SELECT message_id, mine, emoji FROM reactions WHERE contact = ?1 AND emoji != ''",
        )?;
        let rows = stmt.query_map(params![contact.as_slice()], |row| {
            Ok((
                blob_array::<16>(row, 0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut by_message = std::collections::HashMap::<MessageId, ReactionsView>::new();
        for row in rows {
            let (id, mine, emoji) = row?;
            let entry = by_message.entry(id).or_default();
            if mine == 1 {
                entry.mine = Some(emoji)
            } else {
                entry.theirs = Some(emoji)
            }
        }
        Ok(by_message)
    }

    pub fn message_exists(&self, contact: &UserKey, id: &MessageId) -> Result<bool> {
        Ok(self.message_status(contact, id)?.is_some())
    }

    /// Stores a reaction unless a newer one for the same user and message is already there.
    /// Returns the message's resulting reactions if it changed anything.
    pub fn set_reaction(
        &self,
        contact: &UserKey,
        reaction: &wire::Reaction,
        mine: bool,
    ) -> Result<Option<ReactionsView>> {
        let changed = self.db.execute(
            "INSERT INTO reactions (contact, message_id, mine, emoji, at_ms) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (contact, message_id, mine) DO UPDATE SET emoji = excluded.emoji, at_ms = excluded.at_ms
             WHERE excluded.at_ms > reactions.at_ms",
            params![contact.as_slice(), reaction.message_id.as_slice(), mine as i64, reaction.emoji, reaction.at_ms as i64],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some(
            self.conversation_reactions(contact)?
                .remove(&reaction.message_id)
                .unwrap_or_default(),
        ))
    }

    fn purge_orphan_reactions(&self) -> Result<()> {
        self.db.execute(
            "DELETE FROM reactions WHERE NOT EXISTS
             (SELECT 1 FROM messages WHERE messages.contact = reactions.contact AND messages.id = reactions.message_id)",
            [],
        )?;
        Ok(())
    }

    /// All messages sent at or after `since_ms`, oldest first (for syncing a linked device).
    pub fn history_since(&self, since_ms: u64) -> Result<Vec<HistoryItem>> {
        let mut stmt = self.db.prepare(
            "SELECT contact, id, direction, body, sent_at_ms FROM messages WHERE sent_at_ms >= ?1 ORDER BY sent_at_ms, rowid",
        )?;
        let rows = stmt.query_map(params![since_ms as i64], |row| {
            Ok(HistoryItem {
                contact: blob_array(row, 0)?,
                outgoing: direction_from_code(row.get(2)?) == Direction::Outgoing,
                message: ChatMessage {
                    id: blob_array(row, 1)?,
                    body: body_from_blob(row, 3)?,
                    sent_at_ms: row.get::<_, i64>(4)? as u64,
                },
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Removes messages of one conversation; `only` restricts it to one direction (a contact may
    /// only delete what they sent). Returns the ids that were actually removed.
    pub fn delete_messages(
        &self,
        contact: &UserKey,
        ids: &[MessageId],
        only: Option<Direction>,
    ) -> Result<Vec<MessageId>> {
        let mut removed = Vec::new();
        for id in ids {
            let deleted = match only {
                Some(direction) => self.db.execute(
                    "DELETE FROM messages WHERE contact = ?1 AND id = ?2 AND direction = ?3",
                    params![contact.as_slice(), id.as_slice(), direction_code(direction)],
                )?,
                None => self.db.execute(
                    "DELETE FROM messages WHERE contact = ?1 AND id = ?2",
                    params![contact.as_slice(), id.as_slice()],
                )?,
            };
            if deleted == 1 {
                removed.push(*id);
            }
        }
        self.purge_orphan_reactions()?;
        Ok(removed)
    }

    /// True iff every id is a stored outgoing message of this conversation.
    pub fn all_outgoing(&self, contact: &UserKey, ids: &[MessageId]) -> Result<bool> {
        for id in ids {
            let direction: Option<i64> = self
                .db
                .query_row(
                    "SELECT direction FROM messages WHERE contact = ?1 AND id = ?2",
                    params![contact.as_slice(), id.as_slice()],
                    |row| row.get(0),
                )
                .optional()?;
            if direction.map(direction_from_code) != Some(Direction::Outgoing) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Drops queued envelopes whose id equals a deleted message id (unsent copies).
    pub fn drop_outbox_envelopes(&self, ids: &[MessageId]) -> Result<()> {
        for id in ids {
            self.db.execute(
                "DELETE FROM outbox WHERE envelope_id = ?1",
                params![id.as_slice()],
            )?;
        }
        Ok(())
    }

    pub fn message_body(&self, contact: &UserKey, id: &MessageId) -> Result<Option<Body>> {
        let blob: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT body FROM messages WHERE contact = ?1 AND id = ?2",
                params![contact.as_slice(), id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(blob.map(|blob| wire::decode(&blob)).transpose()?)
    }

    // ── avatars ─────────────────────────────────────────────────────────────

    /// Empty `image` removes the avatar.
    pub fn set_avatar(&self, user: &UserKey, image: &[u8]) -> Result<()> {
        if image.is_empty() {
            self.db.execute(
                "DELETE FROM avatars WHERE user_key = ?1",
                params![user.as_slice()],
            )?;
            return Ok(());
        }
        self.db.execute(
            "INSERT OR REPLACE INTO avatars (user_key, image) VALUES (?1, ?2)",
            params![user.as_slice(), image],
        )?;
        Ok(())
    }

    pub fn avatars(&self) -> Result<Vec<(UserKey, Vec<u8>)>> {
        let mut stmt = self.db.prepare("SELECT user_key, image FROM avatars")?;
        let rows = stmt.query_map([], |row| Ok((blob_array(row, 0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Deletes messages (and dedupe records) older than `cutoff_ms`, except the `keep` conversation
    /// (the Notes chat). Returns messages removed.
    pub fn prune_messages_before(&self, cutoff_ms: u64, keep: &UserKey) -> Result<usize> {
        self.db.execute(
            "DELETE FROM seen_ciphertexts WHERE seen_at_ms < ?1",
            params![cutoff_ms as i64],
        )?;
        let removed = self.db.execute(
            "DELETE FROM messages WHERE sent_at_ms < ?1 AND contact != ?2",
            params![cutoff_ms as i64, keep],
        )?;
        self.purge_orphan_reactions()?;
        Ok(removed)
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn replace_devices(
    db: &Connection,
    user_key: &UserKey,
    entries: &[DeviceEntry],
) -> Result<Vec<EndpointId>> {
    let mut stmt = db.prepare("SELECT device_id FROM devices WHERE user_key = ?1")?;
    let existing = stmt
        .query_map(params![user_key.as_slice()], |row| blob_array::<32>(row, 0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut removed = Vec::new();
    for device in existing {
        let device = EndpointId::from_bytes(&device)?;
        if !entries.iter().any(|entry| entry.device_id() == device) {
            db.execute(
                "DELETE FROM devices WHERE device_id = ?1",
                params![device.as_bytes().as_slice()],
            )?;
            removed.push(device);
        }
    }
    for entry in entries {
        db.execute(
            "INSERT OR REPLACE INTO devices (device_id, user_key, entry) VALUES (?1, ?2, ?3)",
            params![
                entry.device_id().as_bytes().as_slice(),
                user_key.as_slice(),
                wire::encode(entry)
            ],
        )?;
    }
    Ok(removed)
}

fn set_meta(db: &Connection, key: &str, value: &[u8]) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}

fn get_meta(db: &Connection, key: &str) -> Result<Option<Vec<u8>>> {
    Ok(db
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?)
}

fn contact_from_row(row: &rusqlite::Row) -> rusqlite::Result<ContactView> {
    let user_key: UserKey = blob_array(row, 0)?;
    Ok(ContactView {
        fingerprint: fingerprint(&user_key),
        user_key,
        display_name: row.get(1)?,
        trust: trust_from_code(row.get(2)?),
        added_at_ms: row.get::<_, i64>(3)? as u64,
    })
}

fn message_from_row(row: &rusqlite::Row) -> rusqlite::Result<MessageView> {
    let (text, attachment) = match body_from_blob(row, 3)? {
        Body::Text(text) => (text, None),
        Body::File { name, data } => (
            String::new(),
            Some(AttachmentView {
                name,
                size: data.len(),
            }),
        ),
    };
    Ok(MessageView {
        contact: blob_array(row, 0)?,
        id: blob_array(row, 1)?,
        direction: direction_from_code(row.get(2)?),
        text,
        attachment,
        reactions: ReactionsView::default(),
        sent_at_ms: row.get::<_, i64>(4)? as u64,
        status: status_from_code(row.get(5)?),
    })
}

fn blob_array<const N: usize>(row: &rusqlite::Row, index: usize) -> rusqlite::Result<[u8; N]> {
    let blob: Vec<u8> = row.get(index)?;
    blob.try_into().map_err(|blob: Vec<u8>| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            format!("expected {N} bytes, got {}", blob.len()).into(),
        )
    })
}

fn body_from_blob(row: &rusqlite::Row, index: usize) -> rusqlite::Result<Body> {
    let blob: Vec<u8> = row.get(index)?;
    wire::decode(&blob).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Blob, Box::new(err))
    })
}

fn trust_code(trust: Trust) -> i64 {
    match trust {
        Trust::Verified => 0,
    }
}

fn trust_from_code(_code: i64) -> Trust {
    Trust::Verified
}

fn direction_code(direction: Direction) -> i64 {
    match direction {
        Direction::Incoming => 0,
        Direction::Outgoing => 1,
    }
}

fn direction_from_code(code: i64) -> Direction {
    if code == 0 {
        Direction::Incoming
    } else {
        Direction::Outgoing
    }
}

fn status_code(status: DeliveryStatus) -> i64 {
    match status {
        DeliveryStatus::Pending => 0,
        DeliveryStatus::Delivered => 1,
        DeliveryStatus::Failed => 2,
        DeliveryStatus::Sent => 3,
    }
}

fn status_from_code(code: i64) -> DeliveryStatus {
    match code {
        0 => DeliveryStatus::Pending,
        1 => DeliveryStatus::Delivered,
        3 => DeliveryStatus::Sent,
        _ => DeliveryStatus::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{UserIdentity, tests::entry};

    fn profile(identity: &UserIdentity, devices: &[EndpointId]) -> Profile {
        let entries = devices
            .iter()
            .map(|device| entry(identity, *device))
            .collect();
        Profile {
            display_name: "bob".into(),
            devices: identity.sign_device_list(1, entries),
        }
    }

    #[test]
    fn invite_is_single_use_and_expires() {
        let store = Store::open_in_memory().unwrap();
        store.add_invite(&[1; 16], 0).unwrap();
        assert!(store.take_invite(&[1; 16], 10).unwrap());
        assert!(!store.take_invite(&[1; 16], 10).unwrap());

        store.add_invite(&[2; 16], 0).unwrap();
        assert!(!store.take_invite(&[2; 16], INVITE_TTL_MS + 1).unwrap());
    }

    #[test]
    fn contact_devices_are_replaced() {
        let store = Store::open_in_memory().unwrap();
        let bob = UserIdentity::generate();
        let (laptop, phone) = (
            SecretKey::generate().public(),
            SecretKey::generate().public(),
        );
        let (contact, removed) = store
            .upsert_contact(&profile(&bob, &[laptop, phone]), Trust::Verified, 0)
            .unwrap();
        assert!(removed.is_empty());
        assert_eq!(store.devices_of(&contact.user_key).unwrap().len(), 2);
        assert_eq!(store.device_owner(&phone).unwrap(), Some(bob.public()));

        let (_, removed) = store
            .upsert_contact(&profile(&bob, &[laptop]), Trust::Verified, 0)
            .unwrap();
        assert_eq!(removed, vec![phone]);
        assert_eq!(store.device_owner(&phone).unwrap(), None);

        let newer = bob.sign_device_list(2, vec![entry(&bob, phone)]);
        assert_eq!(store.set_contact_device_list(&newer).unwrap(), vec![laptop]);
        assert_eq!(
            store.contact_device_list(&bob.public()).unwrap(),
            Some(newer.clone())
        );
        assert_eq!(
            store
                .contact_profile(&bob.public())
                .unwrap()
                .unwrap()
                .devices,
            newer
        );
    }

    #[test]
    fn sessions_and_outbox() {
        let store = Store::open_in_memory().unwrap();
        let device = SecretKey::generate().public();
        store.save_session(&device, "a", "pickle-a", 1).unwrap();
        store.save_session(&device, "b", "pickle-b", 2).unwrap();
        assert_eq!(store.sessions_of(&device).unwrap().len(), 2);
        store.delete_session(&device, "a").unwrap();
        assert_eq!(store.sessions_of(&device).unwrap().len(), 1);

        let sealed = Sealed {
            message_type: 0,
            ciphertext: vec![1, 2, 3],
        };
        let id = [7; 16];
        store
            .enqueue(&device, &id, &sealed, Some(&[9; 32]), false, 0)
            .unwrap();
        store
            .enqueue(&device, &id, &sealed, Some(&[9; 32]), false, 0)
            .unwrap();
        let outbox = store.outbox().unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].sealed, sealed);
        assert!(!outbox[0].mailboxed);
        store.mark_mailboxed(&device, &id).unwrap();
        assert!(store.outbox_item(&device, &id).unwrap().unwrap().mailboxed);
        assert_eq!(
            store.outbox_chat_contact(&device, &id).unwrap(),
            Some([9; 32])
        );

        assert_eq!(store.mailbox_since().unwrap(), None);
        store.advance_mailbox_since(50).unwrap();
        store.advance_mailbox_since(40).unwrap();
        assert_eq!(store.mailbox_since().unwrap(), Some(50));
        assert_eq!(store.dequeue(&device, &id).unwrap(), Some(Some([9; 32])));
        assert_eq!(store.dequeue(&device, &id).unwrap(), None);

        assert_eq!(store.last_opened().unwrap(), None);
        store.set_last_opened(&[4; 32]).unwrap();
        assert_eq!(store.last_opened().unwrap(), Some([4; 32]));
        set_meta(&store.db, META_LAST_OPENED, &[1, 2, 3]).unwrap();
        assert_eq!(store.last_opened().unwrap(), None);

        store.remember_ciphertext(&[1; 32], &id, 5).unwrap();
        assert_eq!(store.seen_ciphertext(&[1; 32]).unwrap(), Some(id));
        store.prune_messages_before(6, &[0; 32]).unwrap();
        assert_eq!(store.seen_ciphertext(&[1; 32]).unwrap(), None);
    }

    #[test]
    fn messages_dedupe_history_and_prune() {
        let store = Store::open_in_memory().unwrap();
        let contact = [5; 32];
        let message = ChatMessage {
            id: [3; 16],
            sent_at_ms: 100,
            body: Body::Text("hi".into()),
        };
        assert!(
            store
                .insert_message(
                    &contact,
                    &message,
                    Direction::Incoming,
                    DeliveryStatus::Delivered,
                    Some(101)
                )
                .unwrap()
        );
        assert!(
            !store
                .insert_message(
                    &contact,
                    &message,
                    Direction::Incoming,
                    DeliveryStatus::Delivered,
                    Some(102)
                )
                .unwrap()
        );

        let outgoing = ChatMessage {
            id: [4; 16],
            sent_at_ms: 200,
            body: Body::Text("yo".into()),
        };
        store
            .insert_message(
                &contact,
                &outgoing,
                Direction::Outgoing,
                DeliveryStatus::Pending,
                None,
            )
            .unwrap();
        store
            .set_status(&contact, &outgoing.id, DeliveryStatus::Delivered)
            .unwrap();
        assert_eq!(
            store.message_status(&contact, &outgoing.id).unwrap(),
            Some(DeliveryStatus::Delivered)
        );

        assert_eq!(store.history_since(150).unwrap().len(), 1);
        assert_eq!(store.conversation(&contact).unwrap().len(), 2);
        assert_eq!(store.prune_messages_before(150, &[0; 32]).unwrap(), 1);
        assert_eq!(store.prune_messages_before(1000, &contact).unwrap(), 0);
    }
}
