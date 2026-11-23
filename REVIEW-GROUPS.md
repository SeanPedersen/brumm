# External review brief: brumm groups (#24)

brumm is a serverless, end-to-end encrypted P2P messenger (Rust, iroh). 1:1 chats use Olm (vodozemac).
Groups of up to 10 000 members use a small protocol of their own, built from standard parts (Ed25519, HPKE RFC 9180
X25519-HKDF-SHA256 + ChaCha20-Poly1305, BLAKE3 as KDF/hash), because pairwise fan-out does not scale and MLS needs a
delivery service we do not have (numbers in SPEC.md §14.9). We ask for a review of that protocol and its code.

## What to read
| | |
|---|---|
| Design | `SPEC.md` §14 (§14.11 lists where the code differs from the design) |
| Pure protocol (no I/O) | `crates/brumm-core/src/group/`: `mod.rs` (ops, consent, device keys), `oplog.rs` (fold, permissions, succession), `checkpoint.rs`, `epochs.rs` (add ratchet, complete epochs, choice), `keys.rs` (HPKE slots, rekey, sender chains, sealing at rest), `message.rs` (wire units, key request/grant), `join.rs`, `sync.rs` (range reconciliation) |
| Node glue | `crates/brumm-core/src/node/groups/`: `ops.rs`, `keys.rs` (when to rekey, grants), `messages.rs` (open/apply payloads), `net.rs` (gossip, log serving, blobs, Nostr), `join.rs` |
| Tests | unit tests in each `group/*.rs`; e2e in `crates/brumm-core/tests/groups.rs`; scale numbers in `group/bench.rs` |

About 6 400 lines of Rust (`group/`, `node/groups/`, `store/groups.rs`, unit tests included).
Run: `cargo test -p brumm-core` (e2e uses real iroh endpoints on localhost and an in-memory Nostr relay).

## Claimed properties
1. **Confidentiality against non-members**: units on gossip, in members' logs and on Nostr relays are AEAD-encrypted
   with per-sender-device chain keys derived from the epoch secret.
2. **Post-removal secrecy**: after a `Remove`/`Leave` (or a device revocation), a rekey to the remaining devices
   follows within the batch window (author: 30 s; anyone else: 60–90 s). Senders never use an epoch whose heads do not
   cover every membership op they know (`epochs.rs is_complete`, `choose`).
3. **No access to earlier history on join**: an `Add` ratchets the epoch secret one way
   (`ratchet_add`, keyed by the op hash); the newcomer receives only the ratcheted secret.
4. **Forward secrecy within an epoch**: message keys come from a hash ratchet per sender device; used keys are
   deleted, skipped keys kept up to 2 000.
5. **Authenticity**: every op is signed by the author's user key and checked against the folded state at its position
   (permissions, roles); every unit is signed by a device key and accepted only from a device in a member's signed
   device list. Messages are not deniable (same as MLS).
6. **Consent to membership**: an `Add` carries the newcomer's signature over `(group, rejoin count)`, so nobody is
   added without accepting, and an old consent cannot re-add someone who left.
7. **Convergence**: every device folds the op DAG to the same state regardless of delivery order; owner succession on
   leave needs no message.

## Not claimed / known limitations
- **Metadata**: members see the member list (user keys, device ids, names); gossip neighbours see IP addresses
  (browsers: only their relay's); relays see one letter per unit under rotating tags.
- **Gossip forwards before we validate.** iroh-gossip has no pre-forward hook, so a node forwards a unit before brumm
  checks its signature and the rate limit; receivers drop invalid units. Anyone who knows the topic id (derived from the
  group id, known to all present and past members) can make members forward junk. SPEC §14.4 asks for checking before
  forwarding; that needs an iroh-gossip change.
- **Removal window**: a removed member keeps reading until the rekey lands (see property 2).
- **At rest**: epoch secrets and chains are sealed with a key derived from the device secret, which itself sits in the
  same (unencrypted) SQLite database. This only keeps them out of plain dumps of single tables; real at-rest protection
  is milestone M5 (OS keychain + SQLCipher).
- **Checkpoints** are trusted by a newcomer as much as the admin who invited it; members holding the full log recheck
  them (`checkpoint.rs matches_log`). Checkpoints travel as blobs named by their hash
  (`GroupPayload::CheckpointBlob`); the hash is checked after fetching.
- **Welcome by pointer**: a newcomer to a larger group gets its op log from any member named in the welcome (or from
  Nostr chunk letters). It checks op signatures, the checkpoint signature and hash, and that the inviter is an admin and
  it is a member in the result, but a member can withhold recent ops (they arrive later through normal sync).
- **Shared history** in a welcome is asserted by the adding admin (shown as "History shared by …"), not by the original
  senders.

## Questions for the reviewers
1. `keys.rs`: slot construction (HPKE `info` = domain ‖ group ‖ epoch id, empty AAD), `epoch_id` as a public hash of
   the secret, and the KDF labels. Is anything reused across contexts?
2. `epochs.rs`: is "complete epoch with the lowest id" a sound choice rule? Can concurrent `Add`s, rekeys and removals
   make two honest devices send in epochs that a removed member can read?
3. `Rekey.extra` (keys of devices linked after their user's `Add`) is not checked by receivers against the members'
   current device lists; a malicious member could add an outsider's device. Members can always leak secrets, so we
   treated this as out of scope. Do you agree?
4. Large rekeys are a blob fetched by byte range from any member. A slot is not checked against the blob hash (no
   proof); a bad slot fails to open, the rekey is dropped, and the device falls back to a `KeyRequest`. Is that
   recovery acceptable, or should slots carry an inclusion proof?
5. `KeyGrant`: any member grants an epoch to a device whose user's latest `Add` (or the `Create`) is in the ancestry of
   that epoch's heads. Can a removed or re-added member obtain an epoch it should not have?
6. `oplog.rs`: fold order by rank then hash, and checking each op against the state at its position. Look for races
   (kick vs. demote, leave vs. promote, rejoin counting).
7. Sender chains: `MAX_SKIPPED_KEYS` and replay handling (a key once used is gone; duplicate units are deduplicated by
   unit id in the log).

## Scale (from `group/bench.rs` and the iroh-gossip simulator)
Tables in SPEC.md §14.11 (bench, simulator, 1 000-member e2e). In short: a 10k-member checkpoint is 13.8 MiB (sent as a blob), a 30k-device rekey 1.8 s /
2.3 MiB, the gossip topic delivered everything at 10k nodes with ~23 hops, and a welcome is always one Nostr letter
(the log comes from members, or for small groups also from chunk letters).
