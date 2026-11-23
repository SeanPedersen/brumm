//! Op logs too big to travel with a welcome or own-device sync (SPEC.md §14.6, §14.11).
//!
//! **Sender:** [`Node::log_source`] sends a small log inline. A larger one goes by
//! [`LogPointer`]: the checkpoint hash and a few member devices, plus, up to
//! `GroupTiming::nostr_log_bytes`, the whole log as Nostr chunk letters under a fresh tag.
//!
//! **Receiver:** the welcome or sync waits in `group_pending_logs` (the UI shows "Loading the
//! group…") while two paths race: fetching from the named members (checkpoint blob in pieces,
//! then the ops after it, page by page) and assembling the chunk letters. A big group always
//! has some member online; a small one can be joined from the relays alone. The first complete
//! log that admits us installs the group; a log that does not (a member that has not seen our
//! `Add` yet) is dropped and the fetch tried again later.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use super::net::fetch_chunks;
use crate::{
    clock::now_ms,
    group::{
        BlobHash, GroupId,
        checkpoint::Checkpoint,
        join::{
            BlobRef, ChunkAssembly, ChunkSet, GroupSync, GroupWelcome, LOG_CHUNK_BYTES, LogChunk, LogPointer,
            LogSource, MAX_FETCHED_LOG_BYTES, MAX_LOG_SOURCES, OpLog, split_log,
        },
        oplog,
    },
    identity::UserKey,
    mailbox,
    node::{Node, Outcome},
    wire::{self, Frame, GroupLogRequest, Reply},
};

/// Restarts of the ops paging when the source moved on to a newer checkpoint meanwhile.
const MAX_CHECKPOINT_MOVES: usize = 2;

/// A welcome or sync waiting for its op log (stored as is, completed with the log).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::node) enum PendingLog {
    Welcome { from: UserKey, welcome: GroupWelcome },
    Sync(GroupSync),
}

impl PendingLog {
    fn group(&self) -> GroupId {
        match self {
            Self::Welcome { welcome, .. } => welcome.group,
            Self::Sync(sync) => sync.group,
        }
    }

    fn pointer(&self) -> Option<&LogPointer> {
        let log = match self {
            Self::Welcome { welcome, .. } => &welcome.log,
            Self::Sync(sync) => &sync.log,
        };
        match log {
            LogSource::Pointer(pointer) => Some(pointer),
            LogSource::Inline(_) => None,
        }
    }
}

/// In-memory side of the pending logs: fetches in flight, last attempts, chunk assemblies.
#[derive(Default)]
pub(in crate::node) struct PendingFetches {
    fetching: HashSet<GroupId>,
    last_try_ms: HashMap<GroupId, u64>,
    assemblies: HashMap<BlobHash, (GroupId, ChunkAssembly)>,
    /// Devices known to be online for a group about to be installed (the member that served
    /// its log, the inviter's devices), so its first gossip and catch-up peers are live ones.
    seen_hints: HashMap<GroupId, Vec<[u8; 32]>>,
}

impl PendingFetches {
    pub(super) fn hint_seen(&mut self, group: GroupId, devices: impl IntoIterator<Item = [u8; 32]>) {
        self.seen_hints.entry(group).or_default().extend(devices);
    }

    pub(super) fn take_seen_hints(&mut self, group: &GroupId) -> Vec<[u8; 32]> {
        self.seen_hints.remove(group).unwrap_or_default()
    }
}

impl Node {
    // ── sender ──────────────────────────────────────────────────────────────

    /// The op log for a welcome or sync to `recipients` (devices that must not be named as
    /// sources), whose mailboxes are on `relays`.
    pub(super) fn log_source(&mut self, group: &GroupId, recipients: &[[u8; 32]], relays: Vec<String>) -> Result<LogSource> {
        let runtime = self.group(group)?;
        let log = OpLog {
            checkpoint: runtime.checkpoint.clone(),
            ops: runtime.ops.clone(),
        };
        let encoded = wire::encode(&log);
        if encoded.len() <= self.group_timing.inline_log_bytes {
            return Ok(LogSource::Inline(Box::new(log)));
        }
        let me = self.active_me()?;
        let own = *me.device_id.as_bytes();
        let mailbox = me.mailbox.clone();
        let runtime = self.group(group)?;
        // The most recently seen members first: they are the likeliest to be online later.
        let sources: Vec<[u8; 32]> = std::iter::once(own)
            .chain(
                runtime
                    .pick_peers(&own, MAX_LOG_SOURCES + recipients.len(), None)
                    .into_iter()
                    .filter(|device| !recipients.contains(device)),
            )
            .take(MAX_LOG_SOURCES)
            .collect();
        let checkpoint = log.checkpoint.as_ref().map(|checkpoint| BlobRef {
            blob: checkpoint.hash(),
            len: wire::encode(checkpoint).len() as u64,
        });
        let chunks = match mailbox {
            Some(mailbox) if encoded.len() <= self.group_timing.nostr_log_bytes => {
                self.group_posting += 1;
                Some(post_log_chunks(mailbox, &encoded, relays, self.outcomes_tx.clone()))
            }
            _ => None,
        };
        Ok(LogSource::Pointer(LogPointer {
            checkpoint,
            sources,
            chunks,
        }))
    }

    // ── receiver ────────────────────────────────────────────────────────────

    /// Keeps a welcome or sync until its log is here, and starts getting it.
    pub(super) fn wait_for_log(&mut self, pending: PendingLog) -> Result<()> {
        let group = pending.group();
        self.store.save_pending_log(&group, &wire::encode(&pending), now_ms())?;
        self.pending_fetches.last_try_ms.remove(&group);
        self.emit_invitations()?;
        self.update_group_tags()?;
        self.fetch_pending_log(&group)
    }

    /// Group tick: tries the members again for every log still missing.
    pub(super) fn retry_pending_logs(&mut self) -> Result<()> {
        let interval = self.group_timing.catch_up_interval.as_millis() as u64;
        let now = now_ms();
        for (group, _) in self.store.pending_logs()? {
            let last = self.pending_fetches.last_try_ms.get(&group).copied().unwrap_or(0);
            if now.saturating_sub(last) >= interval {
                self.fetch_pending_log(&group)?;
            }
        }
        Ok(())
    }

    fn pending_log(&self, group: &GroupId) -> Result<Option<PendingLog>> {
        Ok(match self.store.pending_log(group)? {
            Some(bytes) => wire::decode(&bytes).ok(),
            None => None,
        })
    }

    fn fetch_pending_log(&mut self, group: &GroupId) -> Result<()> {
        if self.pending_fetches.fetching.contains(group) {
            return Ok(());
        }
        let Some(pending) = self.pending_log(group)? else {
            return Ok(());
        };
        let Some(pointer) = pending.pointer().cloned() else {
            return Ok(());
        };
        let me = self.active_me()?;
        let transport = me.transport.clone();
        let own = *me.device_id.as_bytes();
        let sources: Vec<EndpointId> = pointer
            .sources
            .iter()
            .filter(|device| **device != own)
            .filter_map(|device| super::device_endpoint(device).ok())
            .collect();
        self.pending_fetches.fetching.insert(*group);
        self.pending_fetches.last_try_ms.insert(*group, now_ms());
        let outcomes = self.outcomes_tx.clone();
        let chunk_bytes = self.group_timing.blob_chunk_bytes;
        let group = *group;
        n0_future::task::spawn(async move {
            let mut log = None;
            let mut from = None;
            for source in sources {
                match fetch_log(&transport, source, group, pointer.checkpoint, chunk_bytes).await {
                    Ok(fetched) => {
                        log = Some(Box::new(fetched));
                        from = Some(*source.as_bytes());
                        break;
                    }
                    Err(err) => tracing::debug!(%source, ?err, "op log fetch failed"),
                }
            }
            let _ = outcomes.send(Outcome::GroupLogFetched { group, log, from });
        });
        Ok(())
    }

    pub(in crate::node) fn finish_log_fetch(
        &mut self,
        group: GroupId,
        log: Option<Box<OpLog>>,
        from: Option<[u8; 32]>,
    ) -> Result<()> {
        self.pending_fetches.fetching.remove(&group);
        self.pending_fetches.hint_seen(group, from);
        match log {
            Some(log) => self.complete_pending_log(group, *log),
            None => Ok(()),
        }
    }

    /// A letter of some pending log's chunk set.
    pub(in crate::node) fn receive_log_chunk(&mut self, bytes: &[u8]) -> Result<()> {
        let Ok(chunk) = wire::decode::<LogChunk>(bytes) else {
            return Ok(());
        };
        if !self.pending_fetches.assemblies.contains_key(&chunk.log) {
            let Some((group, set)) = self.chunk_set_for(&chunk.log)? else {
                return Ok(());
            };
            let Some(assembly) = ChunkAssembly::new(&set) else {
                return Ok(());
            };
            self.pending_fetches.assemblies.insert(chunk.log, (group, assembly));
        }
        let (group, assembly) = self
            .pending_fetches
            .assemblies
            .get_mut(&chunk.log)
            .context("assembly inserted above")?;
        let group = *group;
        let Some(whole) = assembly.insert(chunk) else {
            return Ok(());
        };
        match wire::decode::<OpLog>(&whole) {
            Ok(log) => self.complete_pending_log(group, log),
            Err(err) => {
                tracing::info!(?err, "undecodable op log from chunk letters");
                Ok(())
            }
        }
    }

    fn chunk_set_for(&self, log: &BlobHash) -> Result<Option<(GroupId, ChunkSet)>> {
        for (group, bytes) in self.store.pending_logs()? {
            let Ok(pending) = wire::decode::<PendingLog>(&bytes) else {
                continue;
            };
            if let Some(set) = pending.pointer().and_then(|pointer| pointer.chunks.clone())
                && set.log.blob == *log
            {
                return Ok(Some((group, set)));
            }
        }
        Ok(None)
    }

    /// Tags (and the earliest posting time) of every pending log's chunk letters.
    pub(super) fn pending_log_tags(&self) -> Result<(Vec<nostr_sdk::prelude::Keys>, u64)> {
        let mut tags = Vec::new();
        let mut since = u64::MAX;
        for (_, bytes) in self.store.pending_logs()? {
            let Ok(pending) = wire::decode::<PendingLog>(&bytes) else {
                continue;
            };
            if let Some(set) = pending.pointer().and_then(|pointer| pointer.chunks.as_ref()) {
                tags.push(mailbox::group_tag_keys(&set.tag_seed));
                since = since.min(set.posted_at_secs);
            }
        }
        Ok((tags, if since == u64::MAX { 0 } else { since }))
    }

    /// Installs the group from the pending record if `log` admits us; otherwise keeps waiting.
    fn complete_pending_log(&mut self, group: GroupId, log: OpLog) -> Result<()> {
        let Some(pending) = self.pending_log(&group)? else {
            return Ok(());
        };
        let me = self.me()?.user_key();
        let folded = oplog::fold(group, log.checkpoint.as_ref(), &log.ops);
        let admits = match &pending {
            PendingLog::Welcome { from, .. } => folded.state.is_admin(from) && folded.state.is_member(&me),
            PendingLog::Sync(_) => folded.state.created || log.checkpoint.is_some(),
        };
        if !admits {
            tracing::info!("fetched op log does not admit us yet; trying again later");
            return Ok(());
        }
        self.store.delete_pending_log(&group)?;
        self.pending_fetches.assemblies.retain(|_, (pending_group, _)| *pending_group != group);
        self.pending_fetches.last_try_ms.remove(&group);
        let accepted = match pending {
            PendingLog::Welcome { from, mut welcome } => {
                welcome.log = LogSource::Inline(Box::new(log));
                self.receive_group_welcome(from, welcome)?
            }
            PendingLog::Sync(mut sync) => {
                sync.log = LogSource::Inline(Box::new(log));
                self.receive_group_sync(sync)?
            }
        };
        if !accepted {
            tracing::info!("pending group record refused after its log arrived");
        }
        self.emit_invitations()?;
        self.update_group_tags()
    }
}

/// Posts `log` as chunk letters under a fresh tag (one task, letter by letter: relays rate
/// limit) and returns the record that lets the recipient find them.
fn post_log_chunks(
    mailbox: std::sync::Arc<mailbox::Mailbox>,
    log: &[u8],
    relays: Vec<String>,
    outcomes: tokio::sync::mpsc::UnboundedSender<Outcome>,
) -> ChunkSet {
    let tag_seed: [u8; 32] = rand::random();
    let chunks = split_log(log, LOG_CHUNK_BYTES);
    let set = ChunkSet {
        tag_seed,
        log: BlobRef {
            blob: *blake3::hash(log).as_bytes(),
            len: log.len() as u64,
        },
        count: chunks.len() as u32,
        posted_at_secs: now_ms() / 1000,
    };
    n0_future::task::spawn(async move {
        let tag = mailbox::group_tag_keys(&tag_seed);
        for chunk in chunks {
            if let Err(err) = mailbox.post_group(&tag, &wire::encode(&chunk), &relays).await {
                // An incomplete set is useless, and each further try waits for the relays again.
                tracing::info!(?err, "log chunks not posted; the newcomer fetches from members");
                break;
            }
        }
        let _ = outcomes.send(Outcome::LogChunksPosted);
    });
    set
}

/// The checkpoint (by hash, in pieces) and then the ops after it, page by page, from one member.
async fn fetch_log(
    transport: &crate::transport::Transport,
    source: EndpointId,
    group: GroupId,
    mut checkpoint: Option<BlobRef>,
    chunk_bytes: u64,
) -> Result<OpLog> {
    for _ in 0..=MAX_CHECKPOINT_MOVES {
        let fetched = match checkpoint {
            Some(blob) => Some(fetch_checkpoint(transport, source, blob, chunk_bytes).await?),
            None => None,
        };
        let mut ops = Vec::new();
        let mut bytes = 0u64;
        let moved = loop {
            let request = Frame::GroupLog(GroupLogRequest::Ops {
                group,
                checkpoint: checkpoint.map(|blob| blob.blob),
                skip: ops.len() as u32,
            });
            let Reply::GroupLogOps { checkpoint: theirs, ops: page, more } = transport.request(source, &request).await?
            else {
                bail!("member refused the op log");
            };
            if theirs.map(|blob| blob.blob) != checkpoint.map(|blob| blob.blob) {
                break Some(theirs);
            }
            bytes += page.iter().map(|op| wire::encode(op).len() as u64).sum::<u64>();
            if bytes > MAX_FETCHED_LOG_BYTES {
                bail!("op log too large");
            }
            let last_page = page.is_empty() || !more;
            ops.extend(page);
            if last_page {
                break None;
            }
        };
        match moved {
            None => return Ok(OpLog { checkpoint: fetched, ops }),
            // The member checkpointed (or never saw ours): go on from theirs.
            Some(Some(theirs)) => checkpoint = Some(theirs),
            Some(None) => bail!("member has no checkpoint"),
        }
    }
    bail!("member kept moving its checkpoint")
}

async fn fetch_checkpoint(
    transport: &crate::transport::Transport,
    source: EndpointId,
    blob: BlobRef,
    chunk_bytes: u64,
) -> Result<Checkpoint> {
    if blob.len > MAX_FETCHED_LOG_BYTES {
        bail!("checkpoint too large");
    }
    let bytes = fetch_chunks(transport, &[source], blob.blob, 0, blob.len, chunk_bytes)
        .await
        .context("member does not have the checkpoint")?;
    if *blake3::hash(&bytes).as_bytes() != blob.blob {
        bail!("checkpoint does not match its hash");
    }
    let checkpoint: Checkpoint = wire::decode(&bytes)?;
    if !checkpoint.verify() {
        bail!("checkpoint signature or shape invalid");
    }
    Ok(checkpoint)
}
