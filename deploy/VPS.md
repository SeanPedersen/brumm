# Running brumm bots on a VPS

A bot or agent is a brumm profile: a directory holding its seed and its encryption (ratchet) state. The
`brumm-cli` daemon keeps profiles online and serves programs over a local socket (`protocol.md`).

## Install
```sh
cargo install --path crates/brumm-cli --profile dist    # or: uv tool install brumm (wheel bundles the binary)
```

## Pair a bot with your app
```sh
brumm-cli --profile buildbot init "Build bot"
# In your app: ☰ → Invite, copy the link. Your app must be online while the bot accepts.
brumm-cli --profile buildbot accept 'brumm://invite/…' --lock
brumm-cli --profile buildbot send me "hello from the VPS"
```
`--lock` refuses any further invite, so a leaked socket cannot pair the bot with someone else. The reverse also
works: `brumm-cli --profile buildbot invite` prints a link and a terminal QR to scan in the app.

Group (agent room): create the group in your app and invite the bot; a locked bot accepts invitations from its
first contact (you) on its own. More users: `brumm-cli --profile buildbot policy --allow alice`.

## Keep it running
Programs start the daemon on demand and it exits a minute after the last one. A bot that should **receive**
around the clock runs it under systemd instead:
```sh
cp deploy/systemd/brumm-daemon.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now brumm-daemon
loginctl enable-linger "$USER"
journalctl --user -u brumm-daemon -f
```
Without a daemon, cron jobs can still send one-shot: `brumm-cli --no-daemon --profile buildbot send me "done"`
(waits until the message reached a relay, then exits).

## Data and backups
- Profiles live in `~/.local/share/brumm/profiles/<name>/` (`--data-dir` / `BRUMM_DATA_DIR` to move them). The
  directory **must persist**: on an ephemeral container, mount a volume.
- Back up the profile directory while the daemon is stopped (or copy `brumm.db` with `sqlite3 brumm.db ".backup
  out.db"`). An old backup is safe to restore: sessions heal through fallback keys.
- Lost the directory? `brumm-cli --profile buildbot restore` with the seed phrase from `init` creates a new
  device of the same identity; keep the phrase off the VPS.
- A leaked profile directory exposes that bot's chats only: stop using it and pair a new bot.

## Memory
The daemon itself takes ~10 MB; the first open profile ~23 MB more, each further one ~4 MB (measured without
relay traffic; real relays add a few MB). At most 16 profiles are open at once (`daemon run --max-profiles`); with
`--idle-exit`, unused ones close after that long.
