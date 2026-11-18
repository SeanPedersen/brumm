# brumm

Decentralized, E2EE, P2P messenger. Desktop app (egui) on top of iroh. The full protocol and
milestone plan is in **SPEC.md**, so read it before changing protocol, storage or identity code.

## Status
- **M1 done:** seed-phrase identity, device certs, single-use invites (link + QR), iroh direct
  text with acks and 30 s retry, SQLite history with 60-day pruning, egui UI.
- **M2 done:** vodozemac Olm session per device pair. The invite carries a one-time key, Hello/HelloAck
  carry sealed profiles, and chat payloads are `Frame::Sealed`. Ratchet state is written through to SQLite.
- **Next: M3:** Nostr mailbox for offline delivery, contact cards plus intro-gated requests.
- **UX rule: no passphrase or unlock screens** (Telegram-like). DB-at-rest encryption will come via an
  OS-keychain-held SQLCipher key (M5), never a prompt.
- The DB is plain SQLite (the seed phrase is readable on disk). Rely on OS disk encryption for now.

## Layout
```
crates/brumm-core/        no UI; everything async/network/storage
  src/api.rs              Command / Event / *View types, EventSink (the UI boundary)
  src/node.rs             actor loop: sole owner of Store + Transport; spawns network tasks,
                          results come back as Outcome messages (no shared mutable state)
  src/identity.rs         UserIdentity (BIP-39 → Ed25519 via blake3 derive_key), DeviceCert
                          (binds EndpointId + Olm identity key to the user key), Profile
  src/crypto.rs           thin vodozemac wrapper: OlmDevice, Sealed, seal/open, pickles
  src/e2ee.rs             DeviceCrypto: account + per-device session cache, write-through persistence
  src/invite.rs           Invite ticket <-> brumm://invite/<base32(postcard)>
  src/wire.rs             ALPN brumm/2, Frame / Payload / Reply, size limits
  src/transport.rs        iroh Endpoint + Router (presets::N0), conn cache, InboundHandler
  src/store.rs            rusqlite schema + queries (user_version = schema version)
  tests/two_nodes.rs      e2e: two real nodes invite + chat + invite reuse rejection
crates/brumm-app/         eframe binary `brumm`
  src/bridge.rs           tokio runtime thread + channels; repaint on event
  src/state.rs            AppState, mutated only via apply(Event)
  src/ui/*.rs             one file per screen/widget; screens push Commands, never call core
```

## Conventions
- Core ↔ UI only via `Command`/`Event`. The UI never blocks or awaits.
- The node actor must stay non-blocking: network work goes in `tokio::spawn`, and results return via `Outcome`.
- Wire types are postcard-serialized. Change enums only by appending variants (wire compatibility).
  Bump `INVITE_VERSION` / ALPN on breaking changes.
- New schema changes: bump `SCHEMA_VERSION` in store.rs and add a migration step (pre-release
  schemas are currently just rejected).
- Never persist plaintext Olm state, and always save the session after `seal`/`open`. `e2ee.rs` does this.
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
bip39 · blake3 · qrcode · jiff. vodozemac 0.11. Planned: nostr-sdk (M3), iroh-blobs (M4), keyring + SQLCipher (M5).
