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
- **Rich messages done:** inline files (`Body::File`, ≤ 10 MiB, one sealed envelope; > mailbox letter limit
  is only delivered while both are online), "delete for everyone" (`Payload::Delete` / own-device `DeleteSync`),
  profile photos (`Payload::Avatar`, ≤ 24 KiB, stored in the `avatars` table, schema v5 upgrades v4 in place),
  emoji reactions (`Payload::Reaction` / own-device `ReactionSync`, one per user and message, last write wins by
  `at_ms`, `reactions` table; schema v6 upgrades v4/v5 in place). Reactions are not part of history sync.
- **Notes chat done:** the conversation keyed by our own `UserKey` (no contacts row; the UI lists a pseudo-contact "Notes").
  Messages are stored as Outgoing/Delivered and synced only via `SentTranscript` / `DeleteSync` / `ReactionSync` to own
  devices (`enqueue_to_user` skips our own key). Exempt from 60-day pruning; included in history sync like any conversation.
- **Next (rest of M3):** offline invites, contact cards plus intro-gated requests.
- **Links:** `http`, `https`, `www.` and `mailto` in a message open with the system handler on click.
  A long note only scans the lines on screen. A single precise click does not select the message.
- **Paste:** Ctrl+V sends clipboard images (`Event::PasteImage`) and copied files (`text/uri-list` via arboard). The latter needs the
  `vendor/egui-winit` patch (Ctrl+V key event passes through when no text/image). Drag & drop does not work on Wayland (winit 0.30).
- **UX rule: no passphrase or unlock screens** (Telegram-like). DB-at-rest encryption will come via an
  OS-keychain-held SQLCipher key (M5), never a prompt.
- The DB is plain SQLite (the seed phrase is readable on disk). Rely on OS disk encryption for now.
- **Window icon / app identity:** the viewport sets `app_id = "brumm"` plus an embedded PNG. Wayland shells ignore a
  client-set icon and match `app_id` → `brumm.desktop` → `Icon=` in the icon theme, so the icon only appears after
  `scripts/install-desktop.sh` installed the entry and the hicolor PNG. That script also rewrites `Exec=` to the
  absolute binary path: the shell runs with the session `PATH` (no `~/.local/bin`), so `Exec=brumm` would not launch.

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
  src/bridge.rs           tokio runtime thread (2 workers) + channels; repaint on event;
                          detached thread, closing the command channel stops the core
  src/state.rs            AppState, mutated only via apply(Event); cached previews + bounded caches
  src/window_size.rs      8-byte window size (`window-size`), read before the window is created
  src/ui/*.rs             one file per screen/widget (onboarding: create/link/restore, devices,
                          revoked, chat, …); screens push Commands, never call core
                          Telegram-Desktop "Night" look: theme.rs (palette, metrics, global style),
                          avatar.rs (initials circles), sidebar.rs (search, chat list sorted by last
                          activity, unread badges), chat.rs (windowed message list, hand-painted bubbles
                          with inline time/ticks, grouping, day pills, pill composer), time_fmt.rs
                          (jiff 1.0 local time)
                          bubble.rs (text/image/file-card bubble + clickable links + selection chrome + context menu),
                          composer.rs (growing one-line pill, emoji, attach), emoji.rs (picker, insert at
                          cursor), files.rs (rfd dialogs on threads, avatar shrink to JPEG, chat image
                          previews, size limits)
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

## Startup & memory
Target: window in ~100 ms, private memory in the tens of MB. Keep it that way.
- **Renderer is glow/OpenGL** (`eframe::Renderer::Glow`, `default-features = false`). Do not
  re-enable wgpu/Vulkan: it pulls the Vulkan ICD + shader compiler into the process for no benefit.
- **`Event::Ready` must never wait for the network.** `Transport::start` binds iroh lazily in the
  background (`OnceCell`); mailbox startup and the outbox flush happen after Ready.
- **Never load every conversation at startup.** `Store::conversation_previews` returns one
  `MessageView` per chat for the sidebar; `Command::LoadConversation` loads a full history only
  when a chat is opened. Bulk changes emit `Event::PreviewChanged` instead of a full reload.
- **Last opened chat** is one `meta` row (`last_opened`, the user key). `Ready` carries that key
  from a single primary-key read and does not load the history. The UI selects the chat if it
  still exists (including Notes) and then sends `LoadConversation`. Opening a different chat
  sends `RememberOpenChat`, which writes the same row. Restoring does not write it again.
- **Window size** is not in SQLite. `window-size` in the profile dir is 8 bytes (width and height,
  little-endian `u32` points). `main` reads it once before the window is created; a missing or
  implausible file falls back to 960×680. A resize is written after it settles, and again on exit.
  Do not turn on eframe's `persistence` feature for this: that also parses the whole egui memory.
- **Window the open chat.** `chat.rs` lays out only the rows near the viewport (plus a
  small overscan). Off-screen rows keep an estimated height and are not shaped into galleys.
  A note longer than 8 KiB shapes only the lines that meet the viewport, so one multi-megabyte
  message does not allocate a text layout per line.
- **Chat image previews.** A photo larger than the bubble (`IMAGE_BUBBLE_EDGE`, 320 logical
  pixels, times the screen scale, capped at 960 physical pixels) is decoded once and re-encoded
  as a preview. The UI keeps that preview only. The original stays in SQLite. Save As reads it
  back on a short-lived thread and drops it after the write. After the decode, `malloc_trim`
  returns the freed bitmap pages to the OS.
- **Chat scroll.** Scrollbar thumbs keep a minimum length (`SCROLLBAR_MIN_LENGTH`) so a long
  history stays grabbable. On the message list, later wheel notches in a burst scroll further,
  and a faster two-finger swipe scrolls further than a slow one. After the gesture ends the list
  glides a short distance and eases to a stop.
- **Bound the caches.** `AppState` keeps only the selected conversation and at most
  `ATTACHMENT_CACHE_ENTRIES` / `ATTACHMENT_CACHE_BYTES` of preview bytes (LRU, `ctx.forget_image`
  on eviction). Large originals are not cached; they stay in the database.
- SQLite runs `WAL` + `synchronous = NORMAL`; the schema batch only runs when `user_version`
  differs. Keep single-row lookups keyed (e.g. `outbox_item`) instead of scanning a list.
- Ship builds with `cargo build --profile dist` (fat LTO, `codegen-units = 1`, `panic = abort`):
  ~30 MB binary and ~19 MB code pages vs ~26 MB for the release profile.

## Commands
```
cargo test                                   # unit + local e2e
cargo test -p brumm-core --test network -- --ignored   # needs internet: relay + DNS discovery
cargo clippy --all-targets
cargo run -- --profile alice                 # separate data dir per profile
cargo run -- --profile bob                   # 2nd instance to chat locally
scripts/install-desktop.sh [binary]          # user-level desktop entry + hicolor icon + ~/.local/bin/brumm
```
Data dir: `~/.local/share/brumm/profiles/<profile>/` (Linux): `brumm.db`, `window-size`.

## Stack
Rust 2024 · iroh 1.4 · eframe/egui 0.37 · tokio · rusqlite (bundled) · postcard · ed25519-dalek 3 ·
bip39 · blake3 · qrcode · jiff. vodozemac 0.11 · nostr-sdk / nostr 0.45 (NIP-44; `local-relay` for tests). Planned: iroh-blobs (M4),
keyring + SQLCipher (M5).
