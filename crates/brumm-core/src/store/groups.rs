//! Group persistence (SPEC.md §14): the folded state per group (a cache of its op log), the
//! ops, a member index for paging, epoch secrets and sender chains (sealed with the device's
//! pickle key by the caller), the group's ciphertext log for catch-up, blobs, invitations and
//! the group keys of known devices.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::{Store, blob_array};
use crate::{
    api::{GroupInvitationView, GroupStatus, InvitationStage, GroupView, MemberView, ReactionsView, Role},
    group::{
        DeviceKey, DeviceKeys, GroupId, GroupOp, OpHash,
        checkpoint::Checkpoint,
        keys::{EpochId, Rekey},
        oplog::GroupState,
        sync::Item,
    },
    identity::{DeviceList, UserKey},
    wire::{self, MessageId, Reaction},
};

/// Added in schema v11: the invitee accepted and waits for the welcome.
pub(super) const INVITATION_ACCEPTED_COLUMN: &str = "accepted";
pub(super) const GROUP_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS groups (
    id            BLOB PRIMARY KEY,
    status        INTEGER NOT NULL,
    view          BLOB NOT NULL,
    state         BLOB NOT NULL,
    checkpoint    BLOB,
    sending_epoch BLOB,
    joined_at_ms  INTEGER NOT NULL,
    last_rekey_ms INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS group_ops (
    group_id BLOB NOT NULL,
    hash     BLOB NOT NULL,
    op       BLOB NOT NULL,
    PRIMARY KEY (group_id, hash)
);
CREATE TABLE IF NOT EXISTS group_members (
    group_id     BLOB NOT NULL,
    user_key     BLOB NOT NULL,
    display_name TEXT NOT NULL,
    role         INTEGER NOT NULL,
    joined_seq   INTEGER NOT NULL,
    active       INTEGER NOT NULL,
    PRIMARY KEY (group_id, user_key)
);
CREATE INDEX IF NOT EXISTS group_members_by_name ON group_members (group_id, active, display_name);
CREATE TABLE IF NOT EXISTS group_epochs (
    group_id      BLOB NOT NULL,
    epoch         BLOB NOT NULL,
    sealed        BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL,
    retired_at_ms INTEGER,
    PRIMARY KEY (group_id, epoch)
);
CREATE TABLE IF NOT EXISTS group_peers (
    user_key    BLOB PRIMARY KEY,
    device_list BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS group_rekeys (
    group_id BLOB NOT NULL,
    epoch    BLOB NOT NULL,
    record   BLOB NOT NULL,
    opened   INTEGER NOT NULL,
    PRIMARY KEY (group_id, epoch)
);
CREATE TABLE IF NOT EXISTS group_chains (
    group_id BLOB NOT NULL,
    epoch    BLOB NOT NULL,
    device   BLOB NOT NULL,
    state    BLOB NOT NULL,
    PRIMARY KEY (group_id, epoch, device)
);
CREATE TABLE IF NOT EXISTS group_log (
    group_id       BLOB NOT NULL,
    id             BLOB NOT NULL,
    sent_at_ms     INTEGER NOT NULL,
    wire           BLOB NOT NULL,
    processed      INTEGER NOT NULL,
    received_at_ms INTEGER NOT NULL,
    PRIMARY KEY (group_id, id)
);
CREATE INDEX IF NOT EXISTS group_log_by_time ON group_log (group_id, sent_at_ms, id);
CREATE TABLE IF NOT EXISTS blobs (
    hash         BLOB PRIMARY KEY,
    data         BLOB NOT NULL,
    stored_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS group_invitations (
    group_id       BLOB PRIMARY KEY,
    inviter        BLOB NOT NULL,
    record         BLOB NOT NULL,
    received_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS group_pending_logs (
    group_id       BLOB PRIMARY KEY,
    record         BLOB NOT NULL,
    received_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS group_invites_sent (
    group_id   BLOB NOT NULL,
    user_key   BLOB NOT NULL,
    sent_at_ms INTEGER NOT NULL,
    PRIMARY KEY (group_id, user_key)
);
CREATE TABLE IF NOT EXISTS device_group_keys (
    device_id BLOB PRIMARY KEY,
    user_key  BLOB NOT NULL,
    record    BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS device_group_keys_by_user ON device_group_keys (user_key);
CREATE TABLE IF NOT EXISTS group_reactions (
    conversation BLOB NOT NULL,
    message_id   BLOB NOT NULL,
    user_key     BLOB NOT NULL,
    mine         INTEGER NOT NULL,
    emoji        TEXT NOT NULL,
    at_ms        INTEGER NOT NULL,
    PRIMARY KEY (conversation, message_id, user_key)
);
";

/// One known epoch (a wire-encoded [`crate::group::epochs::Epoch`], sealed at rest by the caller).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochRow {
    pub epoch: EpochId,
    pub sealed: Vec<u8>,
    pub created_at_ms: u64,
    /// When a newer epoch replaced it for sending; deleted [`crate::group::keys::EPOCH_GRACE_MS`] later.
    pub retired_at_ms: Option<u64>,
}

/// A group as stored: its folded state and how we relate to it.
pub struct StoredGroup {
    pub state: GroupState,
    pub checkpoint: Option<Checkpoint>,
    pub status: GroupStatus,
    pub sending_epoch: Option<EpochId>,
    pub last_rekey_ms: u64,
    /// When this device first stored the group (joined it, or got it from an own device).
    pub joined_at_ms: u64,
}

fn status_code(status: GroupStatus) -> i64 {
    match status {
        GroupStatus::Active => 0,
        GroupStatus::Left => 1,
        GroupStatus::Removed => 2,
    }
}

fn status_from_code(code: i64) -> GroupStatus {
    match code {
        0 => GroupStatus::Active,
        1 => GroupStatus::Left,
        _ => GroupStatus::Removed,
    }
}

fn role_code(role: Role) -> i64 {
    match role {
        Role::Owner => 0,
        Role::Admin => 1,
        Role::Member => 2,
    }
}

fn role_from_code(code: i64) -> Role {
    match code {
        0 => Role::Owner,
        1 => Role::Admin,
        _ => Role::Member,
    }
}

fn decode_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row,
    index: usize,
) -> rusqlite::Result<T> {
    let bytes: Vec<u8> = row.get(index)?;
    wire::decode(&bytes).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Blob, Box::new(err))
    })
}

impl Store {
    // ── groups ──────────────────────────────────────────────────────────────

    /// Ids of the groups we are in (not left or removed).
    pub fn active_group_ids(&self) -> Result<Vec<GroupId>> {
        let mut stmt = self
            .db
            .prepare("SELECT id FROM groups WHERE status = 0")?;
        let rows = stmt.query_map([], |row| blob_array(row, 0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every group with its status, for the chat list. Reads the cached state, never the ops.
    pub fn group_ids(&self) -> Result<Vec<GroupId>> {
        let mut stmt = self.db.prepare("SELECT id FROM groups")?;
        let rows = stmt.query_map([], |row| blob_array(row, 0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn load_group(&self, group: &GroupId) -> Result<Option<StoredGroup>> {
        Ok(self
            .db
            .query_row(
                "SELECT state, checkpoint, status, sending_epoch, last_rekey_ms, joined_at_ms
                 FROM groups WHERE id = ?1",
                params![group.as_slice()],
                |row| {
                    let checkpoint: Option<Vec<u8>> = row.get(1)?;
                    let sending: Option<Vec<u8>> = row.get(3)?;
                    Ok(StoredGroup {
                        state: decode_column(row, 0)?,
                        checkpoint: checkpoint.and_then(|bytes| wire::decode(&bytes).ok()),
                        status: status_from_code(row.get(2)?),
                        sending_epoch: sending.and_then(|bytes| bytes.try_into().ok()),
                        last_rekey_ms: row.get::<_, i64>(4)? as u64,
                        joined_at_ms: row.get::<_, i64>(5)? as u64,
                    })
                },
            )
            .optional()?)
    }

    /// Stores the folded state and its view (the chat list reads only the view).
    pub fn save_group(&self, state: &GroupState, view: &GroupView, now_ms: u64) -> Result<()> {
        self.db.execute(
            "INSERT INTO groups (id, status, view, state, joined_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET status = excluded.status, view = excluded.view,
                 state = excluded.state",
            params![
                state.group.as_slice(),
                status_code(view.status),
                wire::encode(view),
                wire::encode(state),
                now_ms as i64
            ],
        )?;
        Ok(())
    }

    /// Every group's view, for `Ready` (no op log, no member list).
    pub fn group_views(&self) -> Result<Vec<GroupView>> {
        let mut stmt = self.db.prepare("SELECT view FROM groups")?;
        let rows = stmt.query_map([], |row| decode_column(row, 0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_group_checkpoint(&self, group: &GroupId, checkpoint: &Checkpoint) -> Result<()> {
        self.db.execute(
            "UPDATE groups SET checkpoint = ?2 WHERE id = ?1",
            params![group.as_slice(), wire::encode(checkpoint)],
        )?;
        Ok(())
    }

    pub fn set_sending_epoch(&self, group: &GroupId, epoch: Option<&EpochId>) -> Result<()> {
        self.db.execute(
            "UPDATE groups SET sending_epoch = ?2 WHERE id = ?1",
            params![group.as_slice(), epoch.map(|epoch| epoch.as_slice())],
        )?;
        Ok(())
    }

    pub fn set_last_rekey(&self, group: &GroupId, at_ms: u64) -> Result<()> {
        self.db.execute(
            "UPDATE groups SET last_rekey_ms = ?2 WHERE id = ?1",
            params![group.as_slice(), at_ms as i64],
        )?;
        Ok(())
    }

    // ── ops ─────────────────────────────────────────────────────────────────

    /// Returns `false` if the op was already stored.
    pub fn insert_group_op(&self, op: &GroupOp) -> Result<bool> {
        Ok(self.db.execute(
            "INSERT OR IGNORE INTO group_ops (group_id, hash, op) VALUES (?1, ?2, ?3)",
            params![op.group.as_slice(), op.hash().as_slice(), wire::encode(op)],
        )? == 1)
    }

    pub fn group_ops(&self, group: &GroupId) -> Result<Vec<GroupOp>> {
        let mut stmt = self
            .db
            .prepare_cached("SELECT op FROM group_ops WHERE group_id = ?1")?;
        let rows = stmt.query_map(params![group.as_slice()], |row| decode_column(row, 0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Drops ops a verified checkpoint covers.
    pub fn prune_group_ops(&self, group: &GroupId, covered: &[OpHash]) -> Result<usize> {
        let mut removed = 0;
        for hash in covered {
            removed += self.db.execute(
                "DELETE FROM group_ops WHERE group_id = ?1 AND hash = ?2",
                params![group.as_slice(), hash.as_slice()],
            )?;
        }
        Ok(removed)
    }

    // ── member index ───────────────────────────────────────────────────────

    /// Mirrors `state.members` into the paging index. Departed members stay (inactive) so
    /// their old messages keep a sender name.
    pub fn sync_group_members(&self, state: &GroupState) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute(
            "UPDATE group_members SET active = 0 WHERE group_id = ?1",
            params![state.group.as_slice()],
        )?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO group_members (group_id, user_key, display_name, role, joined_seq, active)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)
                 ON CONFLICT (group_id, user_key) DO UPDATE SET display_name = excluded.display_name,
                     role = excluded.role, joined_seq = excluded.joined_seq, active = 1",
            )?;
            for (user, member) in &state.members {
                stmt.execute(params![
                    state.group.as_slice(),
                    user.as_slice(),
                    member.profile.display_name,
                    role_code(member.role),
                    member.joined_seq as i64
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A page of current members whose name contains `query` (case-insensitive), owner and
    /// admins first, then by name; plus the number of matches.
    pub fn group_members_page(
        &self,
        group: &GroupId,
        query: &str,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<MemberView>, u32)> {
        let pattern = format!("%{}%", query.replace(['%', '_'], ""));
        let total: i64 = self.db.query_row(
            "SELECT count(*) FROM group_members WHERE group_id = ?1 AND active = 1 AND display_name LIKE ?2",
            params![group.as_slice(), pattern],
            |row| row.get(0),
        )?;
        let mut stmt = self.db.prepare_cached(
            "SELECT user_key, display_name, role FROM group_members
             WHERE group_id = ?1 AND active = 1 AND display_name LIKE ?2
             ORDER BY role, display_name COLLATE NOCASE LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(
            params![group.as_slice(), pattern, i64::from(limit), i64::from(offset)],
            |row| {
                Ok(MemberView {
                    user_key: blob_array(row, 0)?,
                    display_name: row.get(1)?,
                    role: role_from_code(row.get(2)?),
                })
            },
        )?;
        Ok((rows.collect::<Result<_, _>>()?, total as u32))
    }

    // ── epochs, rekeys, chains ─────────────────────────────────────────────

    /// Returns `false` if the epoch was known.
    pub fn insert_epoch(&self, group: &GroupId, row: &EpochRow) -> Result<bool> {
        Ok(self.db.execute(
            "INSERT OR IGNORE INTO group_epochs (group_id, epoch, sealed, created_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                group.as_slice(),
                row.epoch.as_slice(),
                row.sealed,
                row.created_at_ms as i64
            ],
        )? == 1)
    }

    pub fn epochs(&self, group: &GroupId) -> Result<Vec<EpochRow>> {
        let mut stmt = self.db.prepare_cached(
            "SELECT epoch, sealed, created_at_ms, retired_at_ms FROM group_epochs
             WHERE group_id = ?1 ORDER BY created_at_ms",
        )?;
        let rows = stmt.query_map(params![group.as_slice()], epoch_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Marks every epoch except `keep` as retired at `now_ms` (keeping earlier retirements);
    /// `keep` itself is live again.
    pub fn retire_epochs_except(&self, group: &GroupId, keep: &EpochId, now_ms: u64) -> Result<()> {
        self.db.execute(
            "UPDATE group_epochs SET retired_at_ms = ?3
             WHERE group_id = ?1 AND epoch != ?2 AND retired_at_ms IS NULL",
            params![group.as_slice(), keep.as_slice(), now_ms as i64],
        )?;
        self.db.execute(
            "UPDATE group_epochs SET retired_at_ms = NULL WHERE group_id = ?1 AND epoch = ?2",
            params![group.as_slice(), keep.as_slice()],
        )?;
        Ok(())
    }

    /// Deletes secrets (and their chains) of epochs retired before `cutoff_ms` (SPEC.md §14.3).
    pub fn prune_epochs(&self, cutoff_ms: u64) -> Result<usize> {
        self.db.execute(
            "DELETE FROM group_chains WHERE EXISTS (SELECT 1 FROM group_epochs e
                 WHERE e.group_id = group_chains.group_id AND e.epoch = group_chains.epoch
                   AND e.retired_at_ms IS NOT NULL AND e.retired_at_ms < ?1)",
            params![cutoff_ms as i64],
        )?;
        Ok(self.db.execute(
            "DELETE FROM group_epochs WHERE retired_at_ms IS NOT NULL AND retired_at_ms < ?1",
            params![cutoff_ms as i64],
        )?)
    }

    /// Forgets every secret of a group we left or were removed from.
    pub fn drop_group_keys(&self, group: &GroupId) -> Result<()> {
        for table in ["group_epochs", "group_chains"] {
            self.db.execute(
                &format!("DELETE FROM {table} WHERE group_id = ?1"),
                params![group.as_slice()],
            )?;
        }
        Ok(())
    }

    pub fn insert_rekey(&self, rekey: &Rekey) -> Result<bool> {
        Ok(self.db.execute(
            "INSERT OR IGNORE INTO group_rekeys (group_id, epoch, record, opened) VALUES (?1, ?2, ?3, 0)",
            params![rekey.group.as_slice(), rekey.epoch.as_slice(), wire::encode(rekey)],
        )? == 1)
    }

    pub fn set_rekey_opened(&self, group: &GroupId, epoch: &EpochId) -> Result<()> {
        self.db.execute(
            "UPDATE group_rekeys SET opened = 1 WHERE group_id = ?1 AND epoch = ?2",
            params![group.as_slice(), epoch.as_slice()],
        )?;
        Ok(())
    }

    pub fn chain(&self, group: &GroupId, epoch: &EpochId, device: &DeviceKey) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row(
                "SELECT state FROM group_chains WHERE group_id = ?1 AND epoch = ?2 AND device = ?3",
                params![group.as_slice(), epoch.as_slice(), device.as_slice()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn save_chain(
        &self,
        group: &GroupId,
        epoch: &EpochId,
        device: &DeviceKey,
        sealed_state: &[u8],
    ) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO group_chains (group_id, epoch, device, state) VALUES (?1, ?2, ?3, ?4)",
            params![group.as_slice(), epoch.as_slice(), device.as_slice(), sealed_state],
        )?;
        Ok(())
    }

    // ── the group log (catch-up) ────────────────────────────────────────────

    /// Returns `false` if the unit was already in the log.
    pub fn log_insert(
        &self,
        group: &GroupId,
        id: &[u8; 32],
        sent_at_ms: u64,
        encoded: &[u8],
        now_ms: u64,
    ) -> Result<bool> {
        Ok(self.db.execute(
            "INSERT OR IGNORE INTO group_log (group_id, id, sent_at_ms, wire, processed, received_at_ms)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            params![group.as_slice(), id.as_slice(), sent_at_ms as i64, encoded, now_ms as i64],
        )? == 1)
    }

    pub fn log_mark_processed(&self, group: &GroupId, id: &[u8; 32]) -> Result<()> {
        self.db.execute(
            "UPDATE group_log SET processed = 1 WHERE group_id = ?1 AND id = ?2",
            params![group.as_slice(), id.as_slice()],
        )?;
        Ok(())
    }

    /// Units we could not apply yet (e.g. their epoch was unknown), oldest first.
    pub fn log_unprocessed(&self, group: &GroupId) -> Result<Vec<([u8; 32], Vec<u8>)>> {
        let mut stmt = self.db.prepare_cached(
            "SELECT id, wire FROM group_log WHERE group_id = ?1 AND processed = 0 ORDER BY sent_at_ms, id",
        )?;
        let rows = stmt.query_map(params![group.as_slice()], |row| {
            Ok((blob_array(row, 0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The newest `limit` units of the log as `(sent_at_ms, encoded)`.
    pub fn log_recent(&self, group: &GroupId, limit: u32) -> Result<Vec<(u64, Vec<u8>)>> {
        let mut stmt = self
            .db
            .prepare("SELECT sent_at_ms, wire FROM group_log WHERE group_id = ?1 ORDER BY sent_at_ms DESC LIMIT ?2")?;
        let rows = stmt.query_map(params![group.as_slice(), limit], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// `(sent_at_ms, id)` of the log since `since_ms`, sorted, for reconciliation.
    pub fn log_items(&self, group: &GroupId, since_ms: u64) -> Result<Vec<Item>> {
        let mut stmt = self.db.prepare_cached(
            "SELECT sent_at_ms, id FROM group_log WHERE group_id = ?1 AND sent_at_ms >= ?2
             ORDER BY sent_at_ms, id",
        )?;
        let rows = stmt.query_map(params![group.as_slice(), since_ms as i64], |row| {
            Ok(Item {
                sent_at_ms: row.get::<_, i64>(0)? as u64,
                id: blob_array(row, 1)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn log_get(&self, group: &GroupId, ids: &[[u8; 32]]) -> Result<Vec<Vec<u8>>> {
        let mut stmt = self
            .db
            .prepare_cached("SELECT wire FROM group_log WHERE group_id = ?1 AND id = ?2")?;
        let mut found = Vec::new();
        for id in ids {
            if let Some(bytes) = stmt
                .query_row(params![group.as_slice(), id.as_slice()], |row| row.get(0))
                .optional()?
            {
                found.push(bytes);
            }
        }
        Ok(found)
    }

    /// Drops log entries older than the serving window. Rekeys stay longer (they are small and
    /// needed by devices that come back late).
    pub fn prune_group_log(&self, cutoff_ms: u64) -> Result<usize> {
        Ok(self.db.execute(
            "DELETE FROM group_log WHERE sent_at_ms < ?1 AND processed = 1",
            params![cutoff_ms as i64],
        )?)
    }

    /// Keeps the newest `max_bytes` of one group's log (the per-group serving cap).
    pub fn cap_group_log(&self, group: &GroupId, max_bytes: u64) -> Result<usize> {
        let mut stmt = self.db.prepare_cached(
            "SELECT id, length(wire) FROM group_log WHERE group_id = ?1 ORDER BY sent_at_ms DESC, id DESC",
        )?;
        let rows: Vec<([u8; 32], i64)> = stmt
            .query_map(params![group.as_slice()], |row| {
                Ok((blob_array(row, 0)?, row.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut kept = 0u64;
        let mut removed = 0;
        for (id, len) in rows {
            kept += len as u64;
            if kept > max_bytes {
                removed += self.db.execute(
                    "DELETE FROM group_log WHERE group_id = ?1 AND id = ?2 AND processed = 1",
                    params![group.as_slice(), id.as_slice()],
                )?;
            }
        }
        Ok(removed)
    }

    // ── blobs ───────────────────────────────────────────────────────────────

    pub fn put_blob(&self, hash: &[u8; 32], data: &[u8], now_ms: u64) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO blobs (hash, data, stored_at_ms) VALUES (?1, ?2, ?3)",
            params![hash.as_slice(), data, now_ms as i64],
        )?;
        Ok(())
    }

    /// Bytes `offset..offset + len` of a blob (shorter at its end); `None` if unknown.
    pub fn blob_range(&self, hash: &[u8; 32], offset: u64, len: u64) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row(
                "SELECT substr(data, ?2, ?3) FROM blobs WHERE hash = ?1",
                params![hash.as_slice(), offset as i64 + 1, len as i64],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn prune_blobs(&self, cutoff_ms: u64) -> Result<usize> {
        Ok(self.db.execute(
            "DELETE FROM blobs WHERE stored_at_ms < ?1",
            params![cutoff_ms as i64],
        )?)
    }

    // ── pending logs (a welcome or sync whose op log is still being fetched) ──

    pub fn save_pending_log(&self, group: &GroupId, record: &[u8], now_ms: u64) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO group_pending_logs (group_id, record, received_at_ms) VALUES (?1, ?2, ?3)",
            params![group.as_slice(), record, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn pending_logs(&self) -> Result<Vec<(GroupId, Vec<u8>)>> {
        let mut stmt = self.db.prepare("SELECT group_id, record FROM group_pending_logs")?;
        let rows = stmt.query_map([], |row| Ok((blob_array(row, 0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn pending_log(&self, group: &GroupId) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row(
                "SELECT record FROM group_pending_logs WHERE group_id = ?1",
                params![group.as_slice()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn delete_pending_log(&self, group: &GroupId) -> Result<()> {
        self.db
            .execute("DELETE FROM group_pending_logs WHERE group_id = ?1", params![group.as_slice()])?;
        Ok(())
    }

    // ── invitations ─────────────────────────────────────────────────────────

    /// The invitee accepted; the row stays (shown as waiting) until the welcome is installed.
    pub fn set_invitation_accepted(&self, group: &GroupId) -> Result<Option<UserKey>> {
        Ok(self
            .db
            .query_row(
                "UPDATE group_invitations SET accepted = 1 WHERE group_id = ?1 RETURNING inviter",
                params![group.as_slice()],
                |row| blob_array(row, 0),
            )
            .optional()?)
    }

    pub fn save_invitation(&self, group: &GroupId, inviter: &UserKey, record: &[u8], now_ms: u64) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO group_invitations (group_id, inviter, record, received_at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![group.as_slice(), inviter.as_slice(), record, now_ms as i64],
        )?;
        Ok(())
    }

    /// Pending invitations, newest first.
    pub fn invitations(&self) -> Result<Vec<GroupInvitationView>> {
        let mut stmt = self.db.prepare(
            "SELECT i.inviter, i.record, i.accepted,
                    EXISTS (SELECT 1 FROM group_pending_logs p WHERE p.group_id = i.group_id)
             FROM group_invitations i ORDER BY i.received_at_ms DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let stage = match (row.get::<_, bool>(2)?, row.get::<_, bool>(3)?) {
                (_, true) => InvitationStage::Loading,
                (true, false) => InvitationStage::Accepted,
                (false, false) => InvitationStage::Open,
            };
            Ok((
                blob_array::<32>(row, 0)?,
                decode_column::<crate::group::join::GroupInvitation>(row, 1)?,
                stage,
            ))
        })?;
        let mut views = Vec::new();
        for row in rows {
            let (inviter, invitation, stage) = row?;
            let inviter_name = self
                .contact(&inviter)?
                .map(|contact| contact.display_name)
                .unwrap_or_default();
            views.push(GroupInvitationView {
                group: invitation.group,
                name: invitation.name,
                inviter,
                inviter_name,
                member_count: invitation.member_count,
                stage,
            });
        }
        Ok(views)
    }

    pub fn take_invitation(&self, group: &GroupId) -> Result<Option<(UserKey, Vec<u8>)>> {
        Ok(self
            .db
            .query_row(
                "DELETE FROM group_invitations WHERE group_id = ?1 RETURNING inviter, record",
                params![group.as_slice()],
                |row| Ok((blob_array(row, 0)?, row.get(1)?)),
            )
            .optional()?)
    }

    pub fn note_invite_sent(&self, group: &GroupId, user: &UserKey, now_ms: u64) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO group_invites_sent (group_id, user_key, sent_at_ms) VALUES (?1, ?2, ?3)",
            params![group.as_slice(), user.as_slice(), now_ms as i64],
        )?;
        Ok(())
    }

    /// Consumes our invitation of `user` to `group`; `false` if we never sent one.
    pub fn take_invite_sent(&self, group: &GroupId, user: &UserKey) -> Result<bool> {
        Ok(self.db.execute(
            "DELETE FROM group_invites_sent WHERE group_id = ?1 AND user_key = ?2",
            params![group.as_slice(), user.as_slice()],
        )? == 1)
    }

    // ── group peers (members who are not contacts) ─────────────────────────

    /// Stores a member's device list if it is newer than the known one; `false` otherwise.
    /// Members who are not contacts get no contact or device rows, so they cannot write 1:1.
    pub fn save_peer_device_list(&self, list: &DeviceList) -> Result<bool> {
        let known = self.peer_device_list(&list.user_key)?;
        if known.is_some_and(|known| known.seq >= list.seq) {
            return Ok(false);
        }
        self.db.execute(
            "INSERT OR REPLACE INTO group_peers (user_key, device_list) VALUES (?1, ?2)",
            params![list.user_key.as_slice(), wire::encode(list)],
        )?;
        Ok(true)
    }

    pub fn peer_device_list(&self, user: &UserKey) -> Result<Option<DeviceList>> {
        Ok(self
            .db
            .query_row(
                "SELECT device_list FROM group_peers WHERE user_key = ?1",
                params![user.as_slice()],
                |row| decode_column(row, 0),
            )
            .optional()?)
    }

    // ── device group keys ───────────────────────────────────────────────────

    /// Keeps the newest record per device.
    pub fn save_device_keys(&self, user: &UserKey, keys: &DeviceKeys) -> Result<()> {
        let known = self.device_keys(&keys.device_id)?;
        if known.is_some_and(|known| known.issued_at_ms >= keys.issued_at_ms) {
            return Ok(());
        }
        self.db.execute(
            "INSERT OR REPLACE INTO device_group_keys (device_id, user_key, record) VALUES (?1, ?2, ?3)",
            params![keys.device_id.as_slice(), user.as_slice(), wire::encode(keys)],
        )?;
        Ok(())
    }

    pub fn device_keys(&self, device: &DeviceKey) -> Result<Option<DeviceKeys>> {
        Ok(self
            .db
            .query_row(
                "SELECT record FROM device_group_keys WHERE device_id = ?1",
                params![device.as_slice()],
                |row| decode_column(row, 0),
            )
            .optional()?)
    }

    pub fn device_keys_of(&self, user: &UserKey) -> Result<Vec<DeviceKeys>> {
        let mut stmt = self
            .db
            .prepare_cached("SELECT record FROM device_group_keys WHERE user_key = ?1")?;
        let rows = stmt.query_map(params![user.as_slice()], |row| decode_column(row, 0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ── group reactions (many users per message) ────────────────────────────

    /// Stores `user`'s reaction unless a newer one is there. Returns the message's reactions if
    /// it changed anything.
    pub fn set_group_reaction(
        &self,
        conversation: &crate::conversation::ConversationId,
        user: &UserKey,
        mine: bool,
        reaction: &Reaction,
    ) -> Result<Option<ReactionsView>> {
        let changed = self.db.execute(
            "INSERT INTO group_reactions (conversation, message_id, user_key, mine, emoji, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (conversation, message_id, user_key) DO UPDATE SET emoji = excluded.emoji,
                 at_ms = excluded.at_ms WHERE excluded.at_ms > group_reactions.at_ms",
            params![
                conversation.to_stored(),
                reaction.message_id.as_slice(),
                user.as_slice(),
                mine,
                reaction.emoji,
                reaction.at_ms as i64
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some(
            self.group_reactions(conversation)?
                .remove(&reaction.message_id)
                .unwrap_or_default(),
        ))
    }

    /// Per message: our reaction and everyone else's, counted by emoji.
    pub(super) fn group_reactions(
        &self,
        conversation: &crate::conversation::ConversationId,
    ) -> Result<std::collections::HashMap<MessageId, ReactionsView>> {
        let mut stmt = self.db.prepare_cached(
            "SELECT message_id, mine, emoji FROM group_reactions
             WHERE conversation = ?1 AND emoji != '' ORDER BY at_ms",
        )?;
        let rows = stmt.query_map(params![conversation.to_stored()], |row| {
            Ok((blob_array::<16>(row, 0)?, row.get::<_, bool>(1)?, row.get::<_, String>(2)?))
        })?;
        let mut by_message = std::collections::HashMap::<MessageId, ReactionsView>::new();
        for row in rows {
            let (id, mine, emoji) = row?;
            let view = by_message.entry(id).or_default();
            if mine {
                view.mine = Some(emoji);
                continue;
            }
            match view.others.iter_mut().find(|(known, _)| *known == emoji) {
                Some((_, count)) => *count += 1,
                None => view.others.push((emoji, 1)),
            }
        }
        Ok(by_message)
    }
}

fn epoch_from_row(row: &rusqlite::Row) -> rusqlite::Result<EpochRow> {
    Ok(EpochRow {
        epoch: blob_array(row, 0)?,
        sealed: row.get(1)?,
        created_at_ms: row.get::<_, i64>(2)? as u64,
        retired_at_ms: row.get::<_, Option<i64>>(3)?.map(|at| at as u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        conversation::ConversationId,
        group::{oplog::fold, tests::TestUser},
    };

    #[test]
    fn groups_ops_members_and_log_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let owner = TestUser::new("Owner");
        let bob = TestUser::new("Bob");
        let create = owner.create("Club");
        let group = create.group;
        let add = owner.op(group, vec![create.hash()], bob.add_kind(&group, 0));
        assert!(store.insert_group_op(&create).unwrap());
        assert!(store.insert_group_op(&add).unwrap());
        assert!(!store.insert_group_op(&add).unwrap());
        let state = fold(group, None, &store.group_ops(&group).unwrap()).state;
        let view = GroupView {
            id: group,
            name: state.name.clone(),
            status: GroupStatus::Active,
            my_role: Some(Role::Owner),
            announcement: false,
            history_sharing: false,
            member_count: 2,
            owner: state.owner,
        };
        store.save_group(&state, &view, 1).unwrap();
        assert_eq!(store.group_views().unwrap(), vec![view]);
        store.sync_group_members(&state).unwrap();
        assert_eq!(store.active_group_ids().unwrap(), vec![group]);
        let loaded = store.load_group(&group).unwrap().unwrap();
        assert_eq!(loaded.state, state);

        let (page, total) = store.group_members_page(&group, "", 0, 10).unwrap();
        assert_eq!(total, 2);
        assert_eq!(page[0].role, Role::Owner, "owner first");
        let (found, total) = store.group_members_page(&group, "bo", 0, 10).unwrap();
        assert_eq!((found.len(), total), (1, 1));
        assert_eq!(found[0].user_key, bob.key());

        assert!(store.log_insert(&group, &[2; 32], 20, b"b", 1).unwrap());
        assert!(store.log_insert(&group, &[1; 32], 10, b"a", 1).unwrap());
        assert!(!store.log_insert(&group, &[1; 32], 10, b"a", 1).unwrap());
        let items = store.log_items(&group, 0).unwrap();
        assert_eq!(items.iter().map(|item| item.sent_at_ms).collect::<Vec<_>>(), vec![10, 20]);
        assert_eq!(store.log_unprocessed(&group).unwrap().len(), 2);
        store.log_mark_processed(&group, &[1; 32]).unwrap();
        assert_eq!(store.log_unprocessed(&group).unwrap().len(), 1);
        assert_eq!(store.log_get(&group, &[[2; 32], [9; 32]]).unwrap(), vec![b"b".to_vec()]);

        store.put_blob(&[5; 32], b"0123456789", 1).unwrap();
        assert_eq!(store.blob_range(&[5; 32], 2, 3).unwrap().unwrap(), b"234");
        assert_eq!(store.blob_range(&[5; 32], 8, 10).unwrap().unwrap(), b"89");
        assert!(store.blob_range(&[6; 32], 0, 1).unwrap().is_none());
    }

    #[test]
    fn epochs_retire_and_prune_and_reactions_count() {
        let store = Store::open_in_memory().unwrap();
        let group = [1; 32];
        let row = |epoch: u8| EpochRow {
            epoch: [epoch; 16],
            sealed: vec![epoch],
            created_at_ms: u64::from(epoch),
            retired_at_ms: None,
        };
        assert!(store.insert_epoch(&group, &row(1)).unwrap());
        assert!(store.insert_epoch(&group, &row(2)).unwrap());
        store.save_chain(&group, &[1; 16], &[3; 32], b"chain").unwrap();
        store.retire_epochs_except(&group, &[2; 16], 100).unwrap();
        assert_eq!(store.prune_epochs(50).unwrap(), 0);
        assert_eq!(store.prune_epochs(200).unwrap(), 1);
        assert!(store.chain(&group, &[1; 16], &[3; 32]).unwrap().is_none());
        assert_eq!(store.epochs(&group).unwrap().len(), 1);

        let chat = ConversationId::Group(group);
        let react = |user: u8, emoji: &str, at_ms| {
            store
                .set_group_reaction(
                    &chat,
                    &[user; 32],
                    user == 1,
                    &Reaction {
                        message_id: [7; 16],
                        emoji: emoji.into(),
                        at_ms,
                    },
                )
                .unwrap()
        };
        react(2, "👍", 1);
        react(3, "👍", 1);
        let view = react(1, "❤", 1).unwrap();
        assert_eq!(view.mine.as_deref(), Some("❤"));
        assert_eq!(view.others, vec![("👍".to_owned(), 2)]);
        assert!(react(3, "😂", 0).is_none(), "older than the stored one");
    }
}
