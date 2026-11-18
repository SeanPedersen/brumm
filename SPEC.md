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
│  ├─ crypto      vodozemac Olm sessions per device pair     [M2]  │
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
| **Olm identity** `OK` | Curve25519 + Ed25519 (vodozemac) | per device | [M2] |

`UK = Ed25519::from_bytes(blake3::derive_key("brumm 2026 user identity key v1", bip39_seed))`

### 3.2 Device certificate & device list
```
DeviceCert { user_key: UK_pub, device_id: DK_pub, issued_at_ms, sig_UK(...) }
DeviceList { user_key, seq: u64 (monotonic), devices: Vec<DeviceEntry>, sig_UK(...) }   [later M-multi]
DeviceEntry { device_id, olm_identity_key, olm_fallback_key, added_at_ms }
```
- A peer accepts a device only if its cert/list is validly signed by the contact's `UK`.
- Lists with `seq` ≤ the known `seq` are ignored (no rollback).
- Published (a) as a pkarr record under `UK` (DNS + Mainline DHT, ≤ 1000 bytes ⇒ ~8 devices)
  and (b) pushed to all contacts on change.

### 3.3 Fingerprint
`fingerprint = base32(blake3(UK_pub)[..10])` shown as `ABCD-EFGH-…`. Shown on the profile page and
compared when verifying in person.

### 3.4 Device linking [multi-device milestone]
```
new device:  shows QR / link { DK_new, link_secret(16B) }
old device:  dials DK_new via iroh, authenticates with link_secret
             → transfers UK, contacts, settings, recent history
             → signs DeviceList seq+1, publishes, pushes to contacts
```
Revocation: any device publishes a list without the lost device.
Recovery: fresh install + seed phrase → new `DK`, new list (old devices are dropped).

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
Invite { v: 1, profile: Profile, device_addr: EndpointAddr, secret: [u8;16] }
Profile { user_key, display_name, device_cert }
link = "brumm://invite/" + base32(postcard(Invite))
```
- The inviter stores `secret` as a pending single-use invite.
- The invitee dials `device_addr`, sends `Hello { invite_secret, profile }`.
- The inviter checks that the secret exists and has not been used, that the cert is valid and
  `cert.device_id == connection.remote_id()`, then consumes the secret, stores the contact
  (trust = verified) and replies `HelloAck { profile }`.
- The invitee checks that `HelloAck.profile.user_key == invite.profile.user_key` and stores the contact.
- [M2] Invite and Hello additionally carry Olm identity + one-time keys.
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

### 6.1 Wire frames (`brumm/1`, postcard, max 64 KiB per frame) [M1]
```
Frame::Hello    { invite_secret, profile }           → Reply::HelloAck { profile } | Reply::Rejected
Frame::Message  { id: [u8;16], sent_at_ms, body }    → Reply::Ack { id }           | Reply::Rejected
Body::Text(String)  · Body::FileOffer [M4] · Body::ContactCard [M3] · Body::DeviceList [multi]
```
One request/response per QUIC bi-stream. Connections are cached per peer device.
From M2 `Frame::Message` becomes `Frame::Sealed { olm_message }`, which wraps the same body.

### 6.2 Delivery algorithm
```
for device in contact.devices ∪ my_other_devices:          # fan-out, per-device Olm session [M2]
    try iroh dial + send (timeout 5 s) → Ack ⇒ Delivered
    else                                → Nostr mailbox    ⇒ Queued        [M3]
retry Pending every 30 s (M1, before the mailbox exists)
```
Status shown in UI: `Pending` → `Queued` (stored at the mailbox) → `Delivered` (acknowledged),
or `Failed`.

### 6.3 Ordering & deduplication
- `id` is random; receivers deduplicate by `(sender_user, id)`.
- Messages are shown in `sent_at_ms` order (sender clock); the receive time is stored too.

---

## 7. Encryption [M2]
- **vodozemac Olm** (Double Ratchet) session **per device pair**; each message is encrypted for
  every recipient device and each of my other devices (Sesame-style).
- Session bootstrap: one-time key from the invite/hello; the fallback key from the user record for introductions.
- iroh QUIC/TLS (authenticated by `DK`) is a second, transport-level layer.
- [later] Groups: MLS (openmls) with commit ordering by the group's mailbox, or Megolm-style sender keys.

---

## 8. Offline delivery via Nostr [M3]
- `trait Mailbox { put(tag, ciphertext); fetch(tags, since) -> Vec<..> }`, implemented by `NostrMailbox` (`nostr-sdk`).
- Event = NIP-59-style gift wrap, **signed by a fresh random secp256k1 key per event**.
- Routed by **pairwise, per-device, rotating tags** (`tag = blake3(pair_secret, device, epoch)`),
  never by a real pubkey.
- Written to **3 relays**; NIP-40 expiration = **14 days**; NIP-13 PoW if a relay demands it.
- Delivery is confirmed by our own E2EE `Ack`; mailbox events simply expire.
- Accepted leak (MVP): a relay that serves one device's fetch can link that device's tags (and IP).

---

## 9. Files [M4]
- `Body::FileOffer { name, size, blake3_hash, key }`; the file is encrypted with a random key and
  served via **iroh-blobs** (verified streaming).
- **Direct P2P only** (both online); otherwise the offer waits and the transfer resumes when the peer is reachable.
- Size limit configurable (default 2 GiB).

---

## 10. Storage
- SQLite via rusqlite; **SQLCipher** with a passphrase from M2 (M1 stores plaintext — dev only).
- Tables: `meta`, `contacts`, `devices`, `messages`, `invites`.
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

Out of scope for v0.1: global passive adversaries, traffic analysis, compromised OS.

---

## 12. Milestones
| | Scope | Done when |
|---|---|---|
| **M1** | identity (seed), invite + QR, iroh direct text with ack, SQLite history, egui chat | two local profiles can add each other and chat |
| **M2** | vodozemac Olm sessions, SQLCipher passphrase unlock | wire carries only Olm ciphertext |
| **M3** | Nostr mailbox, offline invite, contact cards + intro requests | message reaches a peer that comes online later |
| **M4** | file offers via iroh-blobs | send a 1 GiB file, resume after disconnect |
| **Multi** | device lists, linking, revocation, own-device fan-out | two devices of one user share the conversations |
| **M5** | retention UI, error states, settings, release builds | installable builds for 3 OSes |

## 13. Roadmap after v0.1
Groups (MLS) · stickers · Android/iOS · own mailbox server · Mainline DHT + mDNS discovery ·
BLE / Wi-Fi mesh transport · domain handles (`name@domain`) · relay-only privacy mode.
