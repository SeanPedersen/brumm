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
Groups, stickers, mobile, global username search, BLE / Wi-Fi mesh, calls, reactions/replies/edits,
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
Sealed { message_type: u8, ciphertext }             # vodozemac OlmMessage::to_parts
```
One request/response per QUIC bi-stream. Connections are cached per peer device.
Acks are plaintext random ids over QUIC/TLS. They become sealed payloads when the mailbox arrives (M3).

### 6.2 Delivery: per-device outbox [Multi ✅]
```
send_text(contact, text):
    for d in devices(contact):   outbox += (d, Envelope{ id = message.id, Chat })        # chat_contact = contact
    for d in my other devices:   outbox += (d, Envelope{ random id, SentTranscript })
each outbox row stores the Olm ciphertext, **sealed once** at enqueue time
flush (on enqueue, after each ack, every 30 s): send rows in queue order, ≤ 2 in flight per device
             # queue order is the priority (history sync); the next row starts when one is acked
Ack{id}                     → drop row; chat rows mark the message Delivered (any device suffices)
Rejected(InvalidMessage)    → drop row; chat → Failed (unless already Delivered)
Rejected(other) / unreachable → keep row, retry   # e.g. contact hasn't processed our new device list yet
unreachable → also post the same ciphertext once to the device's Nostr mailbox (§8) → Sent
              (every row queued for that device, not only the ones in flight)
```
- **Seal once, resend the same bytes.** Re-encrypting on every retry would advance the ratchet without bound while a
  device is offline, beyond Olm's skipped-message-key window.
- The receiver keeps `blake3(ciphertext) → envelope id` for processed envelopes and **re-acks duplicates**
  (a retry whose ack was lost) instead of failing to decrypt. Pruned with message retention.
- Status shown in the UI: `Pending` 🕓 → `Sent` ✔ (in the mailbox) → `Delivered` ✔✔ (acknowledged by at least one
  device of the contact), or `Failed`.

### 6.3 Ordering & deduplication
- `id` is random; receivers deduplicate by `(sender_user, id)`.
- Messages are shown in `sent_at_ms` order (sender clock); the receive time is stored too.

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
- [later] Groups: MLS (openmls) with commit ordering by the group's mailbox. **Not Megolm** (see §7.1).

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
  4. **Groups:** use MLS (openmls), never Megolm. MLS tooling is still maturing (see arewemlsyet.com).
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
  - content = NIP-44(ephemeral → inbox key) of `{ sender device id, sealed }`, max 32 KiB sealed

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
  `olm_sessions`, `messages`, `invites`. `PRAGMA user_version` = schema version; pre-release
  schemas are rejected with a "delete profile" hint.
- **Retention**: messages older than 60 days (configurable) are pruned at startup and every 6 h.

---

## 11. Threat model (summary)
| Adversary | Sees | Doesn't see |
|---|---|---|
| iroh relay / n0 DNS | IPs, that device X is online, encrypted traffic volume between two DKs | content, user keys, contact names |
| Nostr relay | IP of fetcher/poster, timing, size, random tags | sender identity, recipient identity, content |
| Contact | your IP when connected directly, your device count | your other contacts |
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
Groups (MLS) · stickers · Android/iOS · own mailbox server · Mainline DHT + mDNS discovery ·
BLE / Wi-Fi mesh transport · domain handles (`name@domain`) · relay-only privacy mode.
