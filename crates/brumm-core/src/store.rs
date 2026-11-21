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
    wire::{
        self, Body, ChatMessage, EnvelopeId, HistoryCount, HistoryItem, InviteSecret,
        MAX_FILE_NAME_CHARS, MessageId,
    },
};

const SCHEMA_VERSION: i64 = 8;
/// Oldest schema that [`SCHEMA`] upgrades in place (v5–v8 added tables; v8 also
/// [`OUTBOX_ENVELOPE_COLUMN`]).
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
    envelope      BLOB,
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
CREATE TABLE IF NOT EXISTS image_previews (
    contact  BLOB NOT NULL,
    id       BLOB NOT NULL,
    width    INTEGER NOT NULL,
    height   INTEGER NOT NULL,
    max_edge INTEGER NOT NULL,
    bytes    BLOB NOT NULL,
    PRIMARY KEY (contact, id)
);
CREATE TABLE IF NOT EXISTS seen_ciphertexts (
    hash        BLOB PRIMARY KEY,
    envelope_id BLOB NOT NULL,
    seen_at_ms  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS deleted_messages (
    contact       BLOB NOT NULL,
    id            BLOB NOT NULL,
    deleted_at_ms INTEGER NOT NULL,
    PRIMARY KEY (contact, id)
);
";
/// Plaintext envelope of an outbox entry, kept until its one re-encryption (v8).
const OUTBOX_ENVELOPE_COLUMN: &str = "envelope";

const META_SEED_PHRASE: &str = "seed_phrase";
const META_DEVICE_SECRET: &str = "device_secret";
const META_PROFILE: &str = "profile";
const META_OLM_ACCOUNT: &str = "olm_account";
const META_REVOKED: &str = "revoked";
const META_MAILBOX_SINCE: &str = "mailbox_since_secs";
const META_LAST_OPENED: &str = "last_opened";
const META_HISTORY_SYNC: &str = "history_sync";

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
/// The plaintext stays next to it only for one re-encryption ([`Store::reseal`]).
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

/// A message's identity and bucket time, without its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexedMessage {
    pub contact: UserKey,
    pub id: MessageId,
    pub sent_at_ms: u64,
}

pub struct ConversationPage {
    pub messages: Vec<MessageView>,
    pub has_older: bool,
}

/// `LIMIT -1` is SQLite's "no limit".
const SQL_NO_LIMIT: i64 = -1;
/// postcard writes the enum variant index first; `Body::File` is variant 1.
const FILE_BODY_TAG: u8 = 1;
/// Variant tag, name length varint, the longest name (4 bytes per char), data length varint.
const FILE_HEAD_BYTES: usize = 1 + MAX_VARINT_BYTES + 4 * MAX_FILE_NAME_CHARS + MAX_VARINT_BYTES;
const MAX_VARINT_BYTES: usize = 10;

/// Body column for message lists: a file is cut to its head (name and size), so listing a
/// chat never reads multi-megabyte attachments. Text bodies are read whole.
fn list_body_column() -> String {
    format!(
        "CASE WHEN substr(body, 1, 1) = x'{FILE_BODY_TAG:02x}' THEN substr(body, 1, {FILE_HEAD_BYTES}) ELSE body END"
    )
}

/// Initial history sync on a freshly linked device. `expected` comes from the linking device's
/// manifest; chunks may arrive before it, so `received` is counted regardless.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistorySyncState {
    pub expected: Option<Vec<HistoryCount>>,
    pub received: Vec<HistoryCount>,
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
        // WAL + NORMAL: no fsync per commit; a crash can lose the last commits but never
        // corrupt the database. Startup and every message insert get much cheaper.
        // The browser's OPFS VFS (sahpool) has no shared memory, so it keeps the rollback journal.
        #[cfg(not(target_family = "wasm"))]
        db.execute_batch("PRAGMA journal_mode = WAL;")?;
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
            add_column_if_missing(&db, "outbox", OUTBOX_ENVELOPE_COLUMN, "BLOB")?;
            db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self { db })
    }

    /// Log-out: deletes every row (seed phrase, keys, Olm state, chats) and rewrites the file.
    /// `secure_delete` overwrites the freed pages; `VACUUM` and the WAL checkpoint leave no
    /// old copies behind in the database or its write-ahead log. The schema stays.
    pub fn wipe(&self) -> Result<()> {
        self.db.pragma_update(None, "secure_delete", true)?;
        let tables: Vec<String> = self
            .db
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let tx = self.db.unchecked_transaction()?;
        for table in &tables {
            tx.execute(&format!("DELETE FROM \"{table}\""), [])?;
        }
        tx.commit()?;
        self.db.execute_batch("VACUUM;")?;
        #[cfg(not(target_family = "wasm"))]
        self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
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

    /// Progress of the initial history sync (only on a freshly linked device, until complete).
    pub fn history_sync(&self) -> Result<Option<HistorySyncState>> {
        get_meta(&self.db, META_HISTORY_SYNC)?
            .map(|bytes| wire::decode(&bytes).context("history sync state"))
            .transpose()
    }

    pub fn set_history_sync(&self, state: Option<&HistorySyncState>) -> Result<()> {
        match state {
            Some(state) => set_meta(&self.db, META_HISTORY_SYNC, &wire::encode(state)),
            None => {
                self.db.execute(
                    "DELETE FROM meta WHERE key = ?1",
                    params![META_HISTORY_SYNC],
                )?;
                Ok(())
            }
        }
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

    #[allow(clippy::too_many_arguments)]
    pub fn enqueue(
        &self,
        device: &EndpointId,
        envelope_id: &EnvelopeId,
        sealed: &Sealed,
        envelope: &[u8],
        chat_contact: Option<&UserKey>,
        is_receipt: bool,
        now_ms: u64,
    ) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO outbox (device_id, envelope_id, sealed, envelope, chat_contact, is_receipt, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                device.as_bytes().as_slice(),
                envelope_id.as_slice(),
                wire::encode(sealed),
                envelope,
                chat_contact.map(|contact| contact.as_slice()),
                is_receipt,
                now_ms as i64
            ],
        )?;
        Ok(())
    }

    /// The plaintext envelope and enqueue time of an entry that was not re-encrypted yet.
    pub fn outbox_envelope(
        &self,
        device: &EndpointId,
        envelope_id: &EnvelopeId,
    ) -> Result<Option<(Vec<u8>, u64)>> {
        let row: Option<(Option<Vec<u8>>, i64)> = self
            .db
            .query_row(
                "SELECT envelope, created_at_ms FROM outbox WHERE device_id = ?1 AND envelope_id = ?2",
                params![device.as_bytes().as_slice(), envelope_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(envelope, created_at_ms)| Some((envelope?, created_at_ms as u64))))
    }

    /// Replaces an entry's ciphertext with a fresh one and drops its plaintext, so an entry is
    /// re-encrypted at most once. Queue position stays; the new bytes may go to the mailbox again.
    pub fn reseal(
        &self,
        device: &EndpointId,
        envelope_id: &EnvelopeId,
        sealed: &Sealed,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE outbox SET sealed = ?3, envelope = NULL, mailboxed = 0
             WHERE device_id = ?1 AND envelope_id = ?2",
            params![
                device.as_bytes().as_slice(),
                envelope_id.as_slice(),
                wire::encode(sealed)
            ],
        )?;
        Ok(())
    }

    /// Whether anything is still queued for `device`.
    pub fn has_outbox_for(&self, device: &EndpointId) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS (SELECT 1 FROM outbox WHERE device_id = ?1)",
            params![device.as_bytes().as_slice()],
            |row| row.get(0),
        )?)
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

    /// Queue order (oldest first) without the ciphertexts; the sender loads one at a time.
    pub fn outbox_keys(&self) -> Result<Vec<(EndpointId, EnvelopeId)>> {
        let mut stmt = self
            .db
            .prepare("SELECT device_id, envelope_id FROM outbox ORDER BY created_at_ms, rowid")?;
        let rows = stmt.query_map([], |row| {
            Ok((blob_array::<32>(row, 0)?, blob_array::<16>(row, 1)?))
        })?;
        let mut keys = Vec::new();
        for row in rows {
            let (device, envelope_id) = row?;
            keys.push((EndpointId::from_bytes(&device)?, envelope_id));
        }
        Ok(keys)
    }

    #[cfg(test)]
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

    /// Returns `false` if the message was already stored (duplicate delivery or sync) or was
    /// deleted here before (a late copy must not bring it back).
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
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
             WHERE NOT EXISTS (SELECT 1 FROM deleted_messages WHERE contact = ?1 AND id = ?2)",
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

    #[cfg(test)]
    pub fn conversation(&self, contact: &UserKey) -> Result<Vec<MessageView>> {
        Ok(self.conversation_page(contact, None, None)?.messages)
    }

    /// The newest `limit` messages of a conversation (all if `None`), oldest first, or those
    /// older than `before`. File bodies are read only up to their name and size.
    pub fn conversation_page(
        &self,
        contact: &UserKey,
        before: Option<&MessageId>,
        limit: Option<u32>,
    ) -> Result<ConversationPage> {
        let sql = format!(
            "SELECT contact, id, direction, {}, sent_at_ms, status FROM messages
             WHERE contact = ?1 AND (?2 IS NULL OR (sent_at_ms, rowid) <
                 (SELECT sent_at_ms, rowid FROM messages WHERE contact = ?1 AND id = ?2))
             ORDER BY sent_at_ms DESC, rowid DESC LIMIT ?3",
            list_body_column()
        );
        // One extra row tells whether anything older is left.
        let fetch = limit.map_or(SQL_NO_LIMIT, |limit| i64::from(limit) + 1);
        let mut stmt = self.db.prepare_cached(&sql)?;
        let rows = stmt.query_map(
            params![contact.as_slice(), before.map(|id| id.as_slice()), fetch],
            message_from_row,
        )?;
        let mut messages: Vec<MessageView> = rows.collect::<Result<_, _>>()?;
        let has_older = limit.is_some_and(|limit| messages.len() > limit as usize);
        if has_older {
            messages.pop();
        }
        messages.reverse();
        let mut reactions = self.conversation_reactions(contact)?;
        for message in &mut messages {
            message.reactions = reactions.remove(&message.id).unwrap_or_default();
        }
        Ok(ConversationPage {
            messages,
            has_older,
        })
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
    /// Messages from `since_ms` on, plus every message of `keep_all` (the Notes chat, which is
    /// exempt from retention just like in [`Self::prune_messages_before`]).
    pub fn history_since(&self, since_ms: u64, keep_all: &UserKey) -> Result<Vec<HistoryItem>> {
        let mut stmt = self.db.prepare(
            "SELECT contact, id, direction, body, sent_at_ms FROM messages WHERE sent_at_ms >= ?1 OR contact = ?2 ORDER BY sent_at_ms, rowid",
        )?;
        let rows = stmt.query_map(params![since_ms as i64, keep_all], |row| {
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

    /// `(contact, id, sent_at_ms)` of every message from `since_ms` on, plus all of `keep_all`
    /// (Notes). The ids-only input of the own-device consistency check ([`crate::reconcile`]).
    pub fn message_index(&self, since_ms: u64, keep_all: &UserKey) -> Result<Vec<IndexedMessage>> {
        let mut stmt = self.db.prepare_cached(
            "SELECT contact, id, sent_at_ms FROM messages WHERE sent_at_ms >= ?1 OR contact = ?2",
        )?;
        let rows = stmt.query_map(params![since_ms as i64, keep_all], |row| {
            Ok(IndexedMessage {
                contact: blob_array(row, 0)?,
                id: blob_array(row, 1)?,
                sent_at_ms: row.get::<_, i64>(2)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Stored messages of one conversation among `ids`, as history items (with their bodies).
    pub fn history_items(&self, contact: &UserKey, ids: &[MessageId]) -> Result<Vec<HistoryItem>> {
        let mut stmt = self.db.prepare_cached(
            "SELECT direction, body, sent_at_ms FROM messages WHERE contact = ?1 AND id = ?2",
        )?;
        let mut items = Vec::new();
        for id in ids {
            let item = stmt
                .query_row(params![contact.as_slice(), id.as_slice()], |row| {
                    Ok(HistoryItem {
                        contact: *contact,
                        outgoing: direction_from_code(row.get(0)?) == Direction::Outgoing,
                        message: ChatMessage {
                            id: *id,
                            body: body_from_blob(row, 1)?,
                            sent_at_ms: row.get::<_, i64>(2)? as u64,
                        },
                    })
                })
                .optional()?;
            items.extend(item);
        }
        Ok(items)
    }

    /// The ids among `ids` that were deleted in this conversation.
    pub fn deleted_among(&self, contact: &UserKey, ids: &[MessageId]) -> Result<Vec<MessageId>> {
        let mut stmt = self.db.prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM deleted_messages WHERE contact = ?1 AND id = ?2)",
        )?;
        let mut deleted = Vec::new();
        for id in ids {
            if stmt.query_row(params![contact.as_slice(), id.as_slice()], |row| {
                row.get::<_, bool>(0)
            })? {
                deleted.push(*id);
            }
        }
        Ok(deleted)
    }

    /// Removes messages of one conversation; `only` restricts it to one direction (a contact may
    /// only delete what they sent). Returns the ids that were actually removed.
    /// Every removed id, and every id not stored at all (its copy may still be on the way), is
    /// remembered as deleted so it is never inserted again. A kept message (other direction) is not.
    pub fn delete_messages(
        &self,
        contact: &UserKey,
        ids: &[MessageId],
        only: Option<Direction>,
    ) -> Result<Vec<MessageId>> {
        let deleted_at_ms = crate::clock::now_ms();
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
            if deleted == 1 || !self.message_exists(contact, id)? {
                self.db.execute(
                    "INSERT OR IGNORE INTO deleted_messages (contact, id, deleted_at_ms) VALUES (?1, ?2, ?3)",
                    params![contact.as_slice(), id.as_slice(), deleted_at_ms as i64],
                )?;
            }
        }
        self.purge_orphan_reactions()?;
        self.purge_orphan_previews()?;
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
        // Notes keep their messages forever, so they keep their deletions too.
        self.db.execute(
            "DELETE FROM deleted_messages WHERE deleted_at_ms < ?1 AND contact != ?2",
            params![cutoff_ms as i64, keep],
        )?;
        self.purge_orphan_reactions()?;
        self.purge_orphan_previews()?;
        Ok(removed)
    }

    /// Bubble-sized preview for a photo, when one was stored for at least `min_edge` physical
    /// pixels (or the source itself is no larger than the stored edge).
    pub fn image_preview(
        &self,
        contact: &UserKey,
        id: &MessageId,
        min_edge: u32,
    ) -> Result<Option<ImagePreview>> {
        let row: Option<(i64, i64, i64, Vec<u8>)> = self
            .db
            .query_row(
                "SELECT width, height, max_edge, bytes FROM image_previews
                 WHERE contact = ?1 AND id = ?2
                   AND (max_edge >= ?3 OR (width <= max_edge AND height <= max_edge))",
                params![contact.as_slice(), id.as_slice(), i64::from(min_edge)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((width, height, _, bytes)) = row else {
            return Ok(None);
        };
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            return Ok(None);
        };
        if width == 0 || height == 0 || bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(ImagePreview {
            width,
            height,
            bytes,
        }))
    }

    pub fn set_image_preview(
        &self,
        contact: &UserKey,
        id: &MessageId,
        width: u32,
        height: u32,
        max_edge: u32,
        bytes: &[u8],
    ) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO image_previews (contact, id, width, height, max_edge, bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                contact.as_slice(),
                id.as_slice(),
                i64::from(width),
                i64::from(height),
                i64::from(max_edge),
                bytes
            ],
        )?;
        Ok(())
    }

    fn purge_orphan_previews(&self) -> Result<()> {
        self.db.execute(
            "DELETE FROM image_previews WHERE NOT EXISTS (
                 SELECT 1 FROM messages
                 WHERE messages.contact = image_previews.contact AND messages.id = image_previews.id
             )",
            [],
        )?;
        Ok(())
    }
}

/// A stored chat-photo preview. `width` and `height` are the source pixels.
pub struct ImagePreview {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
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

/// In-place upgrade step: `CREATE TABLE IF NOT EXISTS` leaves an older table without new columns.
fn add_column_if_missing(db: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let exists: bool = db.query_row(
        &format!("SELECT EXISTS (SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1)"),
        params![column],
        |row| row.get(0),
    )?;
    if !exists {
        db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
    }
    Ok(())
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
    let blob: &[u8] = row.get_ref(3)?.as_blob()?;
    let (text, attachment) = match file_head(blob) {
        Some((name, size)) => (String::new(), Some(AttachmentView { name, size })),
        None => match body_from_blob(row, 3)? {
            Body::Text(text) => (text, None),
            Body::File { name, data } => (
                String::new(),
                Some(AttachmentView {
                    name,
                    size: data.len(),
                }),
            ),
        },
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

/// A file message's name and size from the start of its encoded body (see [`list_body_column`]).
fn file_head(blob: &[u8]) -> Option<(String, usize)> {
    let (tag, rest) = read_varint(blob)?;
    if tag != usize::from(FILE_BODY_TAG) {
        return None;
    }
    let (name_len, rest) = read_varint(rest)?;
    let name = std::str::from_utf8(rest.get(..name_len)?).ok()?.to_owned();
    let (size, _) = read_varint(&rest[name_len..])?;
    Some((name, size))
}

/// postcard's unsigned LEB128 varint.
fn read_varint(bytes: &[u8]) -> Option<(usize, &[u8])> {
    let mut value: usize = 0;
    for (index, byte) in bytes.iter().take(MAX_VARINT_BYTES).enumerate() {
        value |= usize::from(byte & 0x7f).checked_shl(7 * index as u32)?;
        if byte & 0x80 == 0 {
            return Some((value, &bytes[index + 1..]));
        }
    }
    None
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

    #[test]
    fn file_head_reads_name_and_size_without_the_data() {
        let name = "ä photo.jpg".to_owned();
        let body = wire::encode(&Body::File {
            name: name.clone(),
            data: vec![7; 300],
        });
        assert_eq!(body[0], FILE_BODY_TAG);
        assert_eq!(file_head(&body), Some((name.clone(), 300)));
        let head = &body[..body.len().min(FILE_HEAD_BYTES)];
        let cut = &head[..head.len() - 300];
        assert_eq!(file_head(cut), Some((name, 300)));
        assert_eq!(file_head(&wire::encode(&Body::Text("hi".into()))), None);
    }

    #[test]
    fn conversation_pages_walk_back_without_gaps() {
        let store = Store::open_in_memory().unwrap();
        let contact = [3; 32];
        for index in 0..5u8 {
            let message = ChatMessage {
                id: [index; 16],
                // Two messages share a timestamp: the rowid keeps their order.
                sent_at_ms: u64::from(index.min(3)),
                body: if index == 2 {
                    Body::File {
                        name: "big.bin".into(),
                        data: vec![1; 4096],
                    }
                } else {
                    Body::Text(format!("m{index}"))
                },
            };
            store
                .insert_message(
                    &contact,
                    &message,
                    Direction::Incoming,
                    DeliveryStatus::Delivered,
                    None,
                )
                .unwrap();
        }
        let ids =
            |page: &ConversationPage| page.messages.iter().map(|m| m.id[0]).collect::<Vec<_>>();

        let newest = store.conversation_page(&contact, None, Some(2)).unwrap();
        assert_eq!(ids(&newest), vec![3, 4]);
        assert!(newest.has_older);
        let older = store
            .conversation_page(&contact, Some(&[3; 16]), Some(2))
            .unwrap();
        assert_eq!(ids(&older), vec![1, 2]);
        assert!(older.has_older);
        let file = &older.messages[1].attachment.as_ref().unwrap();
        assert_eq!((file.name.as_str(), file.size), ("big.bin", 4096));
        let rest = store
            .conversation_page(&contact, Some(&[1; 16]), None)
            .unwrap();
        assert_eq!(ids(&rest), vec![0]);
        assert!(!rest.has_older);
        assert_eq!(
            store
                .conversation_page(&contact, None, None)
                .unwrap()
                .messages
                .len(),
            5
        );
    }

    #[test]
    fn wipe_erases_every_row() {
        let store = Store::open_in_memory().unwrap();
        store.set_last_opened(&[7; 32]).unwrap();
        assert_eq!(store.last_opened().unwrap(), Some([7; 32]));
        store.wipe().unwrap();
        assert_eq!(store.last_opened().unwrap(), None);
        assert!(store.load_identity().unwrap().is_none());
        let rows: i64 = store
            .db
            .query_row("SELECT count(*) FROM meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }
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
            .enqueue(&device, &id, &sealed, b"plain", Some(&[9; 32]), false, 0)
            .unwrap();
        store
            .enqueue(&device, &id, &sealed, b"plain", Some(&[9; 32]), false, 0)
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

        assert_eq!(store.history_since(150, &[0; 32]).unwrap().len(), 1);
        assert_eq!(store.history_since(150, &contact).unwrap().len(), 2);
        assert_eq!(store.conversation(&contact).unwrap().len(), 2);
        assert_eq!(store.prune_messages_before(150, &[0; 32]).unwrap(), 1);
        assert_eq!(store.prune_messages_before(1000, &contact).unwrap(), 0);
    }

    #[test]
    fn image_preview_is_reused_until_the_message_is_gone() {
        let store = Store::open_in_memory().unwrap();
        let contact = [5; 32];
        let id = [3; 16];
        store
            .insert_message(
                &contact,
                &ChatMessage {
                    id,
                    sent_at_ms: 100,
                    body: Body::File {
                        name: "photo.jpg".into(),
                        data: vec![1, 2, 3],
                    },
                },
                Direction::Outgoing,
                DeliveryStatus::Delivered,
                None,
            )
            .unwrap();
        store
            .set_image_preview(&contact, &id, 3840, 2160, 960, &[9, 9, 9])
            .unwrap();
        let hit = store
            .image_preview(&contact, &id, 320)
            .unwrap()
            .expect("hit");
        assert_eq!((hit.width, hit.height), (3840, 2160));
        assert_eq!(hit.bytes, vec![9, 9, 9]);
        assert!(store.image_preview(&contact, &id, 1920).unwrap().is_none());

        store.delete_messages(&contact, &[id], None).unwrap();
        assert!(store.image_preview(&contact, &id, 320).unwrap().is_none());
    }

    fn text(id: u8, sent_at_ms: u64) -> ChatMessage {
        ChatMessage {
            id: [id; 16],
            sent_at_ms,
            body: Body::Text(format!("m{id}")),
        }
    }

    fn insert(
        store: &Store,
        contact: &UserKey,
        message: &ChatMessage,
        direction: Direction,
    ) -> bool {
        store
            .insert_message(contact, message, direction, DeliveryStatus::Delivered, None)
            .unwrap()
    }

    #[test]
    fn an_outbox_entry_is_resealed_at_most_once() {
        let store = Store::open_in_memory().unwrap();
        let device = SecretKey::generate().public();
        let id = [1; 16];
        let sealed = |byte| Sealed {
            message_type: 0,
            ciphertext: vec![byte],
        };
        store
            .enqueue(&device, &id, &sealed(1), b"envelope", None, false, 42)
            .unwrap();
        store.mark_mailboxed(&device, &id).unwrap();
        assert!(store.has_outbox_for(&device).unwrap());
        assert_eq!(
            store.outbox_envelope(&device, &id).unwrap(),
            Some((b"envelope".to_vec(), 42))
        );

        store.reseal(&device, &id, &sealed(2)).unwrap();
        let item = store.outbox_item(&device, &id).unwrap().unwrap();
        assert_eq!(item.sealed, sealed(2));
        assert!(
            !item.mailboxed,
            "the new ciphertext goes to the mailbox again"
        );
        assert_eq!(store.outbox_envelope(&device, &id).unwrap(), None);
        store.dequeue(&device, &id).unwrap();
        assert!(!store.has_outbox_for(&device).unwrap());
    }

    #[test]
    fn deleted_messages_never_come_back() {
        let store = Store::open_in_memory().unwrap();
        let contact = [2; 32];
        assert!(insert(&store, &contact, &text(1, 10), Direction::Outgoing));
        assert_eq!(
            store.delete_messages(&contact, &[[1; 16]], None).unwrap(),
            vec![[1; 16]]
        );
        assert!(!insert(&store, &contact, &text(1, 10), Direction::Outgoing));

        // A delete that overtakes its message: the late copy is refused.
        assert!(
            store
                .delete_messages(&contact, &[[2; 16]], None)
                .unwrap()
                .is_empty()
        );
        assert!(!insert(&store, &contact, &text(2, 20), Direction::Incoming));
        assert_eq!(
            store
                .deleted_among(&contact, &[[1; 16], [2; 16], [3; 16]])
                .unwrap(),
            vec![[1; 16], [2; 16]]
        );
        // The same id in another chat is unaffected.
        assert!(insert(&store, &[3; 32], &text(1, 10), Direction::Outgoing));
    }

    #[test]
    fn a_contact_delete_cannot_mark_our_own_messages() {
        let store = Store::open_in_memory().unwrap();
        let contact = [2; 32];
        assert!(insert(&store, &contact, &text(1, 10), Direction::Outgoing));
        let removed = store
            .delete_messages(&contact, &[[1; 16]], Some(Direction::Incoming))
            .unwrap();
        assert!(removed.is_empty());
        assert!(
            store
                .deleted_among(&contact, &[[1; 16]])
                .unwrap()
                .is_empty()
        );
        assert!(store.message_exists(&contact, &[1; 16]).unwrap());
    }

    #[test]
    fn pruning_drops_old_deletions_except_in_notes() {
        let store = Store::open_in_memory().unwrap();
        let (notes, bob) = ([1; 32], [2; 32]);
        store.delete_messages(&notes, &[[1; 16]], None).unwrap();
        store.delete_messages(&bob, &[[1; 16]], None).unwrap();
        store.prune_messages_before(u64::MAX / 2, &notes).unwrap();
        assert_eq!(store.deleted_among(&notes, &[[1; 16]]).unwrap().len(), 1);
        assert!(store.deleted_among(&bob, &[[1; 16]]).unwrap().is_empty());
    }

    #[test]
    fn message_index_covers_the_window_and_all_notes() {
        let store = Store::open_in_memory().unwrap();
        let (notes, bob) = ([1; 32], [2; 32]);
        insert(&store, &notes, &text(1, 5), Direction::Outgoing);
        insert(&store, &bob, &text(2, 5), Direction::Incoming);
        insert(&store, &bob, &text(3, 50), Direction::Outgoing);
        let mut index = store.message_index(10, &notes).unwrap();
        index.sort_by_key(|entry| entry.id);
        assert_eq!(
            index,
            vec![
                IndexedMessage {
                    contact: notes,
                    id: [1; 16],
                    sent_at_ms: 5
                },
                IndexedMessage {
                    contact: bob,
                    id: [3; 16],
                    sent_at_ms: 50
                },
            ]
        );
        let items = store.history_items(&bob, &[[3; 16], [9; 16]]).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].outgoing);
        assert_eq!(items[0].message, text(3, 50));
    }

    #[test]
    fn a_v7_database_upgrades_in_place() {
        let db = Connection::open_in_memory().unwrap();
        let v7_schema = SCHEMA
            .replace("    envelope      BLOB,\n", "")
            .replace("deleted_messages", "unused_in_v7");
        assert!(!v7_schema.contains("envelope      BLOB"));
        db.execute_batch(&v7_schema).unwrap();
        let device = SecretKey::generate().public();
        db.execute(
            "INSERT INTO outbox (device_id, envelope_id, sealed, is_receipt, created_at_ms) VALUES (?1, ?2, ?3, 0, 1)",
            params![device.as_bytes().as_slice(), [6u8; 16].as_slice(), [0u8].as_slice()],
        )
        .unwrap();
        db.pragma_update(None, "user_version", 7).unwrap();

        let store = Store::init(db).unwrap();
        // Entries from before v8 have no plaintext, so they are never re-encrypted.
        assert_eq!(store.outbox_envelope(&device, &[6; 16]).unwrap(), None);
        assert!(store.has_outbox_for(&device).unwrap());
        store.delete_messages(&[1; 32], &[[1; 16]], None).unwrap();
        assert_eq!(store.deleted_among(&[1; 32], &[[1; 16]]).unwrap().len(), 1);
    }
}
