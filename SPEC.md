# brumm — Specification (MVP v0.1)

> A fun, secure, privacy-respecting, decentralized messenger.
> The client owns identity and history. Devices talk directly peer-to-peer; public Nostr relays
> store end-to-end encrypted envelopes only while a recipient device is offline.

Status legend: **[M1]** … **[M5]** = milestone that implements it, **[later]** = post-MVP.

---

## 1. Goals & non-goals

### Goals (v0.1)
- Desktop app (Linux, macOS, Windows) built with **egui/eframe**.
- **No phone number, no account server.** Identity = keypair derived from a 24-word seed phrase.
- **Multi-device**: one identity on several devices; link, revoke, recover from the seed.
- **1:1 chats**: text + file transfer.
- **E2EE content** and **no social graph visible to infrastructure**.
- **Direct P2P first** (iroh); **offline fallback** via public Nostr relays.
- Contacts added by **QR / invite link** or by **introduction** from an existing contact.

### Non-goals (v0.1) — see §13 roadmap
Groups (designed for after v0.1, §14), stickers, mobile, global username search, BLE / Wi-Fi mesh, calls, reactions/replies/edits,
read receipts, typing indicators, Tor / relay-only mode, own mailbox server.

---

## 2. Architecture overview

```
┌──────────────────────── brumm-app (egui) ────────────────────────┐
│  screens: onboarding · contacts · chat · add-contact · settings  │
│  AppState  ◄── Event ──┐                 ┌── Command ──► core    │
└────────────────────────┼─────────────────┼───────────────────────┘
                         │   tokio mpsc    │
┌────────────────────────┴── brumm-core ───┴───────────────────────┐
│ node (actor: owns store, routes commands / inbound / results)    │
│  ├─ identity    user key, device keys, device certs/lists        │
│  ├─ invite      ticket encode/decode, single-use secrets         │
│  ├─ wire        versioned frames (postcard)                      │
│  ├─ crypto/e2ee vodozemac Olm sessions per device pair     [M2]  │
│  ├─ transport   iroh endpoint, ALPN brumm/1, conn cache          │
│  ├─ mailbox     trait Mailbox + NostrMailbox               [M3]  │
│  ├─ files       iroh-blobs offers / fetch                  [M4]  │
│  └─ store       SQLite (SQLCipher from M2), retention            │
└──────────────────────────────────────────────────────────────────┘
        │ iroh QUIC (direct / iroh relay)        │ Nostr websockets
        ▼                                        ▼
   other devices                           public Nostr relays
```

The UI never awaits: it pushes `Command`s into an unbounded channel and drains `Event`s every frame;
the core wakes the UI via a repaint callback.

---

## 3. Identity

### 3.1 Keys
| Key | Type | Lifetime | Where |
|---|---|---|---|
| **User key** `UK` | Ed25519 | forever (recoverable) | derived from seed; **stored on every linked device** |
| **Device key** `DK` | Ed25519 (= iroh `EndpointId`) | per device install | random, never leaves the device |
| **Olm identity** `OK` | Curve25519 (vodozemac account) | per device | bound to the user via `DeviceCert` [M2] |

`UK = Ed25519::from_bytes(blake3::derive_key("brumm 2026 user identity key v1", bip39_seed))`

### 3.2 Device certificate & device list [Multi ✅]
```
DeviceCert  { user_key: UK_pub, device_id: DK_pub, olm_identity_key: OK_pub, issued_at_ms, sig_UK(...) }
DeviceEntry { cert: DeviceCert, fallback_key: Olm fallback key, mailbox: MailboxAddress { inbox_key, relays }, label }
DeviceList  { user_key, seq: u64, entries: Vec<DeviceEntry> (≤ 8), sig_UK(domain ‖ user_key ‖ seq ‖ entries) }
Profile     { display_name, devices: DeviceList }
```
- A user's devices are exactly the entries of their highest-`seq` valid list. A list is valid when the signature
  checks out, every cert is valid for the same `UK`, device ids are unique, and it has 1–8 entries.
- `seq = max(prev + 1, now_ms)`: strictly increasing, and a device restored from the seed
  supersedes lists it never saw. Lists with `seq` ≤ the known `seq` are ignored (no rollback).
- The fallback key lets any device open an Olm session to any listed device without a one-time key.
- Distribution: inside Hello/HelloAck, as `Payload::DeviceList` to every device of every contact
  (and our own devices) on change. [M3] Also as a pkarr record under `UK` for introductions.

### 3.3 Fingerprint
`fingerprint = base32(blake3(UK_pub)[..10])` shown as `ABCD-EFGH-…`. Shown on the profile page and
compared when verifying in person.

### 3.4 Device linking & revocation [Multi ✅]
```
new device:  StartLinking → shows brumm://link/… =
             LinkTicket { device_addr, link_secret, olm_identity_key, one_time_key, fallback_key, label }
old device:  LinkDevice(link) → certifies the new device, signs DeviceList seq' including it
             → Frame::Link { link_secret, sealed: Olm pre-key(LinkBundle { seed_phrase, display_name, devices }) }
new device:  checks the secret, that the seed derives devices.user_key, that the list is valid and lists itself (same Olm + fallback key)
             and the sender (same Olm key as the pre-key) → stores the identity → Reply::Ack { id: link_secret }
old device:  on ack: adopts the list, then queues to the new device ContactSync per contact,
             a HistoryManifest (messages per conversation), and History chunks (≤ 32 KiB each; last 60 days,
             the Notes chat in full), and announces the list to all contacts + own devices
             chunk order = priority: one conversation at a time, most recently active first, newest messages first
new device:  counts arriving items per conversation against the manifest (persisted) → per-chat "syncing" hint
```
- **Removal**: any device signs a list without the target, sends it directly to the removed device (best effort),
  and announces it. A device that receives its own user's list without itself marks itself
  **revoked** (shown in the UI, no further sending or receiving).
- **Recovery**: fresh install + seed phrase → new `DK`, new single-device list. Contacts and chats are
  not recovered (use linking to keep them). Contacts must re-add.

---

## 4. Discovery

| Layer | Question | Mechanism |
|---|---|---|
| Device | "where is `DK`?" | iroh address lookup: n0 DNS / pkarr (relay URL only, no IPs) [M1]; Mainline DHT [later]; mDNS LAN [later] |
| User → devices | "which devices does `UK` have?" | invite / hello [M1]; signed `DeviceList` via pkarr + contact push [multi-device] |
| Human → user | "who is Bob?" | **QR / invite link** (trust: *verified*) or **contact card introduction** (trust: *introduced by X*). **No name search.** |

Names are labels, never lookup keys ⇒ squatting is impossible. Impersonation is mitigated by trust
badges, local nicknames and fingerprints.

---

## 5. Contact establishment

### 5.1 Invite (QR / link) [M1]
```
Invite { v: 3, display_name, device_cert: DeviceCert, device_addr: EndpointAddr, secret: [u8;16], one_time_key: OTK_pub }
Profile { user_key, display_name, device_cert }
link = "brumm://invite/" + base32(postcard(Invite))
```
- The inviter stores `secret` as a pending single-use invite.
- The invite carries only the inviting device's cert, so it stays QR-sized. The full `Profile` (all devices)
  is exchanged inside the encrypted Hello/HelloAck.
- The invitee opens an outbound Olm session from `(invite.cert.olm_identity_key, invite.one_time_key)`
  and sends `Hello { invite_secret, sealed: Olm pre-key message(Profile) }`.
- The inviter consumes the single-use secret, opens the inbound session (consuming the one-time
  key), and checks that the cert is valid, `cert.device_id == connection.remote_id()` and
  `cert.olm_identity_key == pre-key sender key`. It then stores the contact (trust = verified) and the
  session, and replies `HelloAck { sealed: Olm(Profile) }`.
- The invitee decrypts it and checks that the user key and inviting-device cert match the invite, then stores the
  contact and the session. Profiles never travel in plaintext.
- Both sides sync the new contact to their other devices (`Payload::ContactSync`), and those devices open
  sessions to the contact's devices lazily via fallback keys.
- [M3] Invite carries the inviter's Nostr relay list + an inbox tag, so the invite works offline.

### 5.2 Introduction (contact card) [M3]
```
ContactCard { user_key, display_name, intro: Intro }
Intro { introducer: UK_alice, introducee: UK_carol, subject: UK_bob, ts, sig_UK_alice }
```
1. Alice sends Carol a `ContactCard` for Bob (E2EE message).
2. Carol resolves `UK_bob` → signed user record (device list + Olm fallback keys + relays + request tag).
3. Carol sends `ContactRequest { profile_carol, intro }`, encrypted to Bob's fallback keys, directly or
   to Bob's request tag on Nostr.
4. Bob accepts only if `intro.introducer` is an existing contact and the signature verifies.
   **Requests without a valid intro are dropped** (no UI).

---

## 6. Messaging

### 6.1 Wire frames (`brumm/3`, postcard, max 64 KiB per frame) [Multi ✅]
```
Frame::Hello  { invite_secret, sealed: Olm(Profile) }       → Reply::HelloAck { sealed: Olm(Profile) } | Rejected
Frame::Link   { link_secret,   sealed: Olm(LinkBundle) }    → Reply::Ack { id: link_secret }          | Rejected
Frame::Sealed(Olm(Envelope { id, payload }))                → Reply::Ack { id }                       | Rejected
Payload::Chat(ChatMessage)                          contact → contact
Payload::DeviceList(DeviceList)                     any device of the list's owner
Payload::SentTranscript { contact, ChatMessage }    own device only
Payload::ContactSync(ContactRecord { profile, added_at_ms })   own device only
Payload::History(Vec<HistoryItem>)                  own device only
Payload::HistoryManifest(Vec<HistoryCount { contact, messages }>)   own device only, ahead of the History chunks
Payload::Receipt(Vec<EnvelopeId>)                   any known device (ack for mailbox-delivered envelopes)
Payload::SyncDigest / SyncHave / SyncMessages       own device only, consistency check (§6.4)
RejectReason: UnknownInvite | InvalidProfile | UnknownSender | InvalidMessage | Undecryptable | TryLater
Sealed { message_type: u8, ciphertext }             # vodozemac OlmMessage::to_parts
```
One request/response per QUIC bi-stream. Connections are cached per peer device.
Acks are plaintext random ids over QUIC/TLS. They become sealed payloads when the mailbox arrives (M3).

### 6.2 Delivery: per-device outbox [Multi ✅]
```
send_text(contact, text):
    for d in devices(contact):   outbox += (d, Envelope{ id = message.id, Chat })        # chat = Direct(contact)
    for d in my other devices:   outbox += (d, Envelope{ random id, SentTranscript })
each outbox row stores the Olm ciphertext, **sealed once** at enqueue time (plus the plaintext envelope, for one reseal)
flush (on enqueue, after each ack, every 30 s): send rows in queue order, ≤ 2 in flight per device
             # queue order is the priority (history sync); the next row starts when one is acked
Ack{id}                     → drop row; chat rows mark the message Delivered (any device suffices)
Rejected(InvalidMessage)    → drop row; chat → Failed (unless already Delivered)   # the payload itself is refused
Rejected(Undecryptable)     → reseal once on a fresh session (from the device's fallback key), keep the queue position;
                              a second Undecryptable → drop row, chat → Failed
Rejected(other) / unreachable → keep row, retry   # e.g. contact hasn't processed our new device list yet
unreachable → also post the same ciphertext once to the device's Nostr mailbox (§8) → Sent
              (every row queued for that device, not only the ones in flight)
```
- **Seal once, resend the same bytes.** Re-encrypting on every retry would advance the ratchet without bound while a
  device is offline, beyond Olm's skipped-message-key window.
- **Receivers reject finally only what they refuse.** A ciphertext no session opens (the receiver lost or evicted the
  session) is `Undecryptable`, a local failure (storage) is `TryLater`; neither ever makes the sender drop the envelope.
  The fresh session for a reseal is shared: it is only started again if the failed envelope was sealed after it.
- The receiver keeps `blake3(ciphertext) → envelope id` for processed envelopes and **re-acks duplicates**
  (a retry whose ack was lost) instead of failing to decrypt. Pruned with message retention.
- Status shown in the UI: `Pending` 🕓 → `Sent` ✔ (in the mailbox) → `Delivered` ✔✔ (acknowledged by at least one
  device of the contact), or `Failed`.

### 6.3 Ordering & deduplication
- `id` is random; receivers deduplicate by `(sender_user, id)`.
- Messages are shown in `sent_at_ms` order (sender clock, ties in arrival order); the receive time is stored too.
  A message arriving late (mailbox, sync) is inserted at its place in the open chat, not appended.
- Deletions are final: a deleted `(conversation, id)` is remembered (`deleted_messages`) and never inserted again, also
  when the delete arrives before the message. Kept as long as messages are (Notes: forever).

### 6.4 Own-device consistency check (anti-entropy) [Multi ✅]
Delivery is reliable, but anything lost on the way (a bug, a dropped session, a relay that lost a letter) would stay
lost. So every pair of own devices compares what it holds and repairs the difference:
```
A → B  SyncDigest { since_ms, [(conversation, week, blake3(sorted ids)[..16])] }     direct, no outbox
B → A  SyncHave   { since_ms, [(conversation, week, all of B's ids)] }  for differing buckets, direct
A → B  SyncMessages(HistoryItem…)  what B lacks                                       via outbox
       DeleteSync                  what B has but A deleted                           via outbox
B: SyncMessages item that B deleted → refused, DeleteSync back to A
```
- Window: Notes in full, other chats from `now − retention + 1 day` (both sides use the sender's `since_ms`).
- A round starts at startup, when the other device acks or sends something directly, and every 5 minutes while it was
  seen in the last 6 rounds; at most once per interval per device, and **never while envelopes for it are queued**
  (B does not answer while it has envelopes queued for A). The reverse direction is B's own round.
- Digest and answer go out directly (no outbox), so an offline device does not accumulate them; the repairs are
  ordinary envelopes. Reactions and delivery status are not compared.

---

## 7. Encryption [M2]
- **vodozemac Olm** (Double Ratchet, `SessionConfig::version_1`) sessions **per device pair**; each
  message is encrypted for every recipient device and each of my other devices (Sesame-style).
- Sessions are opened from the invite's one-time key, the link ticket's one-time key, or lazily from a
  listed device's **fallback key**.
- **Up to 4 sessions per device** are kept. Decryption tries all of them, and encryption uses the one most recently
  received on. Two devices opening sessions at the same time therefore converge (as in Matrix clients).
- Session bootstrap: the one-time key from the invite [M2]; the fallback key from the user record for
  introductions [M3].
- Ratchet state is written through to the DB after every encrypt/decrypt. If an existing session
  can't decrypt a pre-key message from the peer's *certified* Olm key, a new inbound session replaces it.
- Account and session pickles are encrypted with `pickle_key = blake3::derive_key(ctx, device_secret)`.
- iroh QUIC/TLS (authenticated by `DK`) is a second, transport-level layer.
- [later] Groups: epoch keys handed out as HPKE rekey blobs plus per-sender hash ratchets (§14). **Not Megolm**, and
  not MLS: without a delivery service its commits grow linearly in large groups (§14.9).

### 7.1 Crypto trust assumptions (relation to Matrix)
vodozemac is Matrix's Olm implementation, so Matrix's E2EE criticism needs a precise answer.
- **Used:** only pairwise Olm (Double Ratchet: forward secrecy + post-compromise security per chain).
  vodozemac is a small, pure crypto library (no network or room logic) and had an external audit (Least Authority, 2022).
- **Not used:** Megolm (weak group forward secrecy and recovery after compromise), Matrix cross-signing,
  server-mediated device lists, key backup and key requests, homeservers. The published attacks on Matrix
  (e.g. "Practically-exploitable Cryptographic Vulnerabilities in Matrix", 2023) target that protocol and client-SDK
  layer, not the Olm primitive. No server can lie about key ownership here: identity is the seed-derived `UK`
  signing the `DeviceList`.
- **Residual risks (ours):**
  1. The glue code around the ratchet is brumm's own and **unaudited**: device certs, `DeviceList` signing and
     `seq` rollback, invite/link one-time keys, fallback-key sessions, own-device-only payload rejection,
     revocation. Matrix's failures were in exactly this layer.
  2. **No post-quantum protection** (no PQXDH-style handshake in Olm): recorded traffic is decryptable by a
     future quantum attacker. Not a v0.1 goal; revisit if the threat model grows.
  3. **Upstream dependency:** vodozemac is maintained by the Matrix team. Keep the `crypto.rs` wrapper thin so the
     library can be swapped.
  4. **Groups:** brumm's own rekey protocol (§14), built from standard parts but itself unaudited; never Megolm.
- **Planned:** review the identity-binding and revocation paths against the attack classes in the Matrix paper
  (impersonation via unauthenticated key sharing, device-list manipulation), then an external audit before any
  claim beyond "private chat for individuals".

---

## 8. Offline delivery via Nostr [M3 mailbox ✅]
Public Nostr relays act as a dumb mailbox. **No own server is needed.** The defaults are `relay.damus.io`, `nos.lol`
and `relay.primal.net` (each device publishes its own list, max 3).

- Each device has an **inbox key**: `secp256k1(blake3::derive_key(ctx, device_secret))`. Its public half and
  the relay list (`MailboxAddress`) are part of the signed `DeviceEntry`, so contacts know where to post.
- **Posting** happens when a direct delivery attempt finds the device unreachable (connect timeout 5 s). The *same* Olm
  ciphertext from the outbox is posted **once** as a letter:
  - kind 1059 (gift wrap), signed by a **fresh random key** per letter
  - tag `p` = recipient inbox key; NIP-40 `expiration` = now + 14 days
  - content = NIP-44(ephemeral → inbox key) of `{ sender device id, sealed }` when that payload is ≤ 24.5 KiB
    (the event then stays under the 64 KiB cap of common relays)
  - a larger payload (≤ 1.5 MiB) goes as up to 64 **letter parts**: each a letter of its own (fresh key, same `p` tag),
    content `"part:" ‖ base64(LetterPart { digest = blake3(payload), index, count, ≤ 24 KiB })`. `:` is not base64,
    so a part never reads as a whole letter. The receiver collects parts in any order (≤ 16 MiB pending, oldest
    dropped first), checks the digest and then handles the payload as one letter, dated by its earliest part.
    Group letters (§14.4) use the same parts. Older clients ignore parts (unreadable letter).

  Message status becomes **Sent** (✔). Direct retries continue meanwhile.
- **Fetching**: each device subscribes to `kind 1059, #p = inbox key, since = last letter − 10 min`
  (stored). Letters are verified and decrypted, then processed exactly like direct frames (same dedupe).
- **Receipts**: a processed letter is answered with `Payload::Receipt([envelope ids])` to the sender
  device through the normal outbox, so directly if possible and otherwise via the sender's mailbox. A receipt counts as the
  ack: the outbox row is dropped, and the message becomes **Delivered** (✔✔). Receipts are never receipted,
  and a receipt row is dropped once mailboxed.
- What relays learn: an inbox key receives letters, plus timing, sizes and the IPs of posters and fetchers. They learn nothing
  about sender identity, contacts or content. Known MVP limits: the inbox key doesn't rotate; a letter from a device
  the recipient doesn't know yet (pending device-list update) is dropped (direct retries remain); relays
  that require NIP-42 auth to read kind 1059 aren't supported yet.

## 9. Files [M4]
- `Body::FileOffer { name, size, blake3_hash, key }`; the file is encrypted with a random key and
  served via **iroh-blobs** (verified streaming).
- **Direct P2P only** (both online); otherwise the offer waits and the transfer resumes when the peer is reachable.
- Size limit configurable (default 2 GiB).

---

## 10. Storage
- SQLite via rusqlite. **No passphrase or unlock screen, ever** (Telegram-like UX).
- At rest today: OS disk encryption, plus Olm pickles encrypted with the pickle key.
- [M5] SQLCipher with a random DB key held in the **OS keychain** (Signal Desktop style), so there is still no prompt.
  Optional app lock later.
- Tables: `meta` (seed phrase, device secret, profile, Olm account, last opened chat), `contacts`, `devices`,
  `olm_sessions`, `messages`, `deleted_messages`, `invites`, `outbox` (ciphertext + plaintext envelope until resealed), … `PRAGMA user_version` = schema version; pre-release
  schemas are rejected with a "delete profile" hint.
- Conversation-scoped rows (`messages`, `reactions`, `image_previews`, `deleted_messages`, `outbox.chat`, meta
  `last_opened`) are keyed by a `ConversationId` stored as `tag byte ‖ 32 bytes` (0 = 1:1 chat or Notes by user key,
  1 = group by `GroupId`), since schema v9. `messages.sender` names a group message's author (NULL in 1:1 chats).
- **Retention**: messages older than 60 days (configurable) are pruned at startup and every 6 h.

---

## 11. Threat model (summary)
| Adversary | Sees | Doesn't see |
|---|---|---|
| iroh relay / n0 DNS | IPs, that device X is online, encrypted traffic volume between two DKs | content, user keys, contact names |
| Nostr relay | IP of fetcher/poster, timing, size, random tags | sender identity, recipient identity, content |
| Contact | your IP when connected directly, your device count | your other contacts |
| Group member | the member list (user keys, devices), gossip neighbours' IPs | your other groups' content, your contacts |
| Holder of your `UK_pub` | your home relay, device count, online-ish (record refresh) | IPs, contacts, content |
| Stolen unlocked device | everything on it, the `UK` (revoke via other device) | — |

Out of scope for v0.1: global passive adversaries, traffic analysis, compromised OS, quantum adversaries
(harvest-now-decrypt-later), national-security-grade assurance. See §7.1.

---

## 12. Milestones
| | Scope | Done when |
|---|---|---|
| **M1** ✅ | identity (seed), invite + QR, iroh direct text with ack, SQLite history, egui chat | two local profiles can add each other and chat |
| **M2** ✅ | vodozemac Olm sessions per device pair, sealed hello/profiles | wire carries only Olm ciphertext (besides acks) |
| **M3** | ✅ Nostr mailbox (offline delivery + receipts) · ⏳ offline invite, contact cards + intro requests | ✅ `mailbox::never_online_together`, `network::offline_delivery_via_public_nostr_relays` |
| **M4** | file offers via iroh-blobs | send a 1 GiB file, resume after disconnect |
| **Multi** ✅ | device lists, linking with contact + history sync, revocation, per-device outbox fan-out | two devices of one user share the conversations |
| **M5** | keychain-backed SQLCipher, retention UI, error states, settings, release builds | installable builds for 3 OSes |

## 13. Roadmap after v0.1
Groups (§14) · stickers · Android/iOS · own mailbox server · Mainline DHT + mDNS discovery ·
BLE / Wi-Fi mesh transport · domain handles (`name@domain`) · relay-only privacy mode.

---

## 14. Groups [G] (#24, built; differences in §14.11)

Groups of up to **10 000 members**, run by their members: no server orders, stores or relays group traffic.
Pairwise Olm fan-out (§6.2) does not scale to this (one text = ~30k seals), and neither does MLS without a
delivery service (§14.9). Groups therefore use their own small protocol built from standard parts
(Ed25519, HPKE RFC 9180, HKDF, AEAD), modeled on server-aided CGKA (SAIK): the device that changes the key does
linear work, and every other device downloads only its own 84-byte slot. 1:1 chats and Notes keep Olm.

### 14.1 Roles and rules
| Role | Can |
|---|---|
| **Owner** (exactly one) | everything an admin can, promote/demote admins, toggle announcement mode |
| **Admin** | add and kick members (not admins or the owner), rename, set the group photo |
| **Member** | post (unless announcement mode is on), react, delete own messages, leave |

- The owner is fixed; inactivity never moves the role. **Succession only on leave:** when the owner leaves, the
  admin promoted earliest becomes owner; without admins, the member who joined earliest. Every device computes this
  from the op log (§14.2), so it needs no extra message.
- **Announcement mode** (only owner and admins post) is a toggle at creation; the owner can change it later.
- Anyone can be added only from the adder's contacts, and only after accepting an invitation (§14.6).
- `MAX_GROUP_MEMBERS = 10_000` users (each with ≤ 8 devices).

### 14.2 Group state: a signed op log
```
GroupOp { group: GroupId, parents: Vec<OpHash>, at_ms, author: UK, kind, sig_UK(domain ‖ all of the above) }
kind: Create { name, announcement } | Add { member: Profile (signed DeviceList), device_keys } | Remove { user }
    | Leave | SetRole { user, Admin | Member } | Rename { name } | SetAnnouncement(bool) | SetAvatar { blob }
GroupId = 32 random bytes · OpHash = blake3(op)
```
- The ops form a DAG (each op names the current heads as `parents`). The state is a **deterministic fold** in
  topological order; concurrent ops are ordered by the author's rank (owner, then admins by promotion time, then
  members), then by hash. Each op is checked against the state its fold position gives it, so e.g. an admin's kick
  that races the owner's demotion of that admin is dropped by every device alike.
- `Leave` is signed by the leaver and needs no admin. Device-list changes are not ops: a member's newer signed
  `DeviceList` (§3.2) is gossiped in the group and accepted from any member.
- Every member device keeps the op log since the latest checkpoint (no message content).
- **Checkpoints:** every `CHECKPOINT_OPS` (1000) ops the owner or an admin signs
  `Checkpoint { group, heads, snapshot: members + roles + settings + member device keys, sig_UK }`. Ops before it may be
  pruned. A newcomer starts from the latest checkpoint plus the ops after it, instead of replaying 10k `Add` ops
  (10–20 MB with their device lists); a 10k-member snapshot is about 4 MB (mostly the device-key table that fixes the
  slot order). A member holding the full log recomputes every checkpoint and rejects (and reports) one that differs;
  a newcomer trusts it as much as it trusts the admin who invited it.

### 14.3 Keys and epochs
- **Device group key:** every device has an X25519 HPKE key pair for groups, published as
  `DeviceKeys { device_id, hpke_pk, issued_at_ms, sig_DK }` (signed by the iroh device key, which the user's
  `DeviceList` certifies). Rotated by publishing a newer record.
- **Epoch:** a random 32-byte `epoch_secret`, bound to one membership version (the op-log heads).
  ```
  Rekey { group, epoch_id, parent_epoch, membership: Vec<OpHash>, blob: blake3 Hash, author_device, sig_DK }
  blob = slot[0] ‖ slot[1] ‖ …   one 84-byte slot per member device, ordered by device id (each device computes its index)
  slot = HPKE-seal(hpke_pk, info = "brumm group epoch v1" ‖ group ‖ epoch_id, epoch_secret)
  ```
  The blob is an iroh-blob; a device fetches only the chunk with its slot, verified against the hash (§14.4).
- **When the key changes:**
  - `Remove`, `Leave` or a revoked device ⇒ **rekey** without them. The op's author rekeys; for a leave or a
    revocation, any member device may (concurrent rekeys are fine, see below). Removals are **batched**: the rekey
    waits `REKEY_BATCH_WINDOW` (30 s) and then covers every removal seen so far, so a busy group does not rekey per
    leave. That window is how long someone who left (or was kicked) can still read.
  - `Add` ⇒ **no rekey**: everyone ratchets `epoch_secret' = HKDF(epoch_secret, "add" ‖ op hash)`, and the newcomer
    gets only `epoch_secret'`, so it cannot read earlier messages.
  - Any member may rekey to heal after a compromise (rate limited, `MIN_REKEY_INTERVAL` per device).
- **Concurrency:** a rekey is only valid for the newest membership a device knows; one made for an older membership
  is never used for sending (so a kicked member never gets a key used after the kick). Two valid rekeys with the same
  parent both decrypt; senders switch to the one with the lower `epoch_id`. Old epochs stay decryptable for
  `EPOCH_GRACE` (7 days), then their secrets are deleted. Nobody gets stuck on a fork.
- **Missed rekeys:** a device that lacks the current epoch asks any member (`KeyRequest`); the member checks the
  op log and wraps the current secret for that device. Recovery never needs an admin.
- **Messages:** per sender device a hash ratchet derived from the epoch:
  `chain₀ = HKDF(epoch_secret, "sender" ‖ device_id)`, `key_n = HKDF(chain_n, "msg")`, `chain_{n+1} = HKDF(chain_n, "chain")`.
  Used keys are deleted (forward secrecy within an epoch); skipped keys are kept up to `MAX_SKIPPED_KEYS`.
  ```
  GroupMessage { group, epoch_id, sender_device, counter, ciphertext = AEAD(key_counter, GroupPayload), sig_DK }
  GroupPayload: Chat(ChatMessage) | Delete | Reaction | File { blob, key, name, size } | …   (append-only enum)
  ```
  The device signature authenticates the sender to every member (like MLS, group messages are not deniable).

### 14.4 Distribution: P2P first
1. **Live: iroh-gossip** (HyParView + PlumTree), one topic per group, `topic = blake3::derive_key("brumm group topic v1", group_id)`.
   Bootstrap from member devices in the op log. A device forwards a message only after checking that the signature
   belongs to a current member device and the sender is within its rate limit, so outsiders cannot inject anything
   and spam is cut at the first hop. Phones and browsers use a smaller active view and do not forward.
2. **Group log, served by members (#15):** every member device keeps the op log, the rekey records and the group's
   message ciphertexts. A device coming back reconciles with a few online members using **range-based set
   reconciliation** (as in Nostr's negentropy) over `(sent_at_ms, message id)`, whose cost grows with what is missing,
   not with the log (week buckets as in §6.4 would ship ~1.6 MB of ids per differing bucket in a busy 10k group). Online
   members are the group's mailbox; they learn nothing they did not already know as members.
   Serving is bounded: ciphertexts are kept for others only for `LOG_SERVE_WINDOW` (14 days, the same as Nostr
   letters) and up to a per-group storage cap; phones and browsers do not serve. Everyone keeps its own decrypted
   history as for 1:1 chats.
3. **Nostr fallback:** a message is posted once (not once per device) when the sender has no gossip neighbour. The
   letter's `p` tag is a secp256k1 key derived from the epoch secret, so it rotates every epoch and relays cannot link
   epochs. Members subscribe to the tags of their live epochs. Letters expire after 14 days.
4. **iroh-blobs** (M4, pulled forward): rekey blobs, files (encrypted once with a random key; key and hash travel in
   the message), group photos, checkpoints and history snapshots. Any member that has a blob serves it.
   **Rekey blobs are replicated:** always-on members (desktop, not phones or browsers) fetch the whole blob
   (2.4 MiB for 30k devices) as soon as the rekey is announced and serve slots from it, so the 30k slot fetches spread
   over the group instead of hitting the device that rekeyed. Files are fetched on view, from any member that has them.

### 14.5 Without admins online
Messaging, catch-up, linking/revoking one's own devices, leaving, owner succession, key recovery and rekeying all
work with no owner or admin online. Only adding, kicking, roles, renaming and settings wait for one. If every admin
lost their keys for good, membership is frozen but the group keeps working. A member who left can read until some
member device rekeys (normally right away).

### 14.6 Joining and history
- The admin sends a contact a `GroupInvitation` over the 1:1 Olm outbox. Accepting it lets the admin sign `Add`; the
  newcomer then gets the op log (as a blob), the post-add `epoch_secret` and bootstrap peers, again over Olm.
- History for new members: **off by default**. A per-group setting (owner/admin) lets the adding admin share the
  last N days as a history blob, shown under "History shared by <admin>" (its authorship rests on the admin's
  signature, not the original senders').

### 14.7 Status and limits
- Status: ✓ once the message reached one gossip neighbour or a relay. No per-member ✓✓ in groups.
- Per-sender rate limit (enforced by forwarders), `MAX_GROUP_MEMBERS`, the member list is virtualized in the UI.
- **Cost model at 10k members (~30k devices).** A message is cheap: ~0.1 ms to seal, ~170 B overhead, every device
  receives it about once (PlumTree spanning tree), ~6–7 gossip hops. But every device receives and stores the whole
  group's traffic: 10 messages per member per day ≈ 50 MB per device per day. Announcement groups and quiet groups are
  cheap; a 10k group where everyone chats all day is a desktop-class load (inherent without a server; a later option
  is phones fetching history on demand instead of storing it). Expensive: rekeying (for the rekeying device, batched
  and replicated as above), joining (checkpoint download ≈ 4 MB), and each group topic's ~5 gossip connections when a
  user is in many large groups.

### 14.8 Privacy
Every member sees the member list (user keys, device ids, display names), which links a user across groups.
Gossip neighbours see each other's IP addresses (browsers: only their iroh relay's). Relays see one letter per
message under rotating tags.

### 14.9 Why not MLS (spike, Nov 2026)
openmls 0.9.2, native, 8 cores; a sparse tree, which is the normal case in large groups:

| Leaves | 1k | 10k | 30k |
|---|---|---|---|
| state per member device | 1.2 MiB | 12 MiB | 36 MiB |
| commit that kicks one member (every device downloads it) | 81 KiB | 802 KiB | 2.35 MiB |
| apply it, per device | 15 ms | 238 ms | 696 ms |
| join (Welcome + tree) | 68 ms | 2.2 s + 3.2 MiB | 26 s + 9.7 MiB |

The rekey blob for 30k devices: 2.8 s of HPKE on one core and a 2.4 MiB blob for the device that rekeys; every other
device fetches 1 KiB chunk + 776 B proof. iroh-gossip (localhost, 100/300/600 nodes, ~5 neighbours each):
100 % delivery, median 11/18/73 ms to reach every node. Revisit MLS if decentralized MLS or "light" clients mature.

### 14.10 Implementation phases
1. **G1** `ConversationId { Direct(UserKey), Group(GroupId) }` through core, API and UI; schema v9 (`messages.sender`).
2. **G2** op log: types, signing, deterministic fold, roles, succession, checkpoints (pure module + tests), invitation flow.
3. **G3** keys: device group keys, epochs, rekey blobs (batched), sender ratchets, key requests. iroh 1.5, iroh-blobs.
4. **G4** distribution: gossip topics, group log + range-based catch-up, rekey-blob replication, Nostr fallback,
   own-device sync of groups.
5. **G5** UI: new group (announcement toggle), members panel, roles, sender names, system lines, files via blobs.
6. **G6** tests: N-node in-process network; kicked device cannot read the next epoch; concurrent ops and rekeys
   converge; catch-up from peers only and from Nostr only; no admin online; owner succession; a 10k-node run of
   iroh-gossip's `simulator` to confirm latency and bandwidth.
   An **external review** of §14 before groups are announced as secure.

### 14.11 As built (Nov 2026)
What the code does where it differs from or adds to §14.1–14.10:
- **No iroh-blobs.** Its file store does not run in the browser, so blobs (large rekey slot sets, group files, the
  group photo) are rows of a SQLite `blobs` table, fetched with `Frame::BlobGet { hash, offset, len }` on `brumm/3`
  from up to 5 member devices. A slot fetch is not verified against the hash (no Bao proof); opening the HPKE slot
  authenticates it. Rekeys with ≤ 16 KiB of slots carry them inline. A slot is 80 bytes.
- **Rekey-blob replication:** desktops fetch a large rekey's whole blob (checked against its hash) instead of their
  slot and serve slots from it; browsers fetch only their slot, and so does a desktop when no member could hand over
  the whole blob. Sources are picked at random among member devices.
- **Browsers** keep a smaller gossip active view (4 instead of 5) and do not serve the group log. They still forward
  (iroh-gossip has no receive-only mode).
- **Gossip forwards before brumm validates.** iroh-gossip has no hook between receive and forward, so the signature,
  member and rate-limit checks of §14.4 item 1 run on receipt, not before forwarding. Invalid units are dropped by every
  receiver, but they do travel the topic; cutting spam at the first hop needs an iroh-gossip change.
- **The group photo** stays servable after the blob table's 14-day retention (it is served from the avatars table),
  and a device that joins or syncs a group fetches it if missing.
- **Units.** Everything on the group channels is a `GroupWire` (`Message | Rekey | KeyRequest | KeyGrant`, ≤ 64 KiB,
  signed by the device). Ops, checkpoints, device lists and `DeviceKeys` are `GroupPayload`s inside epoch messages.
  A key grant is one HPKE slot for the requester, sent as a unit like any other; any member answers after checking that
  the requester's `Add` (or `Create`) is in the ancestry of the epoch's heads, after a random 0–1.5 s, and stays quiet
  if another grant was seen.
- **Epoch choice.** An epoch is *complete* when the ancestry of its heads contains every membership op since the
  checkpoint. Senders use the complete epoch with the lowest id; if none is complete, a rekey is due (the op author
  after `rekey_delay`, anyone else after `2×delay + rand(delay)`). Concurrent `Add`s fork the add-ratchet and resolve
  through that rekey. Rekey recipients are the op state's device keys minus devices no longer in their user's newest
  device list, plus keys learned since (both lists are in the signed `Rekey`).
- **Group peers.** Members who are not contacts get no Olm session: their device lists (`group_peers`) and HPKE
  records (`device_group_keys`) are kept separately, and invitations, replies, welcomes and `GroupSync` are accepted
  only from contacts (welcome, invitation) or own devices (`GroupSync`).
- **Welcome.** Carries the post-add epoch, the op log and, if history sharing is on, at most 7 days / 200 messages /
  12 KiB of history (bigger messages such as files are left out), so that a welcome is always one Nostr letter. The op
  log goes inline up to 12 KiB (a group of a few members); otherwise a **pointer**: the checkpoint hash, up to 8 member
  devices (the inviter's first) and, for a log up to 1 MiB (a few hundred members), a `ChunkSet` naming Nostr letters
  that the inviter posts under a random tag (30 KiB each, on the invitee's relays). The newcomer keeps the welcome in
  `group_pending_logs` and races both paths: members (checkpoint blob in 4 MiB pieces, then the ops after it via
  `GroupLogRequest::Ops`, page by page; a member with a newer checkpoint answers with it) and the chunk letters
  (reassembled, checked against the hash). The first log in which the inviter is an admin and we are a member installs
  the group; otherwise it waits and asks again every catch-up interval. So a big group is joined from any online
  member, and a small one from the relays with nobody online. `GroupSync` to own devices uses the same pointer.
  The UI keeps an accepted invitation listed: "Waiting for Alice to add you…" (cancellable), then "Loading members
  and recent messages…" while the log is fetched.
- **Log and catch-up.** Each device stores every unit it saw in `group_log` (14 days, 256 MiB per group). Catch-up runs
  at start, when the first gossip neighbour appears and every 5 minutes, reconciling `(sent_at_ms, id)` ranges with up
  to 3 members (16-way split, explicit id lists below 32 items), then fetching what is missing, oldest first.
- **Nostr tags.** Messages use a tag from their epoch secret, rekeys one from the parent epoch, control units (key
  requests and grants) one from the group id. Units nobody received are retried when a neighbour appears and posted to
  the relays on quit (§ shutdown drain).
- **At rest.** Epoch secrets and sender chains are sealed with a key derived from the device secret.
- **Scale, measured (Nov 2026).** `group/bench.rs` (release, 8 threads), 10k members × 3 devices:

  | | |
  |---|---|
  | op log of 10k `Add`s | 15.3 MiB |
  | verify every op signature | 5.3 s |
  | fold (order, permissions, state) | 76 ms |
  | folded state in memory | ~25 MiB resident (13.5 MiB encoded) |
  | checkpoint | 13.8 MiB, built in 0.19 s, verified + refolded in 0.25 s |
  | checkpoint + 100 ops, fold | 9 ms |
  | rekey for 30k devices | 1.8 s, 2.29 MiB blob |
  | catch-up after 1 day offline (10 msgs/member/day) | 100k units missing: 3.1 MiB of reconciliation, then ≈ 29 MiB of units |
  | catch-up after 7 days | 700k units: 21 MiB of reconciliation, then ≈ 200 MiB of units |

  **Checkpoints are blobs.** A checkpoint of more than a few hundred members exceeds a unit (64 KiB), so the author
  stores it as a blob (hash = checkpoint hash) and posts `GroupPayload::CheckpointBlob { blob, len }`; members fetch it
  in 4 MiB pieces from the author first, then other members. A member that lacks some covered ops keeps the unit
  waiting (≤ 10 min) until catch-up brings them. Members serve the current checkpoint even after the blob table pruned
  it. An own checkpoint is installed before it is announced, so a failed post does not rebuild it every tick.
  The checkpoint is about 3.5× the estimate in §14.2: the snapshot carries every member's full profile (signed
  device list with certificates and mailbox addresses), not only the device-key table. A newcomer verifies at most the
  ops after the last checkpoint (≤ 1000, ~0.5 s). Reconciliation costs ~32 bytes per missing unit, so it grows with
  what is missing, as intended; the units themselves dominate.
  **Welcome size:** a 10k group's log (~15 MiB) never travels in the welcome: the welcome is a pointer (one Nostr
  letter), and the log comes from members (see *Welcome* above). Posting it as Nostr chunks would take ~480 letters
  per join, so chunks stop at 1 MiB.
- **Picking peers in a big group.** Most of a big group's devices are offline, so random picks (gossip bootstrap 16,
  catch-up 3, blob sources 5) rarely reach an online one: in a 1 000-member test the three online members never heard
  of each other's joins. Each device now keeps when every member device was last seen online (a verified unit from
  it, a request from it, the member that served our log, the inviter's devices on join, the newest 500 log units at
  startup) and picks the most recently seen first, leaving one slot in four to devices not seen yet so new ones are
  found (`net.rs choose_peers`). With no gossip neighbour it asks the topic to join the recently seen devices every
  catch-up interval. Not persisted: rebuilt from the log at startup.
- **1 000-member e2e** (`tests/large_group.rs`): the owner's database gets 1 000 signed `Add`s for generated members
  (offline forever), then 4 real nodes run on localhost (debug build, loaded machine):

  | step | time |
  |---|---|
  | owner restart: checkpoint (556 KiB) + rekey blob for 1 001 devices (78 KiB) | 4.8 s |
  | 3 joins by pointer (checkpoint in 128 KiB pieces + ops from the owner) | 2.9 s + 6.5 s |
  | live chat among the 4 online | 4.0 s |
  | kick + rekey blob for 1 003 devices, the kicked member reads nothing after | 6.3 s |
  | a member back online catches up from the one other online member | 9.5 s |

  Chunk letters stop at the first relay failure (an incomplete set is useless, and every retry waited for the relays
  again, which held shutdown drains for minutes).
- **Gossip simulator** (iroh-gossip `sim`, latency 10–80 ms, bootstrap through a set of peers, `GossipMulti`):

  | nodes | active view | missed | last-delivery hop (mean / max) | round (mean) |
  |---|---|---|---|---|
  | 1 000 | 5 | 0 | 15.9 / 20 | 563 ms |
  | 10 000 | 5 | 0 | 23.2 / 28 | 717 ms |
  | 10 000 | 4 | 0 | 25.2 / 27 | 930 ms |
  | 10 000 | 3 | 4 per round (5 s timeout) | 32.4 / 35 | – |

  Delivery is complete at the default view, but paths are much longer than the 6–7 hops §14.7 assumed (the
  simulator's bootstrap clusters the overlay). A topic where every node keeps only 3 neighbours loses messages; with 4
  nothing was lost, so browsers use 4.

## 15. Headless hosts (#14, built; plan in PLAN-AGENTS.md)
Programs use brumm without the app: a **bot** is a user identity whose only contact is its owner (a 1:1 log
channel), an **agent** is a member of a group (agent room). Each has its own identity, so senders are attributed
and a leaked profile exposes only its own chats. The protocol is unchanged: a bot is a normal user.

- **Core additions.** `SendText` / `SendFile` take an optional caller-chosen `MessageId`; a known id (stored or in
  `deleted_messages`) is a no-op, so a retry never sends twice. Messages carry an **arrival number**
  (`messages.seq`, schema v12, set by an insert trigger from the `meta` counter `message_seq`; never reused, survives
  `wipe`). `LoadNewer { after, limit }` / `LoadLatest { limit }` read by it: a reader with a cursor never misses a
  message that arrives late with an old `sent_at_ms` (mailbox, history sync). `LoadMessage` answers one message;
  since the actor handles commands in order, it also confirms that an earlier command is done.
- **`brumm-host`** (native): `ProfileLock` (one core per profile directory, across processes), an async `Client` over
  `Command` / `Event` (subscribe, then command, then a `LoadMessage` barrier), stable JSON views (base32 ids, rooms
  `d:…` / `g:…`, names resolved without ambiguity) and the policy (`policy.toml`: `locked` refuses invites;
  group invitations from `allow`ed users, default the first contact, are accepted automatically).
- **`brumm-cli`**: one-shot commands (open, act, drain, exit) and a daemon: one per OS user and data root, several
  profiles in one runtime, opened lazily, closed when idle. JSON-RPC 2.0 as NDJSON over a `0600` Unix socket with a
  peer-uid check, no TCP ([protocol.md](protocol.md)). Subscriptions are bounded per connection: a slow reader gets
  `lagged` and rereads from its cursor. Clients auto-start the daemon (a lock file serializes racing starts).
- **Python** (`python/`): a stdlib-only client over the same socket; the wheel bundles the binary.
- Measured: ~10 MB for the daemon, +23 MB for the first profile, +4 MB for each further one (cap 16 by default).
- Limits: invites still need the inviter online (§5, M3); an invite created before the lock can still be accepted.

