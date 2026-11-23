"""brumm for Python: bots, log channels and agent rooms on the brumm messenger.

>>> import brumm
>>> with brumm.Client(profile="bot") as client:
...     client.send("me", "build finished")

The package talks to the ``brumm-cli`` daemon (bundled in the wheel) over a local socket; the
first client starts it, later ones (in any process) share it. Rooms are named by contact name,
group name, ``notes``, ``me`` (the first contact) or room id. See ``brumm.aio`` for asyncio and
``brumm.logging.BrummHandler`` for logging.
"""

from .client import Client
from .errors import BrummError, BrummRefused, BrummTimeout
from .types import Contact, Identity, Invitation, Message, Page, Room, Sent

__all__ = [
    "BrummError",
    "BrummRefused",
    "BrummTimeout",
    "Client",
    "Contact",
    "Identity",
    "Invitation",
    "Message",
    "Page",
    "Room",
    "Sent",
]
