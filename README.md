# brumm

![brumm desktop client](brumm-desktop.png)

A fun, secure and decentralized messenger: no phone number, no account server.
Devices talk **directly peer-to-peer** ([iroh](https://github.com/n0-computer/iroh)), everything is
**end-to-end encrypted** (Olm double ratchet via [vodozemac](https://github.com/matrix-org/vodozemac)),
and when a device is offline its messages wait encrypted on **public Nostr relays**.

> ⚠️ **Pre-release.** The protocol and storage format still change without migrations, and the local
> database is not encrypted at rest yet. Don't use it for anything sensitive.

## Features (so far)
- **Identity from a 24-word seed phrase.** No phone number, email or server account.
- **Add contacts** with a single-use invite link or QR code.
- **1:1 chats** with end-to-end encryption. Profiles and device lists are encrypted too.
- **Multi-device**: link a new device with a link or QR code. It receives your contacts and the last 60 days of chats.
  You can remove lost devices.
- **Offline delivery** through public Nostr relays (no own server). Letters are signed by throwaway keys and
  encrypted with NIP-44, so relays learn neither who is talking nor what is said.
  Statuses: 🕓 pending → ✔ sent (in the mailbox) → ✔✔ delivered.
- **Groups** of up to 10 000 members without a server: owner, admins and members, announcement mode, invitations,
  kicking with a fresh group key, optional shared history for newcomers. Messages spread peer-to-peer (iroh-gossip),
  members serve the group's log to each other, and Nostr relays carry what nobody online received.

## Why brumm, compared to WhatsApp and Signal
WhatsApp and Signal encrypt message content well. brumm's different bet is **less trust in whoever runs
the infrastructure**, including metadata, not just content.

| | WhatsApp | Signal | brumm |
|---|---|---|---|
| Account needs a phone number | Yes | Yes | **No** (24-word seed phrase) |
| Central server required | Yes (Meta) | Yes (Signal Foundation) | **No own server.** Uses public iroh and Nostr infrastructure that anyone can replace |
| Can be switched off or blocked by one operator | Yes | Yes | **Much harder.** There is no single operator to shut down or compel |
| Social graph and metadata held by a company | Yes (Meta) | Minimal, but the server sees your number and when you connect | **No contact list on any server.** Relays see IPs and timing at most |
| Open source | No | Yes | Yes |
| End-to-end encryption | Yes (Signal protocol) | Yes (Signal protocol) | Yes (Olm double ratchet) |
| Multi-device without a primary phone | Limited | Limited (phone-linked) | **Yes**, any device can link, remove or revoke others |
| Groups without a server | No | No | **Yes**, up to 10 000 members, gossip + member-served logs |
| Bots and agents as ordinary identities | Business API only | Unofficial | **Yes**, `brumm-cli` and the Python package, no approval needed |

What that buys you: no phone number to leak, SIM-swap or hand over, no company that can build a
social graph from your contacts, no single point of failure or legal pressure, and an identity you
fully own and can restore from the seed phrase.

**Where they are still ahead, honestly:**
- brumm is **pre-release and unaudited**. Signal's protocol and apps have years of scrutiny. Don't
  trust brumm with anything sensitive yet.
- No native mobile apps (only desktop and a mobile-friendly web client), no push notifications,
  no voice or video calls, no stickers.
- The local database is **not encrypted at rest** yet.
- Signal's sealed sender and mature abuse and spam handling are more refined. brumm exposes your IP
  to the relays and, in direct connections, to your contacts.
- Offline delivery depends on public Nostr relays, which give no delivery guarantee. Signal's
  server holds messages reliably.
- Finding people is manual (invite link or QR). There is no phone-book discovery, by design.

## Clients
- **Desktop app** (Linux, macOS, Windows) built with [egui](https://github.com/emilk/egui).
- **Web client**: the same egui UI in the browser, optimized for mobile (single-pane chat list / chat,
  touch-friendly composer, on-screen keyboard aware). The core runs in a Web Worker and stores data in the
  browser's private storage (OPFS); it connects through iroh relays only.
  Build with `(cd crates/brumm-app && trunk serve)`.
- **`brumm-cli`** for bots, scripts and agents: a headless brumm identity per profile. One-shot
  (`echo done | brumm-cli --profile bot send me`) or through a local daemon that keeps profiles online and
  serves any language over a JSON-RPC socket ([protocol.md](protocol.md)). Pair it with your app once
  (`brumm-cli --profile bot accept '<invite link>' --lock`) and read its messages in your own chat list.
  Running on a server: [deploy/VPS.md](deploy/VPS.md).
- **Python package** ([python/](python/README.md)): `brumm.Client("bot").send("me", "build finished")`,
  `iter_messages()`, asyncio, and a rate-limited `logging` handler. The wheel bundles `brumm-cli`.

## Decentralization
brumm has no server of its own and no account system, but it does lean on public infrastructure
run by others. Where it is truly P2P and where it is not:

| Part | Status | Details |
|---|---|---|
| Identity, keys, contacts, history | **Fully P2P** | Seed phrase and device keys exist only on your devices. Data lives in the local SQLite database. No registry, no phone number. |
| Direct messages and files | **P2P** | End-to-end encrypted, sent device to device over iroh (QUIC, hole punching). Content is never visible to a third party. |
| Groups | **P2P** | iroh-gossip per group, members serve the log to each other. No group server. |
| Connection setup (iroh relays and DNS) | **Centralized default** | The default relays and address discovery are run by [n0](https://n0.computer). They help peers find each other and punch through NATs, and carry the traffic when no direct path works (always, in the browser). They only see encrypted traffic plus IP addresses and who connects to whom. They can be replaced or self-hosted, but brumm does not ship that yet. |
| Offline delivery | **Public, replaceable** | Encrypted letters wait on public Nostr relays (`relay.damus.io`, `nos.lol`, `relay.primal.net`), from a hard-coded list. Anyone can run one, and one reachable relay is enough. Relays see timing, size and your IP, not sender, recipient or content. |
| Web client hosting | **Centralized** | The browser build is static files from one host. Whoever serves it could ship you different code, so the desktop app is the more trustworthy client. Once loaded, the app does not depend on that host. |

In short: messages and keys are decentralized and end-to-end encrypted, while peer discovery, NAT
traversal fallback and offline delivery depend on third-party servers that can see metadata but not
content. If those go down, devices that are online together and know each other's addresses still
connect directly.

Planned: offline invites and contact introductions, stickers, native mobile apps,
Bluetooth/Wi‑Fi mesh. See [SPEC.md](SPEC.md) for the full design and milestones.

## Run
Requires Rust (stable, edition 2024).
```sh
cargo run --release                                    # default profile
cargo run --release -- --profile alice                 # separate identity per profile,
cargo run --release -- --profile bob                   # e.g. to chat with yourself locally
```
Profile data lives in `~/.local/share/brumm/profiles/<profile>/` (Linux).

Desktop integration (taskbar icon and app grid entry on Wayland/GNOME):
```sh
cargo build --release
scripts/install-desktop.sh      # -> ~/.local/share/{applications,icons/hicolor/256x256}, ~/.local/bin/brumm
```
Wayland compositors ignore window icons set by the client: they match the window's `app_id` against an
installed `brumm.desktop` and take its `Icon=`. Running straight from `target/` therefore shows the
generic icon until the entry and icon are installed.

## Test
```sh
cargo test                                                    # unit + local end-to-end (incl. in-memory Nostr relay)
cargo test -p brumm-core --test network -- --ignored          # needs internet: iroh relays, DNS discovery, public Nostr relays
(cd python && uv run --group dev pytest)                      # Python package against a real daemon (after cargo build)
```

## Layout
```
crates/brumm-core   protocol, crypto, storage, networking (no UI) — talks to UIs via Command/Event
crates/brumm-worker browser only: runs brumm-core in a Web Worker
crates/brumm-app    egui app (desktop + web)
crates/brumm-host   native glue for the app and headless hosts (profile lock, async client, JSON views, policy)
crates/brumm-cli    `brumm-cli`: one-shot commands and the daemon (protocol.md)
python/             the `brumm` Python package
SPEC.md             protocol + architecture specification
```

## License
Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
