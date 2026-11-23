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
- **Consistency between own devices:** receivers answer `Undecryptable` (no session opens it) or `TryLater` (local
  error) instead of the final `InvalidMessage`, so a sender never drops an envelope for a problem on the receiver's side.
  On `Undecryptable` the outbox entry is resealed **once** on a fresh session from the fallback key (schema v8 keeps the
  plaintext envelope in `outbox.envelope` until then). Own devices run an anti-entropy round (`reconcile.rs` pure
  compare, `node/reconcile.rs` triggers): digest of message ids per chat and week → ids of differing buckets → missing
  messages (`SyncMessages`) and lost deletes (`DeleteSync`) via the outbox. Rounds start at startup, on direct contact
  and every `reconcile_interval` (5 min) while the device was seen recently, never while its outbox is non-empty.
  Deleted ids are kept in `deleted_messages` and never re-inserted (also when a delete overtakes its message).
  The UI inserts a live `MessageAdded` at its `sent_at_ms` position (`state.rs insert_in_order`), not at the end.
- **Log out** (☰ menu, and "Erase this device" on the removed screen): `Command::LogOut` signs a device list without
  this device (any device holds the seed) and announces it to own devices + contacts, waits until no delivery/mailbox post
  is in flight (max 20 s), then `Store::wipe` (every row, `secure_delete` + `VACUUM`) and a fresh node → `Event::LoggedOut`
  → onboarding. Same path on desktop and in the browser (OPFS). Commands are refused while logging out.
- **M3 mailbox done:** offline delivery via **public Nostr relays** (no own server). Unreachable
  device → the outbox ciphertext is posted once as a NIP-44 letter to its inbox key → status Sent ✔ →
  the recipient answers with `Payload::Receipt` → Delivered ✔✔.
- **Rich messages done:** inline files (`Body::File`, ≤ 10 MiB, one sealed envelope; > mailbox letter limit
  is only delivered while both are online), "delete for everyone" (`Payload::Delete` / own-device `DeleteSync`),
  profile photos (`Payload::Avatar`, ≤ 24 KiB, stored in the `avatars` table, schema v5 upgrades v4 in place),
  emoji reactions (`Payload::Reaction` / own-device `ReactionSync`, one per user and message, last write wins by
  `at_ms`, `reactions` table; schema v6 upgrades v4/v5 in place). Reactions are not part of history sync.
- **Notes chat done:** the conversation keyed by our own `UserKey` (no contacts row; the UI lists a pseudo-contact "Notes").
  Messages are stored as Outgoing and tick like any chat (🕓 drawn as `icons::clock` → ✓ on the relay → ✓✓ when another own device
  has it; Delivered at once while this is the only device). The `SentTranscript` is enqueued under the message id with the Notes chat as
  `chat` (`enqueue_to_own_devices_as`), so acks and relay posts update the status. Contact messages' transcripts stay untracked. Synced only via `SentTranscript` / `DeleteSync` / `ReactionSync` to own
  devices (`enqueue_to_user` skips our own key). Exempt from 60-day pruning, and history sync to a new device sends it in full (other chats: last 60 days).
  Linking sends a `HistoryManifest` first; history chunks go one chat at a time, most recent chat first, newest messages
  first. The outbox keeps queue order (≤ 2 in flight per device, next on ack). The new device shows a per-chat
  "Syncing history… n of m" hint (`ui/sync_hint.rs`, progress persisted in `meta`).
  Request timeouts grow with frame size (`MIN_TRANSFER_BYTES_PER_SEC`): a multi-MB file over an iroh relay (always the case for
  the browser) needs far more than the 10 s base.
- **Browser build (#2, in progress):** the same egui UI on a canvas (`eframe::WebRunner`, built with
  trunk). brumm-core runs in a dedicated Web Worker (`crates/brumm-worker`), because SQLite's OPFS VFS
  (`sqlite-wasm-vfs` sahpool) needs sync access handles. UI ↔ worker: postcard `api::ToWorker` /
  `api::FromWorker` over `postMessage`; commands queue until the worker reports `Listening`.
  `?profile=<name>` picks the identity, and each profile has its own OPFS directory. iroh is relay-only
  in the browser. Relays are iroh's n0 relays **without the DNS root dot** (`transport::relay_map`):
  WebKit (every iOS browser) fails `fetch`/WebSocket to `….iroh.link.`, so with iroh's default names no
  relay is ever chosen. Peer addresses are normalized the same way before dialing. A ticket (link or
  invite) is only issued with a relay address (`Transport::addr` fails after 30 s instead). File picking uses a hidden `<input type=file>` (`web_files.rs`, bytes as `Picked::Dropped` /
  `AvatarData`); Save As has no dialog: the worker answers `SaveAttachment` with `Event::FileExport`
  and the UI starts a Blob download into the browser's download folder. Not on the web yet:
  clipboard file paste, system color emoji. Core code uses `n0_future::{task::spawn, time}` (tokio natively,
  setTimeout/spawn_local in wasm), never `tokio::spawn` / `tokio::time` / `std::time::Instant`.
- **Responsive layout:** below `NARROW_BREAKPOINT` (600 logical px, width-based, so a narrowed desktop window too) only one pane
  shows, Telegram style (`ui/narrow.rs`): the chat list is home (also after a restart, `last_opened` only preselects), a tap opens
  the chat full-screen with a ← back arrow in its header. In-chat search shows its results in the list pane; picking a hit returns to the chat.
  In the browser, opening a chat pushes a history entry and `popstate` closes it (`history.rs`), so the back gesture works; the arrow calls
  `history.back()`. Desktop has no history and closes directly. `index.html` uses `viewport-fit=cover`, `100dvh` and safe-area padding.
  The composer has a round send button (desktop and phone); on touch devices (`touch.rs`, `(pointer: coarse)`) Enter inserts a newline
  and only the button sends. On the web the send button is backed by a transparent DOM overlay (`touch.rs`) that swallows the press, because a tap on the canvas blurs the hidden text input and closes the on-screen keyboard. On the web the body is sized to `visualViewport`, so the on-screen keyboard pushes the chat up (iOS overlays it otherwise).
  Touch selection: a long tap opens the context menu (Select item), never selects by itself. Moving the finger (> `TOUCH_MOVE_SLOP`) is
  scrolling and drops the press, drag-selection is off on touch. Once selection mode is on, a tap toggles.
  Not yet phone-adapted: dialogs/devices/onboarding screens, touch-sized hit targets.
- **Quit right after sending:** the Nostr post normally starts only after the direct attempt failed (5-10 s), so a quit
  in that window used to leave the message in the outbox until the next launch. On shutdown `node/drain.rs` posts every
  unmailboxed outbox entry to the relays at once (cap `SHUTDOWN_DRAIN_MAX`, 10 s), and `main` waits for it in `bridge::wait_for_drain` after
  the window is gone (`NodeConfig::on_drained` fires after the drain, not after the slow graceful network teardown, so closing
  the window is instant; the process used to kill the core mid-drain). `bridge/profile_lock.rs` (`brumm.lock`, held for the core's whole life
  incl. the drain) makes a relaunch wait for it and refuses a second live instance: two processes on one DB would
  diverge the Olm ratchets. A browser tab cannot drain, so on wasm `enqueue_to_own_devices` also posts to the mailbox
  at enqueue time (not on native: no extra relay traffic). Tests: `tests/mailbox.rs` (quit right after send, Notes via relay).
- **Next (rest of M3):** offline invites, contact cards plus intro-gated requests.
- **Groups (#24): G1–G6 done** (only the manual browser run is open). SPEC.md §14 (design) and §14.11 (what was
  built, how it differs, scale numbers). Plan with checkboxes: **PLAN-GROUPS.md**; review brief: **REVIEW-GROUPS.md**. Up to 10k members, owner/admin/member, signed op
  log, HPKE rekeys (not MLS, see §14.9). Every chat is keyed by `ConversationId { Direct(UserKey), Group(GroupId) }`
  (`conversation.rs`, stored as `tag byte ‖ 32 bytes` since schema v9; schema v10 adds the group tables in
  `store/groups.rs`). Core split: pure protocol in `group/` (no I/O, unit tested), node glue in `node/groups/`.
  - **Op log** (`group/oplog.rs`): DAG of signed `GroupOp`s, deterministic fold (rank, then hash), each op checked
    against the state at its fold position; owner succession on leave; checkpoints every 1000 ops (`checkpoint.rs`), sent as blobs
    (`GroupPayload::CheckpointBlob`: 13.8 MiB at 10k members, far over a 64 KiB unit).
    Ops travel as `GroupPayload::Op` inside epoch messages and inside the welcome / `GroupSync`.
  - **Epochs** (`group/epochs.rs`, `group/keys.rs`): a `Rekey` roots an epoch; an `Add` posted in epoch E derives a
    child (`ratchet_add`), so newcomers never read the past. Sending epoch = the complete epoch (its heads cover every
    membership op) with the lowest id; none complete → rekey due. Rekeys wait `rekey_delay` (author) or
    `2×delay + rand(delay)` (others), batching removals. Slots are inline when small (≤ 16 KiB), else a blob fetched
    by byte range. Missed epochs: `KeyRequest` → a member grants after an op-log check (`KeyGrant`, jittered,
    cancelled when someone else answered). Epoch secrets and sender chains are sealed at rest with a key derived from
    the device secret.
  - **Distribution** (`node/groups/net.rs`): iroh-gossip topic per group (verify signature + member device + rate limit
    before storing; gossip has no pre-forward hook), a member-served log (`group_log`, 14 days, 256 MiB cap) with
    range-based reconciliation (`group/sync.rs`, `Frame::GroupLog`) from ≤ 3 members, blobs (`blobs` table,
    `Frame::BlobGet`, own SQLite store instead of iroh-blobs: wasm; desktops replicate whole rekey blobs and serve
    slots, `BlobPurpose::Replica`), and Nostr letters (`mailbox.rs post_group`, `p` tag
    from the epoch secret, the parent epoch for rekeys, the group id for control units) when no neighbour was reached.
    Unpublished units are retried when a neighbour appears and drained to the relays on quit. Peers (gossip bootstrap,
    catch-up, blob and log sources) are picked by when each device was last seen online (`GroupRuntime::seen`,
    `choose_peers`), with a share left for unseen devices: random picks miss the few online devices of a big group. Browsers do not serve
    and keep 4 gossip neighbours (`transport::gossip_membership`). iroh-gossip forwards before we validate.
  - **Joining** (`node/groups/join.rs`): invitation, reply and welcome go over 1:1 Olm to contacts only; the welcome
    carries the post-add epoch, ≤ 7 days / 200 messages / 12 KiB of history (one Nostr letter) and the op log: inline
    up to 12 KiB, else a `LogPointer` (`node/groups/pending.rs`): checkpoint hash + member devices to fetch from
    (`GroupLogRequest::Ops`, blobs in pieces), plus Nostr chunk letters for logs ≤ 1 MiB. The newcomer waits in
    `group_pending_logs` (schema v11) and takes whichever arrives first. An accepted invitation stays listed
    (`InvitationStage`: Accepted → Loading) until the group is installed. Own devices get `GroupSync` (log + epochs,
    same pointer), also when linked. Non-contact members have no Olm session: their device lists live in
    `group_peers`, their HPKE records in `device_group_keys`.
  - **UI:** groups and invitations in the chat list, "New group" (☰ menu, `ui/new_group.rs`), header click → "Group
    info" (`ui/group_info.rs`: paged member search via `LoadMembers`, roles, settings, invites, key refresh, leave),
    sender names and system lines (`ui/group_rows.rs`; a system line is a message row with an empty `sender`), reaction
    counts (`ReactionsView.others`), composer replaced by a notice in announcement mode / after leaving, group photo
    (`files::AvatarTarget`). Dialogs use `theme::dialog` (fits a phone screen); the member list lays out visible rows
    only and pages from the core. Groups tick ✓ only.
  - Tests: `tests/groups.rs` (kick, succession, catch-up from members, Nostr only, own device, announcement + files +
    reactions, photo for a later joiner, 20 members + no admin online, blob rekey replication, checkpoints as blobs
    fetched in pieces, joining from a member / from relay chunks while the inviter is offline); `tests/large_group.rs`:
    1 000 generated members (signed Adds written into the stopped owner's DB) + 4 real nodes; timings and size limits
    via `NodeConfig::group_timing` (`TestNode::spawn_tuned`, `spawn_tuned_with`). Scale bench: `group/bench.rs` (ignored,
    run in release). Real network: `tests/network.rs group_over_relays_and_public_nostr` (ignored).
- **Emoji:** the system color emoji font (Noto Color Emoji, via `fc-match emoji` if it is not
  in a well-known path) is installed after the text fonts and before discovered fallbacks.
  Sans fonts that also contain emoji outlines (DejaVu Sans, Noto Sans Symbols 2) would otherwise
  paint those codepoints grey. Letters and digits stay with the text font.
- **Links:** `http`, `https`, `www.` and `mailto` in a message open with the system handler on click.
  A long note only scans the lines on screen. A single precise click does not select the message.
- **In-chat search:** the magnifying glass at the right of the chat header (also Ctrl+F) switches the
  sidebar to that room. The sidebar field searches message text and file names; the list shows the
  newest 100 matches. A click scrolls the room to that message and tints it until the user scrolls.
  Esc, the header icon, or the sidebar ✕ leaves search and restores the chat list (the contact
  filter text is kept). Matching runs again only when the query or the conversation changes.
- **Jump to latest:** when the open room is scrolled up, a down-chevron button overlays the bottom
  right of the message list (just left of the scrollbar) and scrolls back to the newest message,
  sticking there again.
- **Paste:** Ctrl+V sends clipboard images (`Event::PasteImage`) and copied files (`text/uri-list` via arboard). The latter needs the
  `vendor/egui-winit` patch (Ctrl+V key event passes through when no text/image). Drag & drop does not work on Wayland (winit 0.30).
- **UX rule: no passphrase or unlock screens** (Telegram-like). DB-at-rest encryption will come via an
  OS-keychain-held SQLCipher key (M5), never a prompt.
- The DB is plain SQLite (the seed phrase is readable on disk). Rely on OS disk encryption for now.
- **Window buttons:** on Wayland the frame is client-drawn. `brumm-app` enables winit's
  `wayland-csd-adwaita` (the same feature ZapFast uses) so minimize, maximize and close are
  Adwaita buttons in the desktop's `button-layout`, themed dark with the UI. X11 keeps the
  window manager's frame.
- **Window icon / app identity:** the viewport sets `app_id = "brumm"` plus an embedded PNG. Wayland shells ignore a
  client-set icon and match `app_id` → `brumm.desktop` → `Icon=` in the icon theme, so the icon only appears after
  `scripts/install-desktop.sh` installed the entry and the hicolor PNG. That script also rewrites `Exec=` to the
  absolute binary path: the shell runs with the session `PATH` (no `~/.local/bin`), so `Exec=brumm` would not launch.

## Layout
```
crates/brumm-core/        no UI; everything async/network/storage
  src/api.rs              Command / Event / *View types, EventSink (the UI boundary)
  src/conversation.rs     ConversationId { Direct(UserKey), Group(GroupId) } + its stored form (tag ‖ key)
  src/node/mod.rs         actor loop: sole owner of Store + DeviceCrypto + Transport; spawns network
                          tasks, results come back as Outcome messages (no shared mutable state)
  src/node/commands.rs    identity creation, invites, send_text
  src/node/contacts.rs    invite handshake (both sides), contact sync to own devices
  src/node/linking.rs     link / remove devices, apply own DeviceList, revocation
  src/node/delivery.rs    per-device outbox: enqueue (seal once), flush, acks, mailbox fallback, receipts
  src/node/inbound.rs     decrypt + dedupe + apply Envelope payloads
  src/node/drain.rs       shutdown: post the whole outbox to the Nostr relays, capped wait
  src/node/reconcile.rs   own-device consistency rounds: when to run, apply digest / have / missing messages
  src/node/groups/        group glue: mod (runtime, tick), ops, keys (epochs, rekeys, grants), messages, pending (op
                          logs by pointer: member fetch + Nostr chunks),
                          net (gossip, publish, catch-up, blobs), join (invitations, welcome, GroupSync)
  src/group/              pure group protocol: oplog, checkpoint, epochs, keys (HPKE slots, chains), message
                          (GroupWire units), join (invitation/welcome types), sync (range reconciliation)
  src/reconcile.rs        pure bucket digest + compare (no I/O)
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
  src/store/mod.rs        rusqlite schema + queries (user_version = schema version)
  src/store/groups.rs     group tables: ops, members, epochs, chains, log, blobs, invitations, pending logs, peers
  tests/common/mod.rs     TestNode harness (buffers unmatched events; peers deliver in any order)
  tests/two_nodes.rs      e2e: invite + chat + invite reuse rejection
  tests/multi_device.rs   e2e: link, contact/history sync, fan-out, transcripts, revocation
  tests/mailbox.rs        e2e via in-memory Nostr relay: never online together, receipts
  tests/groups.rs         e2e: groups (kick, succession, catch-up, Nostr only, own devices, announcement)
  tests/large_group.rs    e2e: 1 000 generated members + 4 real nodes (pointer joins, blobs at size, peer ranking)
  tests/consistency.rs    e2e: lost envelopes/deletes/sessions repaired, mailbox notes once + in order
                          (simulates losses by editing a stopped node's DB, `Stopped::sql`)
  tests/network.rs        ignored (needs internet): relay-only, DNS rediscovery, offline retry, public Nostr relays
crates/brumm-worker/      browser only: runs brumm-core in a Web Worker (OPFS storage, message pump)
crates/brumm-app/         eframe binary `brumm` (desktop + browser); index.html + Trunk.toml for the web
  src/main.rs             picks native.rs (window, --profile, window size) or web.rs (canvas, ?profile=)
  src/bridge/native.rs    tokio runtime thread (2 workers) + channels; repaint on event;
                          closing the command channel stops the core, Drop waits for its drain
  src/bridge/profile_lock.rs  one core per profile dir (file lock)
  src/bridge/web.rs       starts brumm-worker, postcard messages, queues commands until Listening
  src/history.rs          browser history push/back/popstate for the narrow layout (no-op on desktop)
  src/state.rs            AppState, mutated only via apply(Event); cached previews + bounded caches
  src/preview.rs          background thread: scale a chat photo, then the UI stores the preview
  src/window_size.rs      8-byte window size (`window-size`), read before the window is created
  src/ui/*.rs             one file per screen/widget (onboarding: create/link/restore, devices,
                          revoked, chat, …); screens push Commands, never call core
                          narrow.rs (single-pane phone navigation state), main_screen.rs picks the layout.
                          Telegram-Desktop "Night" look: theme.rs (palette, metrics, global style),
                          avatar.rs (initials circles), sidebar.rs (contact search, or the open room's
                          message hits while in-chat search is on; chat list sorted by last
                          activity, unread badges), chat.rs (windowed message list, hand-painted bubbles
                          with inline time/ticks, grouping, day pills, pill composer, jump-to-latest),
                          chat_search.rs (query, snippets, jump target), time_fmt.rs
                          (jiff 1.0 local time)
                          bubble.rs (text/image/file-card bubble + clickable links + selection chrome + context menu),
                          composer.rs (growing one-line pill, emoji, attach), emoji.rs (picker, insert at
                          cursor), emoji_font.rs (system color emoji ahead of monochrome symbol fonts),
                          files.rs (rfd dialogs on threads, avatar shrink to JPEG, chat image
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
- Never re-encrypt an outbox entry on retry: always resend the stored ciphertext (ratchet gap limit). The only
  exception is the single reseal after the receiver answered `Undecryptable`.
- Only answer `InvalidMessage` for a payload we refuse; it makes the sender drop the envelope for good.
- Own-device-only payloads (`SentTranscript`, `ContactSync`, `History`) must be rejected from contacts.
- Names for constants, no magic numbers. Module doc comment at the top of every file.

## Startup & memory
Target: window in ~100 ms, private memory in the tens of MB. Keep it that way.
- **Renderer is glow/OpenGL** (`eframe::Renderer::Glow`, `default-features = false`). Do not
  re-enable wgpu/Vulkan: it pulls the Vulkan ICD + shader compiler into the process for no benefit.
- **`Event::Ready` must never wait for the network.** `Transport::start` binds iroh lazily in the
  background (`OnceCell`); mailbox startup and the outbox flush happen after Ready.
- **Never load every conversation at startup.** `Store::conversation_previews` returns one
  `MessageView` per chat for the sidebar. Opening a chat loads only its newest page
  (`LoadConversation { limit }`, `CONVERSATION_PAGE` = 50, keyset `conversation_page`). File bodies
  are read only up to their head (name + size), never the data. Older pages come with `LoadOlder`
  once the view is within `LOAD_OLDER_MARGIN` of the top (all of them while in-chat search is on).
  A reload after a bulk change asks for as many messages as are already loaded.
  No visual shift: when rows appear above the view (older page) or rows above the message being
  read get measured, `chat.rs` moves the scroll offset by the same height in the same frame (with
  rubber-banding off for that frame, since egui's physics still has the old bounds). Bulk changes emit `Event::PreviewChanged` instead of a full reload.
- **Last opened chat** is one `meta` row (`last_opened`, the stored `ConversationId`). `Ready` carries that key
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
  pixels, times the screen scale, capped at 960 physical pixels) is decoded on a background
  thread (`preview.rs`) and re-encoded as a preview. JPEG uses a scaled IDCT (1/2, 1/4, 1/8),
  so a 4K photo is not decoded at full size and the UI thread keeps scrolling. The preview is
  stored in `image_previews` (schema v7) and reused on the next open. The UI keeps that preview
  only. The original stays in SQLite. Save As reads it back on a short-lived thread and drops it
  after the write. After a large decode, `malloc_trim` on the preview thread returns the freed
  bitmap pages to the OS.
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
cargo clippy -p brumm-app -p brumm-worker --target wasm32-unknown-unknown
(cd crates/brumm-app && trunk serve)         # browser build: http://localhost:8080/?profile=alice
cargo run -- --profile alice                 # separate data dir per profile
cargo run -- --profile bob                   # 2nd instance to chat locally
scripts/install-desktop.sh [binary]          # user-level desktop entry + hicolor icon + ~/.local/bin/brumm
deploy/deploy.sh                             # web client: trunk --release, rsync, docker compose on the host
```
**Web deploy** (`deploy/`): a static Caddy container (`docker-compose.yml`, `Caddyfile`) joins an
existing Docker network behind a TLS-terminating proxy/tunnel; no host port. Host, directory,
network and URL live only in the gitignored `deploy/.env` (template: `.env.example`). The repo
is public: never commit host names, paths or keys. Caching: trunk-hashed assets are immutable,
`index.html` and the fixed-name worker files are revalidated (`no-store` for the worker `.js`,
since Cloudflare rewrites `no-cache` on `.js`), so a new UI never meets an old worker.
`index.html` passes Rust's default wasm features to `wasm-opt` (`data-wasm-opt-params`);
without them it rejects the bulk-memory ops that current rustc emits.
Data dir: `~/.local/share/brumm/profiles/<profile>/` (Linux): `brumm.db`, `window-size`.

## Stack
Rust 2024 · iroh 1.5 · iroh-gossip · hpke · chacha20poly1305 · eframe/egui 0.37 · tokio · rusqlite (bundled) · postcard · ed25519-dalek 3 ·
bip39 · blake3 · qrcode · jiff. vodozemac 0.11 · nostr-sdk / nostr 0.45 (NIP-44; `local-relay` for tests). Planned: keyring + SQLCipher (M5).
