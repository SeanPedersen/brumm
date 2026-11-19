# brumm

Decentralized, E2EE, P2P messenger. Desktop app (egui) on top of iroh. The full protocol and
milestone plan is in **SPEC.md**, so read it before changing protocol, storage or identity code.

## Status
- **M1 done:** seed-phrase identity, device certs, single-use invites (link + QR), iroh direct
  text with acks and 30 s retry, SQLite history with 60-day pruning, egui UI.
- **M2 done:** vodozemac Olm session per device pair. The invite carries a one-time key, Hello/HelloAck
  carry sealed profiles, and chat payloads are `Frame::Sealed`. Ratchet state is written through to SQLite.
- **Multi-device done:** signed `DeviceList` (seq, ≤ 8 devices, fallback keys), linking via a
  `brumm://link/` ticket with contact + history sync, removal/revocation, a per-device outbox
  (sealed once, resent as-is), several Olm sessions per device, and re-acking duplicate ciphertexts.
- **M3 mailbox done:** offline delivery via **public Nostr relays** (no own server). Unreachable
  device → the outbox ciphertext is posted once as a NIP-44 letter to its inbox key → status Sent ✔ →
  the recipient answers with `Payload::Receipt` → Delivered ✔✔.
- **Rich messages done:** inline files (`Body::File`, ≤ 1 MiB, one sealed envelope; > mailbox letter limit
  is only delivered while both are online), "delete for everyone" (`Payload::Delete` / own-device `DeleteSync`),
  profile photos (`Payload::Avatar`, ≤ 24 KiB, stored in the `avatars` table, schema v5 upgrades v4 in place),
  emoji reactions (`Payload::Reaction` / own-device `ReactionSync`, one per user and message, last write wins by
  `at_ms`, `reactions` table; schema v6 upgrades v4/v5 in place). Reactions are not part of history sync.
- **Next (rest of M3):** offline invites, contact cards plus intro-gated requests.
- **UX rule: no passphrase or unlock screens** (Telegram-like). DB-at-rest encryption will come via an
  OS-keychain-held SQLCipher key (M5), never a prompt.
- The DB is plain SQLite (the seed phrase is readable on disk). Rely on OS disk encryption for now.

## Layout
```
crates/brumm-core/        no UI; everything async/network/storage
  src/api.rs              Command / Event / *View types, EventSink (the UI boundary)
  src/node/mod.rs         actor loop: sole owner of Store + DeviceCrypto + Transport; spawns network
                          tasks, results come back as Outcome messages (no shared mutable state)
  src/node/commands.rs    identity creation, invites, send_text
  src/node/contacts.rs    invite handshake (both sides), contact sync to own devices
  src/node/linking.rs     link / remove devices, apply own DeviceList, revocation
  src/node/delivery.rs    per-device outbox: enqueue (seal once), flush, acks, mailbox fallback, receipts
  src/node/inbound.rs     decrypt + dedupe + apply Envelope payloads
  src/identity.rs         UserIdentity (BIP-39 → Ed25519 via blake3 derive_key), DeviceCert,
                          DeviceEntry, signed DeviceList, Profile {display_name, devices}
  src/crypto.rs           thin vodozemac wrapper: OlmDevice, Sealed, seal/open, pickles
  src/e2ee.rs             DeviceCrypto: account, ≤ 4 sessions per device, lazy fallback-key sessions,
                          write-through persistence
  src/link.rs             LinkTicket (brumm://link/) + LinkBundle
  src/mailbox.rs          Nostr mailbox: inbox keys, letters (kind 1059 + NIP-44), post/subscribe
  src/invite.rs           Invite ticket <-> brumm://invite/<base32(postcard)>
  src/wire.rs             ALPN brumm/3, Frame / Envelope / Payload / Reply, URI + history chunking
  src/transport.rs        iroh Endpoint + Router (presets::N0), conn cache, InboundHandler
  src/store.rs            rusqlite schema + queries (user_version = schema version)
  tests/common/mod.rs     TestNode harness (buffers unmatched events; peers deliver in any order)
  tests/two_nodes.rs      e2e: invite + chat + invite reuse rejection
  tests/multi_device.rs   e2e: link, contact/history sync, fan-out, transcripts, revocation
  tests/mailbox.rs        e2e via in-memory Nostr relay: never online together, receipts
  tests/network.rs        ignored (needs internet): relay-only, DNS rediscovery, offline retry, public Nostr relays
crates/brumm-app/         eframe binary `brumm`
  src/bridge.rs           tokio runtime thread + channels; repaint on event
  src/state.rs            AppState, mutated only via apply(Event)
  src/ui/*.rs             one file per screen/widget (onboarding: create/link/restore, devices,
                          revoked, chat, …); screens push Commands, never call core
                          Telegram-Desktop "Night" look: theme.rs (palette, metrics, global style),
                          avatar.rs (initials circles), sidebar.rs (search, chat list sorted by last
                          activity, unread badges), chat.rs (hand-painted bubbles with inline time/ticks,
                          grouping, day pills, pill composer), time_fmt.rs (jiff 1.0 local time)
                          bubble.rs (text/image/file-card bubble + selection chrome + context menu),
                          composer.rs (growing one-line pill, emoji, attach), emoji.rs (picker, insert at
                          cursor), files.rs (rfd dialogs on threads, avatar shrink to JPEG, size limits)
```

## Conventions
- Core ↔ UI only via `Command`/`Event`. The UI never blocks or awaits.
- The node actor must stay non-blocking: network work goes in `tokio::spawn`, and results return via `Outcome`.
- Wire types are postcard-serialized. Change enums only by appending variants (wire compatibility).
  Bump `INVITE_VERSION` / ALPN on breaking changes.
- New schema changes: bump `SCHEMA_VERSION` in store.rs and add a migration step (pre-release
  schemas are currently just rejected).
- Never persist plaintext Olm state, and always save the session after `seal`/`open`. `e2ee.rs` does this.
- Never re-encrypt an outbox entry on retry: always resend the stored ciphertext (ratchet gap limit).
- Own-device-only payloads (`SentTranscript`, `ContactSync`, `History`) must be rejected from contacts.
- Names for constants, no magic numbers. Module doc comment at the top of every file.

## Commands
```
cargo test                                   # unit + local e2e
cargo test -p brumm-core --test network -- --ignored   # needs internet: relay + DNS discovery
cargo clippy --all-targets
cargo run -- --profile alice                 # separate data dir per profile
cargo run -- --profile bob                   # 2nd instance to chat locally
```
Data dir: `~/.local/share/brumm/profiles/<profile>/brumm.db` (Linux).

## Stack
Rust 2024 · iroh 1.4 · eframe/egui 0.37 · tokio · rusqlite (bundled) · postcard · ed25519-dalek 3 ·
bip39 · blake3 · qrcode · jiff. vodozemac 0.11 · nostr-sdk / nostr 0.45 (NIP-44; `local-relay` for tests). Planned: iroh-blobs (M4),
keyring + SQLCipher (M5).
