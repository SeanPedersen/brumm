"""Use case B: a bot profile as your private log channel.

Pair it once (the app must be online), then every run logs into your chat with the bot:

    brumm-cli --profile buildbot init "Build bot"
    brumm-cli --profile buildbot accept 'brumm://invite/…' --lock
    uv run examples/log_bot.py
"""

import logging
import time

from brumm.logging import BrummHandler

handler = BrummHandler("me", profile="buildbot", interval=5, max_per_minute=6)
handler.setFormatter(logging.Formatter("%(levelname)s %(name)s: %(message)s"))
handler.setLevel(logging.WARNING)
logging.basicConfig(level=logging.INFO)
logging.getLogger().addHandler(handler)

log = logging.getLogger("nightly")
log.info("starting")  # stays local: below the handler's level
for step in ("fetch", "build", "test"):
    time.sleep(0.5)
    log.warning("%s done", step)
log.error("deploy skipped: no approval")
handler.close()  # sends what is still batched
