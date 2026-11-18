//! Local persistence (SQLite): own identity + Olm account, contacts and their devices, Olm
//! sessions, message history, pending invites. Olm pickles are encrypted with the device's
//! pickle key; the database itself relies on OS disk encryption for now (SPEC.md §10).

use std::path::Path;

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, SecretKey};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    api::{ContactView, DeliveryStatus, Direction, MessageView, Trust},
    identity::{DeviceCert, Profile, UserKey, fingerprint},
    invite::INVITE_TTL_MS,
    wire::{self, Body, ChatMessage, InviteSecret, MessageId},
};

const SCHEMA_VERSION: i64 = 2;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS contacts (
    user_key     BLOB PRIMARY KEY,
    display_name TEXT NOT NULL,
    trust        INTEGER NOT NULL,
    added_at_ms  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS devices (
    device_id BLOB PRIMARY KEY,
    user_key  BLOB NOT NULL REFERENCES contacts(user_key) ON DELETE CASCADE,
    cert      BLOB NOT NULL
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
CREATE TABLE IF NOT EXISTS olm_sessions (
    device_id BLOB PRIMARY KEY REFERENCES devices(device_id) ON DELETE CASCADE,
    pickle    TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS invites (
    secret        BLOB PRIMARY KEY,
    created_at_ms INTEGER NOT NULL
);
";

const META_SEED_PHRASE: &str = "seed_phrase";
const META_DEVICE_SECRET: &str = "device_secret";
const META_PROFILE: &str = "profile";
const META_OLM_ACCOUNT: &str = "olm_account";

/// Own identity as persisted on this device.
pub struct StoredIdentity {
    pub seed_phrase: String,
    pub device_secret: SecretKey,
    pub profile: Profile,
    /// Encrypted vodozemac account pickle.
    pub olm_account: String,
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
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            bail!("database schema v{version} is newer than this brumm build");
        }
        if version != 0 && version < SCHEMA_VERSION {
            bail!("this profile was created by an incompatible pre-release build (schema v{version}); delete its data folder to start fresh");
        }
        db.execute_batch(SCHEMA)?;
        db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self { db })
    }

    // ── identity ────────────────────────────────────────────────────────────

    pub fn save_identity(&self, identity: &StoredIdentity) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        set_meta(&tx, META_SEED_PHRASE, identity.seed_phrase.as_bytes())?;
        set_meta(&tx, META_DEVICE_SECRET, &identity.device_secret.to_bytes())?;
        set_meta(&tx, META_PROFILE, &wire::encode(&identity.profile))?;
        set_meta(&tx, META_OLM_ACCOUNT, identity.olm_account.as_bytes())?;
        tx.commit()?;
        Ok(())
    }

    pub fn save_olm_account(&self, pickle: &str) -> Result<()> {
        set_meta(&self.db, META_OLM_ACCOUNT, pickle.as_bytes())
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

    pub fn upsert_contact(&self, profile: &Profile, trust: Trust, now_ms: u64) -> Result<ContactView> {
        let tx = self.db.unchecked_transaction()?;
        let user_key = profile.user_key();
        tx.execute(
            "INSERT INTO contacts (user_key, display_name, trust, added_at_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (user_key) DO UPDATE SET display_name = excluded.display_name",
            params![user_key.as_slice(), profile.display_name, trust_code(trust), now_ms as i64],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO devices (device_id, user_key, cert) VALUES (?1, ?2, ?3)",
            params![
                profile.device_cert.device_id.as_bytes().as_slice(),
                user_key.as_slice(),
                wire::encode(&profile.device_cert)
            ],
        )?;
        tx.commit()?;
        self.contact(&user_key)?.context("contact vanished after insert")
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

    pub fn contact_for_device(&self, device_id: &EndpointId) -> Result<Option<UserKey>> {
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

    pub fn device_cert(&self, device_id: &EndpointId) -> Result<Option<DeviceCert>> {
        let cert: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT cert FROM devices WHERE device_id = ?1",
                params![device_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        cert.map(|cert| Ok(wire::decode(&cert)?)).transpose()
    }

    // ── olm sessions ────────────────────────────────────────────────────────

    pub fn session_pickle(&self, device_id: &EndpointId) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT pickle FROM olm_sessions WHERE device_id = ?1",
                params![device_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn save_session(&self, device_id: &EndpointId, pickle: &str) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO olm_sessions (device_id, pickle) VALUES (?1, ?2)",
            params![device_id.as_bytes().as_slice(), pickle],
        )?;
        Ok(())
    }

    pub fn devices_of(&self, user_key: &UserKey) -> Result<Vec<EndpointId>> {
        let mut stmt = self.db.prepare("SELECT cert FROM devices WHERE user_key = ?1")?;
        let certs = stmt.query_map(params![user_key.as_slice()], |row| row.get::<_, Vec<u8>>(0))?;
        let mut devices = Vec::new();
        for cert in certs {
            let cert: DeviceCert = wire::decode(&cert?)?;
            devices.push(cert.device_id);
        }
        Ok(devices)
    }

    // ── messages ────────────────────────────────────────────────────────────

    pub fn insert_outgoing(&self, contact: &UserKey, message: &ChatMessage) -> Result<()> {
        self.db.execute(
            "INSERT INTO messages (contact, id, direction, body, sent_at_ms, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                contact.as_slice(),
                message.id.as_slice(),
                direction_code(Direction::Outgoing),
                wire::encode(&message.body),
                message.sent_at_ms as i64,
                status_code(DeliveryStatus::Pending)
            ],
        )?;
        Ok(())
    }

    /// Returns `false` if the message was already stored (duplicate delivery).
    pub fn insert_incoming(&self, contact: &UserKey, message: &ChatMessage, now_ms: u64) -> Result<bool> {
        let inserted = self.db.execute(
            "INSERT OR IGNORE INTO messages (contact, id, direction, body, sent_at_ms, received_at_ms, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                contact.as_slice(),
                message.id.as_slice(),
                direction_code(Direction::Incoming),
                wire::encode(&message.body),
                message.sent_at_ms as i64,
                now_ms as i64,
                status_code(DeliveryStatus::Delivered)
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn set_status(&self, contact: &UserKey, id: &MessageId, status: DeliveryStatus) -> Result<()> {
        self.db.execute(
            "UPDATE messages SET status = ?3 WHERE contact = ?1 AND id = ?2",
            params![contact.as_slice(), id.as_slice(), status_code(status)],
        )?;
        Ok(())
    }

    pub fn conversation(&self, contact: &UserKey) -> Result<Vec<MessageView>> {
        let mut stmt = self.db.prepare(
            "SELECT contact, id, direction, body, sent_at_ms, status FROM messages
             WHERE contact = ?1 ORDER BY sent_at_ms, rowid",
        )?;
        let rows = stmt.query_map(params![contact.as_slice()], message_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn pending_outgoing(&self) -> Result<Vec<(UserKey, ChatMessage)>> {
        let mut stmt = self.db.prepare(
            "SELECT contact, id, body, sent_at_ms FROM messages WHERE direction = ?1 AND status = ?2",
        )?;
        let rows = stmt.query_map(
            params![direction_code(Direction::Outgoing), status_code(DeliveryStatus::Pending)],
            |row| {
                let contact = blob_array(row, 0)?;
                let message = ChatMessage {
                    id: blob_array(row, 1)?,
                    body: body_from_blob(row, 2)?,
                    sent_at_ms: row.get::<_, i64>(3)? as u64,
                };
                Ok((contact, message))
            },
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Deletes messages sent before `cutoff_ms`. Returns the number removed.
    pub fn prune_messages_before(&self, cutoff_ms: u64) -> Result<usize> {
        Ok(self
            .db
            .execute("DELETE FROM messages WHERE sent_at_ms < ?1", params![cutoff_ms as i64])?)
    }
}

// ── row mapping ─────────────────────────────────────────────────────────────

fn set_meta(db: &Connection, key: &str, value: &[u8]) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}

fn get_meta(db: &Connection, key: &str) -> Result<Option<Vec<u8>>> {
    Ok(db
        .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| row.get(0))
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
    let Body::Text(text) = body_from_blob(row, 3)?;
    Ok(MessageView {
        contact: blob_array(row, 0)?,
        id: blob_array(row, 1)?,
        direction: direction_from_code(row.get(2)?),
        text,
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
    if code == 0 { Direction::Incoming } else { Direction::Outgoing }
}

fn status_code(status: DeliveryStatus) -> i64 {
    match status {
        DeliveryStatus::Pending => 0,
        DeliveryStatus::Delivered => 1,
        DeliveryStatus::Failed => 2,
    }
}

fn status_from_code(code: i64) -> DeliveryStatus {
    match code {
        0 => DeliveryStatus::Pending,
        1 => DeliveryStatus::Delivered,
        _ => DeliveryStatus::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::UserIdentity;

    fn profile(name: &str) -> Profile {
        let identity = UserIdentity::generate();
        Profile {
            display_name: name.into(),
            device_cert: identity.certify_device(SecretKey::generate().public(), [3; 32], 1),
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
    fn messages_roundtrip_dedupe_and_prune() {
        let store = Store::open_in_memory().unwrap();
        let bob = profile("bob");
        let contact = store.upsert_contact(&bob, Trust::Verified, 0).unwrap();
        assert_eq!(store.contact_for_device(&bob.device_cert.device_id).unwrap(), Some(contact.user_key));

        let device = bob.device_cert.device_id;
        assert_eq!(store.device_cert(&device).unwrap(), Some(bob.device_cert.clone()));
        store.save_session(&device, "pickle").unwrap();
        assert_eq!(store.session_pickle(&device).unwrap().as_deref(), Some("pickle"));

        let message = ChatMessage { id: [3; 16], sent_at_ms: 100, body: Body::Text("hi".into()) };
        assert!(store.insert_incoming(&contact.user_key, &message, 101).unwrap());
        assert!(!store.insert_incoming(&contact.user_key, &message, 102).unwrap());

        let outgoing = ChatMessage { id: [4; 16], sent_at_ms: 200, body: Body::Text("yo".into()) };
        store.insert_outgoing(&contact.user_key, &outgoing).unwrap();
        assert_eq!(store.pending_outgoing().unwrap().len(), 1);
        store.set_status(&contact.user_key, &outgoing.id, DeliveryStatus::Delivered).unwrap();
        assert!(store.pending_outgoing().unwrap().is_empty());

        assert_eq!(store.conversation(&contact.user_key).unwrap().len(), 2);
        assert_eq!(store.prune_messages_before(150).unwrap(), 1);
    }
}
