# PLAN-AGENTS: headless brumm for software, bots and agents (#14)

Goal: programs and coding agents talk over brumm without the desktop app, and you read along in your own app.

- **A. Agent room:** a group (#24) per project. Each agent is its own user identity and a member; you are the owner.
- **B. Bot channel:** a bot identity whose only contact is you, used as a 1:1 log channel. Its "key" is its profile
  dir (seed + Olm state). A leak exposes that one chat, nothing else.

Agents and bots may run on remote VPS hosts. MCP comes later (it will be a thin layer over the same daemon protocol).

## Decisions
| Topic | Choice | Why |
|---|---|---|
| Identity | one user identity per agent/bot | sender attribution, per-agent kick/revoke, a leak stays local |
| Process model | one daemon per OS user, hosting several profiles in one tokio runtime | several agents on one VPS share one runtime (RAM); profiles open lazily |
| Interface | JSON-RPC 2.0, NDJSON over a Unix socket (named pipe on Windows), versioned (`protocol.md`) | any language, no native bindings, many client processes per identity |
| CLI | `brumm-cli` talks to the daemon and auto-starts it; `send` also works one-shot when no daemon is wanted | cron/CI friendly |
| Python | standalone `brumm` package (uv, 3.12, stdlib only). The wheel bundles the `brumm-cli` binary; importing and connecting reuses a running daemon or spawns one | `uv add brumm` is all a bot needs; many imports/processes share one daemon |
| Core types | not exposed: the host layer has its own stable JSON format (keys/ids as base32 strings) | core types can keep changing |
| Auto-accept | host-level policy (`policy.toml`), no core change | bots must join without a click, but only from allowed users |

Tech: Rust 2024, tokio, clap 4, serde_json, `qrcode` (terminal QR); Python 3.12 + uv, maturin `bindings = "bin"` with a
`python-source` package (check that maturin supports this mixed layout; otherwise hatchling with a prebuilt binary per
platform tag). Wheels: linux x86_64/aarch64 (musl + glibc), macOS, Windows.

## Layout (as built)
```
crates/brumm-host/     native-only glue shared by brumm-app and brumm-cli
  lock.rs              ProfileLock (moved from brumm-app/src/bridge/profile_lock.rs)
  client.rs            async Client over Command/Event: send → MessageId, wait_status, read_since/read_latest, events
  mirror.rs            Snapshot of profile, contacts, groups, invitations (kept current from events)
  policy.rs            auto-accept rules + locked mode (pure, unit tested), policy.toml load/save
  enforcer.rs          answers group invitations by the policy (re-checks when the policy changes)
  dto.rs               stable JSON views (Room, Message, Contact, Status, Notice), base32 ids, room resolution
crates/brumm-cli/      binary `brumm-cli` (+ lib for tests)
  cli.rs / commands.rs clap subcommands → protocol calls over a Transport (daemon socket or in-process)
  host/                profile registry (lazy open, idle close, cap), Session + subscriptions, JSON-RPC methods
  daemon/              listener + lifecycle (mod), socket (Unix 0600 + peer uid, Windows pipe), spawn (connect or start)
  protocol.rs          request/response/notification framing, error codes, PROTOCOL_VERSION
  remote.rs            client side of the socket; transport.rs picks remote or local
  tests/               oneshot, daemon, concurrency (DB-safety races) e2e
python/                the Python package (sync + asyncio API, logging handler), examples, pytest
protocol.md            the daemon protocol (v1)
deploy/VPS.md, deploy/systemd/brumm-daemon.service
```

## Protocol sketch (v1)
Requests carry `profile`. Methods: `identity.init {name}`, `identity.restore {phrase, name}`, `identity.show`,
`invite.create`, `invite.accept {link}`, `contacts.list`, `rooms.list`, `message.send {room, text | file, id?}` → `{id}`,
`message.wait {room, id, status, timeout}`, `messages.read {room, since?, limit}` → `{messages, cursor, has_more}`
(`cursor`: arrival number, see H1),
`subscribe {rooms?}` → `event` notifications (`message`, `status`, `deleted`, `invitation`, `room`), `group.create`,
`group.invite`, `group.leave`, `policy.get/set`, `daemon.status`, `daemon.stop`.
A room is a contact name, a group name or an id (`d:…` / `g:…`). An ambiguous name is an error.

Socket: `$XDG_RUNTIME_DIR/brumm/daemon.sock` (fallback: data dir), mode 0600, peer-uid check. No TCP.

## Daemon lifecycle
- Started by the first client (`brumm-cli`, Python) or by systemd (`brumm-cli daemon`; unit template in `deploy/`).
- Holds each open profile's `ProfileLock`, so the desktop app and the daemon cannot both run the same profile (clear error).
- `--idle-exit <dur>`: auto-started daemons exit after the last client has gone and the outbox is empty (drain first).
  systemd daemons stay up, since bots that receive need to be online or poll the mailbox.

## Pairing on a VPS
- Default: create an invite in your app, then `brumm-cli --profile bot accept '<link>'` over ssh (no QR needed).
- Reverse: `brumm-cli invite` prints the link plus a terminal QR you scan in the app.
- `--lock` after pairing (bots): refuse new invites, contacts and links; accept group invitations only from `policy.toml`
  allowlisted users (default: the first contact).

## Phases
- [x] **H1 core + host**
  - [x] `SendText`/`SendFile` take an optional caller-chosen `id: Option<MessageId>`. A known id (stored or deleted) is a no-op (idempotent retries).
  - [x] `Command::LoadNewer { conversation, after, limit }` + keyset `store.conversation_page_after`.
        Built differently: the cursor is an **arrival number** (`messages.seq`, schema v12, set by a trigger from a
        `meta` counter that is never reused), not a message id over `(sent_at_ms, rowid)`. A late message (mailbox, sync)
        with an old timestamp still comes after the cursor, and deleting the cursor message does not stop the read.
        `Event::NewerMessages { after, messages, cursor, has_newer }`.
  - [x] `Command::LoadMessage { conversation, id }` → `Event::Message { message: Option<MessageView> }` (added): the
        client's ack for a send (the actor handles commands in order) and the current status for `wait_status`.
  - [x] `brumm-host`: move `ProfileLock`, async `Client` (match `MessageAdded`/`MessageStatusChanged` by id), `dto.rs`.
  - [x] `brumm-app` uses `brumm-host` for the lock (nothing else changes).
  - [x] Tests: send then wait for Delivered, cursor read across pages, retry with the same id stores once.
- [x] **H2 CLI one-shot** (enough for use case B)
  - [x] `init / restore / show / invite / accept / contacts / rooms / send / read / listen`, `--json` everywhere,
        `--profile`, `--data-dir`.
  - [x] `send` one-shot: start → send → wait for Sent/Delivered (`--wait`, default Sent) → drain → exit.
  - [x] Text over `MAX_TEXT_BYTES` (16 KiB): `--split` or send as file; stdin input (`echo … | brumm-cli send me`).
  - [x] Tests: CLI identity ↔ test node over the in-memory Nostr relay, exit right after send, never online together.
- [x] **H3 daemon**
  - [x] Socket + JSON-RPC v1, `protocol.md`, multi-profile registry, subscriptions with backpressure (bounded per client).
        Added: `LoadLatest` (listen "from now"), per-message cursors in reads, concurrency e2e tests
        (`tests/concurrency.rs`: retry storms, racing daemon starts, idle close vs new requests, kill -9, slow subscriber).
  - [x] CLI auto-starts and uses the daemon; one-shot stays available (`--no-daemon`).
  - [x] `policy.rs`: auto-accept group invitations from allowlisted contacts, lock mode.
  - [x] systemd user unit + VPS guide (persistent data dir, backups of the profile dir).
  - [x] Tests: two clients on one profile, daemon restart with a cursor, policy accept/reject, lock refuses invites.
- [x] **H4 Python package**
  - [x] `brumm.Client(profile=…)`: connect or spawn the daemon (`BRUMM_BIN` override), sync + `brumm.aio`.
  - [x] `send`, `read(since)`, `iter_messages()` / `async for`, `wait`, `rooms`, `invite`, `accept`.
  - [x] `brumm.logging.BrummHandler` (rate-limited and batched, so a log storm does not flood the chat).
  - [x] Wheels with the bundled binary (CI matrix), examples: log bot, agent room coordinator.
        maturin `bindings = "bin"` + `python-source` works: the wheel holds `brumm/` and `data/scripts/brumm-cli`
        (checked locally; `.github/workflows/python-wheels.yml` builds the matrix, not yet run).
  - [x] Tests: pytest against a real daemon on a temp data dir, two imports share one daemon.
- [x] **Docs:** SPEC.md section (headless hosts), CLAUDE.md status/layout/commands, README clients section.
- [ ] **Later:** MCP server over the protocol; `bot` flag in `Profile` (wire: appended field + invite version) so the
      app can label/mute bots; per-client tokens scoped to one room; desktop app as a daemon client (#19).

## Known limits
- The profile dir must persist (ratchet state). On an ephemeral host, losing it means re-pairing. Restoring from the seed
  creates a new device, and old sessions heal through fallback keys.
- Invites are not offline yet (M3): your app must be online while an agent accepts.
- Each open profile runs its own iroh endpoint and Nostr subscriptions. Measured: ~10 MB daemon, +23 MB first profile,
  +4 MB each further one (no relay traffic); default cap 16 (`--max-profiles`).
- A lock only refuses new invites: an invite created before `--lock` can still be accepted.
- Windows (named pipe, Python client) is written but untested.
