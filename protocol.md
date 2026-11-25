# brumm daemon protocol, version 1

The `brumm-cli` daemon hosts brumm profiles (one identity each) for programs: bots, log channels and
agents (PLAN-AGENTS.md). Any language can talk to it; the Python package (`python/`) and `brumm-cli` itself
are clients. This file is the contract. Fields may be **added** within version 1; removing or changing
one bumps `protocol` (see `daemon.status`).

## Transport and framing
- **Unix:** a stream socket at `$XDG_RUNTIME_DIR/brumm/daemon.sock` for the default data root, or
  `daemon-<12 hex>.sock` (a hash of the root) for another `--data-dir`. Without `XDG_RUNTIME_DIR` the socket
  lives in the data root. The directory is `0700`, the socket `0600`, and the daemon refuses peers whose uid
  differs from the socket's owner. There is no TCP listener: reach a remote daemon over `ssh`.
- **Windows:** a named pipe `\\.\pipe\brumm-…` (not yet tested).
- **Framing:** NDJSON, one JSON-RPC 2.0 object per line, UTF-8, `\n` terminated. A line may be up to 16 MiB
  (a 10 MiB file travels base64 encoded). Requests on one connection run concurrently; answers carry the
  request `id` and may come out of order. When a connection closes, its unfinished requests are cancelled.
- **Start:** `brumm-cli daemon start` connects to a running daemon or starts one (a lock file makes racing
  starts end with one daemon) and prints `daemon.status`. Auto-started daemons exit after
  `--idle-exit` (CLI default 60 s) with no client connected; profiles close after that long unused.

## Requests
```json
{"jsonrpc":"2.0","id":1,"method":"message.send","params":{"profile":"bot","room":"me","text":"hi"}}
{"jsonrpc":"2.0","id":1,"result":{"id":"…","room":"d:…","status":"sent"}}
```
Every method except `daemon.*`, `profiles.list` and `unsubscribe` takes `profile` (default `"default"`,
`[A-Za-z0-9_-]{1,64}`). A profile is opened on first use; one held by another process (the desktop app,
a one-shot `brumm-cli --no-daemon`) answers `BUSY`.

**Ids** (users, messages, groups) are lowercase base32 without padding. **Rooms** are named by a contact's name,
a group's name, `notes` (the identity's own notes chat), `me` (the first contact: a bot's owner) or a room id:
`d:<user>` (direct), `g:<group>` (group). A name that matches several rooms is `AMBIGUOUS`; use the id.

| Method | Params | Result |
|---|---|---|
| `daemon.status` | | `{pid, uptime_ms, connections, protocol, version, root, open: [profile]}` |
| `daemon.stop` | | `{}`; open profiles drain (outbox to the relays) before the daemon exits |
| `profiles.list` | | `{profiles: [name], open: [name]}` (on disk / open now) |
| `identity.init` | `name` | `{identity, seed_phrase}` |
| `identity.restore` | `phrase, name` | `{identity}` (a new device of that identity) |
| `identity.show` | | `{identity \| null, locked}` |
| `invite.create` | | `{link}`: a single-use `brumm://invite/…`; refused when locked |
| `invite.accept` | `link, lock?` | `{contact}` once the inviter answered; `lock: true` locks the profile afterwards |
| `contacts.list` | | `{contacts: [Contact]}` |
| `rooms.list` | | `{rooms: [Room]}` |
| `invitations.list` | | `{invitations: [Invitation]}` (group invitations) |
| `message.send` | `room`, one of `text` / `file {name, data}` (base64) / `path` (read by the daemon), `id?`, `wait?`, `timeout_ms?` | `{id, room, status}` |
| `message.wait` | `room, id, status` (`sent` / `delivered`), `timeout_ms?` | `{status}` |
| `message.get` | `room, id` | `{message: Message \| null}` |
| `messages.read` | `room`, `since?`, `limit?` **or** `last` | `{room, messages, cursor, has_more}` |
| `subscribe` | `rooms?` (all rooms if absent) | `{subscription}`; then `event` notifications |
| `unsubscribe` | `subscription` | `{}` |
| `group.create` | `name, members: [user], announcement?` | `{room}` |
| `group.invite` | `room, users: [user]` | `{}` |
| `group.accept` / `group.decline` | `room` (of the invitation) | `{}` |
| `group.leave` | `room` | `{}` |
| `policy.get` | | `{policy}` |
| `policy.set` | `policy` (whole object) | `{policy}`; takes effect at once |

**Sending.** `id` (16 bytes, base32) is chosen by the caller to make retries safe: a known id (stored, or
deleted) is a no-op that answers the stored message's status. `wait`: `none` answers once the message is
stored; `sent` (default) once it reached the recipient or a mailbox relay; `delivered` once the recipient
confirmed it. Groups tick `sent` only. A wait that runs out answers `TIMEOUT` with `data: {id, room, status}`:
the message is stored and keeps being retried, so wait again or resend with the same id.
Text is limited to 56 KiB and files to 10 MiB (`brumm-cli send` sends longer text as a file or `--split`). Offline readers get anything up to 1.5 MiB through the Nostr mailbox (in parts); larger files only arrive while both are online.

**Reading.** Messages are numbered in **arrival order** on this device (not send time), and `cursor` is the
arrival number of the last message returned. `since: cursor` returns what arrived after it, so a message that
arrives late (from a mailbox, with an old timestamp) is still read after an earlier cursor. Without `since`
reading starts at the first message; `limit` caps a page, and `has_more` says another page is waiting.
`last: n` returns the newest `n` messages and the current cursor (`last: 0`: only the cursor, "from now").
Each message also carries its own `cursor`: resuming from it continues right after that message. Cursors stay
valid across daemon restarts and never repeat, also after deletes.

## Views
```jsonc
Identity   {"user", "name", "fingerprint"}
Contact    {"user", "name", "fingerprint", "room"}
Room       {"id", "kind": "notes"|"direct"|"group", "name",
            "group"?: {"members", "role": "owner"|"admin"|"member", "active", "announcement"}}
Message    {"id", "room", "direction": "in"|"out", "text", "sent_at_ms",
            "status": "pending"|"sent"|"delivered"|"failed",
            "file"?: {"name", "size"}, "sender"?: {"user", "name"}, "system"?: true, "cursor"?}
Invitation {"room", "name", "inviter", "inviter_name", "members", "stage"}
Policy     {"locked": false, "allow": [user], "auto_accept": true}
```
`sender` is set in groups; `system` marks a line about the group itself ("Bob joined"). Read results never
carry file bytes.

## Notifications
```json
{"jsonrpc":"2.0","method":"event","params":{"subscription":1,"profile":"bot","event":{"type":"message","message":{…}}}}
```
| `type` | Fields | Meaning |
|---|---|---|
| `message` | `message` | a message was stored (incoming, or sent by another client of this profile) |
| `status` | `room, id, status` | a message ticked |
| `deleted` | `room, ids` | messages were deleted |
| `invitation` | `invitations` | the open group invitations changed |
| `room` | `room` | a contact was added or a group changed |
| `changed` | `rooms` | many messages changed at once (history sync): read these rooms again |
| `lagged` | | events were dropped because the client read too slowly: read every room again from its cursor |

Each connection has a bounded queue (256 lines); a slow reader never holds up the daemon or other clients, it
gets `lagged` instead. Robust clients therefore treat a `message` notice as "read this room from my cursor"
(as `brumm-cli listen` and the Python `iter_messages` do), which never misses or repeats a message.

## Errors
| Code | Name | When |
|---|---|---|
| -32700 | parse error | the line is not JSON |
| -32600 | invalid request | not a JSON-RPC request |
| -32601 | method not found | |
| -32602 | invalid params | missing or malformed parameters, not a contact |
| -32000 | failed | anything else (message says why) |
| -32001 | not found | room, message or identity does not exist |
| -32002 | ambiguous | a room name matches several rooms (the message names them) |
| -32003 | refused | the profile's policy forbids it (locked) |
| -32004 | timeout | a wait ran out (`data` says how far it got) |
| -32005 | busy | the profile is open in another process |

## Policy (`policy.toml` in the profile dir)
- `locked`: refuse new invites (creating and accepting); set by `invite.accept {lock: true}`.
- `allow`: users whose group invitations are answered automatically; empty means the first contact.
- `auto_accept`: accept invitations from `allow`ed users (else they wait for `group.accept`). Invitations from
  anyone else are declined when locked and left open otherwise.
