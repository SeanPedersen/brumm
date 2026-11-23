# brumm for Python

Bots, log channels and agent rooms on [brumm](https://github.com/SeanPedersen/brumm), the
decentralized end-to-end encrypted messenger. Standard library only; the wheel bundles the
`brumm-cli` binary, whose daemon the package talks to over a local socket.

```sh
uv add brumm
brumm-cli --profile bot init "My bot"
brumm-cli --profile bot accept 'brumm://invite/…' --lock   # created in your app
```

```python
import brumm

with brumm.Client("bot") as client:
    sent = client.send("me", "build finished", wait="delivered")   # "me": the bot's owner
    for message in client.iter_messages(["me"]):                    # replies, as they come
        print(message.text, message.cursor)
```

- `send(room, text | path= | data=, id=, wait=None|"sent"|"delivered")`: `id` makes retries safe.
- `read(room, since=cursor, limit=)` / `read(room, last=n)`: arrival order, with cursors.
- `iter_messages(rooms, since=)`: follows rooms by cursor; nothing twice, nothing skipped.
- `brumm.aio.connect(...)`: the same as coroutines, `async for message in client.messages()`.
- `brumm.logging.BrummHandler("me", profile="bot")`: batched, rate limited logging.
- `create_group`, `invite_to_group`, `accept_group`, `leave_group`, `policy`, `set_policy`.

Rooms are a contact name, a group name, `notes`, `me` (the first contact) or a room id
(`d:…` / `g:…`). The first client starts the daemon (`brumm-cli daemon start`); other
processes share it, and it exits a minute after the last one disconnects. `BRUMM_BIN` picks a
different binary. Protocol: `protocol.md` in the repository.

Tests: `cargo build -p brumm-cli && uv run --group dev pytest`.
