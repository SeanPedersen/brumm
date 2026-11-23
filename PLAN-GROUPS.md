# Implementation plan: groups (#24)

Hand-off file for implementing groups. The protocol is **SPEC.md §14**: read it first, together with CLAUDE.md
(conventions, startup and memory budget). This file turns §14 into ordered, testable steps. Tick the boxes as
phases land, and keep SPEC.md and CLAUDE.md in sync with what is actually built.

## Decisions (settled, do not reopen without the user)
- Up to **10 000 members**, all P2P: no own server. 1:1 chats and Notes stay on Olm, unchanged.
- **Encryption: HPKE rekey blobs + per-sender hash ratchets, not MLS** (numbers in SPEC §14.9).
- Roles **owner / admin / member**. The owner is fixed; only leaving moves it (earliest-promoted admin, else
  earliest-joined member). Only the owner manages admins; admins kick members only.
- **Announcement mode**: toggle at creation, owner can change it later.
- Members see the member list (accepted privacy cost, SPEC §14.8).
- Joining needs the invitee's consent (`GroupInvitation` → accept → `Add`). Newcomers cannot read earlier
  messages; sharing history is a per-group setting, off by default, labelled "shared by <admin>".
- Groups show ✓ only (no per-member ✓✓).
- iroh-blobs (M4) is pulled forward; iroh goes 1.4 → 1.5 (iroh-gossip 0.103 and iroh-blobs 0.105 need it).
- An external review of SPEC §14 before groups are announced as secure.
- Scaling measures (SPEC §14.2, §14.3, §14.4, §14.7): op-log **checkpoints**, **batched** removals
  (`REKEY_BATCH_WINDOW`), **replicated** rekey blobs (always-on members serve slots), **range-based set
  reconciliation** for catch-up, a bounded serving window (`LOG_SERVE_WINDOW`, per-group cap; phones/browsers don't serve).

## Ground rules for every phase
- Pure logic (op-log fold, epoch derivation, slot layout, ratchets) lives in modules without I/O, with unit tests,
  like `reconcile.rs`. The node actor only wires it up; network work is spawned, results return as `Outcome`.
- Wire: only append enum variants; existing 1:1 payloads keep `UserKey` fields (no wire change for 1:1).
- Schema changes bump `SCHEMA_VERSION` with an in-place migration step.
- Memory budget holds: never load all groups or a whole member list at startup; group state is loaded on use.
- Core code uses `n0_future::{task::spawn, time}`; everything must also build for `wasm32-unknown-unknown`.
- Run `cargo test`, `cargo clippy --all-targets` and the wasm clippy after each phase.

## G1: `ConversationId` refactor (no behaviour change)
- [x] `api.rs`: `ConversationId { Direct(UserKey), Group(GroupId) }` (Notes = `Direct(own key)` as today).
      Replace `contact: UserKey` in conversation-scoped commands/events/views (`LoadConversation`, `LoadOlder`,
      `RememberOpenChat`, `SendText`, `SendFile`, `DeleteMessages`, reactions, search, previews).
- [x] `store.rs`, schema v9: conversation columns store `tag byte ‖ 32 bytes` (`messages`, `reactions`,
      `image_previews`, `deleted_messages`, `outbox.chat_contact`, meta `last_opened`); the migration prefixes existing
      rows with the `Direct` tag. Add `messages.sender BLOB NULL` (NULL = derived from direction, as today).
- [x] `reconcile.rs` / `node/reconcile.rs`: buckets keyed by `ConversationId` internally; the wire `SyncBucket`
      keeps `contact: UserKey` for direct chats (groups get their own catch-up in G4).
- [x] App: `state.rs`, `ui/sidebar.rs`, `ui/chat.rs`, `ui/main_screen.rs`, `ui/chat_search.rs`, `ui/files.rs`,
      `ui/composer.rs`, `preview.rs`, `narrow.rs` key chats by `ConversationId`.
- [x] Done when: all existing tests pass unchanged (`tests/common` adapted), a v8 DB opens and migrates (unit test).
- As built: the columns were also renamed (`contact` → `conversation`, `outbox.chat_contact` → `outbox.chat`).
      Tests that build commands directly name the new `conversation` field; their assertions are unchanged.
      `narrow.rs` needed no change (it holds no chat key). Group conversations are refused by send/delete-for-everyone/
      react (`commands.rs direct_only`) and skipped by history sync and anti-entropy until G3/G4.

## G2: op log and membership (pure first)
- [x] `group/mod.rs`: `GroupId`, `GroupOp`, `OpKind`, `Role`, signing domain strings, constants
      (`MAX_GROUP_MEMBERS = 10_000`).
- [x] `group/oplog.rs` (pure): validate signature + author permission at fold position; deterministic topological
      fold (rank: owner, admins by promotion time, members; then hash); derived state: members, roles, owner
      (succession on leave), name, announcement flag, heads. Property tests: any delivery order → same state;
      racing kick/demote; owner leaves with and without admins.
- [x] Checkpoints (pure): `Checkpoint` every `CHECKPOINT_OPS` (1000) ops, signed by owner/admin; snapshot = members,
      roles, settings, device-key table; verify by refolding; start a state from checkpoint + later ops; prune ops
      before it. Test: checkpoint + tail folds to the same state as the full log; a forged snapshot is rejected.
- [x] `store.rs`: `groups (id, name, state_cache)`, `group_ops (group, hash, op)`, `group_members (group, user, role,
      joined_at)` index; peers' device lists in `devices`, peers as a hidden contact kind (`trust = GroupPeer`, not
      listed as chats).
- [x] Wire (1:1 Olm payloads, appended): `GroupInvitation`, `GroupInvitationReply`, `GroupWelcome` (op-log blob ref,
      post-add epoch secret, bootstrap peers), `GroupSync` (own devices: groups + epoch secrets, also on linking).
- [x] `node/groups.rs`: commands `CreateGroup { name, announcement }`, `InviteToGroup`, `AcceptGroupInvitation`,
      `DeclineGroupInvitation`, `RemoveMember`, `SetRole`, `RenameGroup`, `SetAnnouncement`, `LeaveGroup`.
      `inbound.rs`: accept group payloads from group peers only for shared groups.
- [x] Done when: oplog unit tests + a 3-node e2e (create, invite, accept, kick) on the op level (no messages yet).

## G3: keys and messages
- [x] Dependencies: `hpke` (X25519-HKDF-SHA256, ChaCha20-Poly1305; pure Rust, wasm OK), `chacha20poly1305`;
      KDFs via `blake3::derive_key` / keyed hash, matching the rest of the code. Check wasm builds.
- [x] `group/keys.rs` (pure): `DeviceKeys` record (signed by the iroh device key), epoch secrets, `add` ratchet,
      slot layout (sorted device ids → index → byte range), rekey blob build/open, sender hash ratchet with
      `MAX_SKIPPED_KEYS`, `EPOCH_GRACE` (7 days), choice rule between concurrent epochs (lower `epoch_id`).
- [x] `group/message.rs` (pure): `GroupMessage` seal/open/verify; `GroupPayload` enum (Chat, Delete, Reaction,
      File, DeviceKeys, DeviceList, KeyRequest, …).
- [x] Rekey triggers: remove/leave/revocation (author or any member), manual heal (`MIN_REKEY_INTERVAL`); key
      requests answered by any member after an op-log check. Removals batched for `REKEY_BATCH_WINDOW` (30 s) into
      one rekey. HPKE sealing parallel over cores (a 30k-device rekey is ~2.8 s on one core).
- [x] `store.rs`: `group_epochs`, `group_sender_chains`, device group key pair (never stored in plaintext beyond what
      the Olm pickles already do; follow the pickle-key pattern).
- [x] Done when: unit tests for every pure module; kicked device cannot open a message of the next epoch;
      concurrent rekeys both decrypt; skipped/out-of-order messages decrypt once.

## G4: distribution
- [x] iroh 1.5, `iroh-gossip` (`net`), `iroh-blobs` on the shared `Transport` endpoint/router (new ALPNs, `brumm/3`
      unchanged). Phones/browser: smaller HyParView active view, no forwarding.
- [x] **Blob store decision first:** iroh-blobs' fs-store (redb) does not run in the browser. Either a SQLite-backed
      store implementing the iroh-blobs store API, or MemStore + persistence of the blobs brumm needs. Spike it.
- [x] `node/group_net.rs`: one topic per active group, `topic = derive_key("brumm group topic v1", group_id)`;
      verify signature + membership + rate limit before forwarding; unknown/old epochs → key request.
- [x] Group log + catch-up: per-group ciphertext log kept for others for `LOG_SERVE_WINDOW` (14 days) up to a
      per-group cap; phones/browsers don't serve. Catch-up with ≤ 3 online members by **range-based set
      reconciliation** over `(sent_at_ms, id)` (negentropy-style; evaluate the `negentropy` crate vs. an own pure
      module, both native + wasm). Rekey records and ops (since the checkpoint) kept beyond message retention.
- [x] Rekey-blob replication: always-on members fetch the full blob on a rekey announcement and serve slots;
      slot fetches pick a random holder.
- [x] Nostr fallback: one letter per message when no gossip neighbour; `p` tag = secp256k1 key from the epoch secret;
      subscribe to live epochs' tags; 14-day expiry. Shutdown drain (`node/drain.rs`) includes queued group posts.
- [x] Files via blobs (encrypted once, key + hash in `GroupPayload::File`), group photo as blob.
- [x] Done when: N-node in-process tests (e.g. 20 nodes) pass: live delivery, catch-up from peers only, from Nostr
      only (`local-relay`), no admin online.

      **As built (SPEC §14.11):** no iroh-blobs (its redb store does not run in the browser); blobs live in a SQLite
      table and are served over `Frame::BlobGet`. Own range-reconciliation module (`group/sync.rs`), not `negentropy`.
      Replication, a 4-neighbour view in the browser, a 20-node test (`twenty_members_chat_and_go_on_without_an_admin`)
      and a blob-rekey test (`a_rekey_blob_is_replicated_and_served_by_members`) are in. Browsers still forward
      (iroh-gossip has no receive-only mode), and validation happens after forwarding (no pre-forward hook).

## G5: UI
- [x] Sidebar: groups mixed into the chat list (last activity), group avatar; "New group" (name, contacts,
      announcement toggle); invitation prompt (Join / Decline).
- [x] Chat: sender name + avatar on incoming bubbles, system lines (added, removed, left, renamed, new owner),
      composer disabled in announcement mode for members, "you were removed" state, ✓ only.
- [x] Members panel: virtualized list + search, roles, admin actions (add, kick, promote/demote for the owner),
      leave group, history-sharing setting. Narrow layout support.
- [ ] Done when: two local profiles + one browser profile can run a group end to end.

      Group photo (set/remove in Group info), dialogs that fit a phone screen (`theme::dialog`), member list laid out
      only for visible rows and paged from the core near the end. **Open:** the browser end-to-end run (manual).

## G6: hardening
- [x] Scale bench: 10k-member op log fold time, checkpoint size, rekey for 30k devices (parallel HPKE), memory
      per large group, catch-up bytes after 1 day / 1 week offline in a busy group.
- [x] 10k-node run of iroh-gossip's `simulator` feature: delivery, hop count, per-node bandwidth.
- [x] Tests from SPEC §14.10 G6 not covered yet; network test (ignored) over real relays.
      Bench: `group/bench.rs`; simulator config and numbers in SPEC §14.11 (iroh-gossip `sim`, run outside the repo).
- [x] SPEC.md §14 updated to what was built; CLAUDE.md status + layout; README feature list.
- [x] Prepare the external review brief (SPEC §14, `group/*.rs`): **REVIEW-GROUPS.md**.

## Not in this plan: client relays for 1:1 chats (#15)
Groups get member-served mailboxes (SPEC §14.4, item 2): online members keep and serve the group's ciphertext log.
1:1 chats still use only the Nostr relays when the recipient is offline. Letting contacts' (or own always-on)
devices hold 1:1 letters is #15 and is planned separately. Open points there: a contact holding A→B letters learns
that A writes to B (own devices do not have this problem), storage quotas, and how the sender picks a holder.

## Follow-ups after G6
- [x] Checkpoints as blobs (`GroupPayload::CheckpointBlob`); a 10k checkpoint (13.8 MiB) exceeds a 64 KiB unit.
      Blob fetches in pieces (`GroupTiming::blob_chunk_bytes`).
- [x] Welcome / `GroupSync` by pointer (`node/groups/pending.rs`): op log from members, plus Nostr chunk letters for
      logs ≤ 1 MiB, so joining works with the inviter offline. Accepted invitations show Waiting → Loading in the list.
- [x] Tests: checkpoint in several pieces, joining from a member, joining from relay chunks with nobody online.
- [x] 1 000-member e2e (`tests/large_group.rs`). Found and fixed: random peer picks never reached the few online
      devices (now ranked by last seen), and chunk posting to a dead relay held shutdown for minutes.
