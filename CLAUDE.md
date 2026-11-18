# brumm

Decentralized, E2EE, P2P messenger. Desktop app (egui) on top of iroh. The full protocol and
milestone plan is in **SPEC.md**, so read it before changing protocol, storage or identity code.

## Status
- **M1 done:** seed-phrase identity, device certs, single-use invites (link + QR), iroh direct
  text with acks and 30 s retry, SQLite history with 60-day pruning, egui UI.
- **Next: M2:** vodozemac Olm sessions per device pair, SQLCipher with passphrase unlock.
- M1 is **not secure for real use**: the seed phrase and history are stored as plaintext in SQLite, and
  messages are protected only by iroh's QUIC/TLS.

## Layout
```
crates/brumm-core/        no UI; everything async/network/storage
  src/api.rs              Command / Event / *View types, EventSink (the UI boundary)
  src/node.rs             actor loop: sole owner of Store + Transport; spawns network tasks,
                          results come back as Outcome messages (no shared mutable state)
  src/identity.rs         UserIdentity (BIP-39 → Ed25519 via blake3 derive_key), DeviceCert, Profile
  src/invite.rs           Invite ticket <-> brumm://invite/<base32(postcard)>
  src/wire.rs             ALPN brumm/1, Frame / Reply, size limits
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
- New schema changes: bump `SCHEMA_VERSION` in store.rs and add a migration step.
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
bip39 · blake3 · qrcode · jiff. Planned: vodozemac (M2), nostr-sdk (M3), iroh-blobs (M4).
