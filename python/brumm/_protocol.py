"""Pieces shared by the sync and the asyncio client: request parameters and the cursor follower.

``Follower`` is pure: notices in, rooms to read out; pages in, messages out. It follows rooms by
cursor, so a ``message`` notice only says "read this room again": nothing is yielded twice or
skipped, also after a ``lagged`` notice (the reader fell behind and events were dropped).
"""

from __future__ import annotations

import base64
import os
from typing import Any

from .types import Message, Page

JSONRPC = "2.0"
EVENT_METHOD = "event"
DEFAULT_PROFILE = "default"
WAIT_CHOICES = (None, "sent", "delivered")


def send_params(
    room: str,
    text: str | None,
    *,
    path: str | os.PathLike[str] | None,
    data: bytes | None,
    name: str | None,
    id: str | None,
    wait: str | None,
    timeout: float,
) -> dict[str, Any]:
    if wait not in WAIT_CHOICES:
        raise ValueError(f"wait must be one of {WAIT_CHOICES}")
    given = sum(value is not None for value in (text, path, data))
    if given != 1:
        raise ValueError("give exactly one of text, path or data")
    params: dict[str, Any] = {"room": room, "wait": wait or "none", "timeout_ms": int(timeout * 1000)}
    if text is not None:
        params["text"] = text
    elif path is not None:
        params["path"] = os.path.abspath(os.fspath(path))
    else:
        assert data is not None
        params["file"] = {"name": name or "file.bin", "data": base64.b64encode(data).decode("ascii")}
    if id is not None:
        params["id"] = id
    return params


class Follower:
    def __init__(self, incoming_only: bool) -> None:
        self.incoming_only = incoming_only
        self.cursors: dict[str, int] = {}

    def start(self, room: str, cursor: int) -> None:
        self.cursors[room] = cursor

    def stale_rooms(self, notice: dict[str, Any]) -> list[str]:
        kind = notice.get("type")
        if kind == "message":
            return [notice["message"]["room"]]
        if kind == "lagged":
            return list(self.cursors)
        if kind == "changed":
            return list(notice.get("rooms", []))
        return []

    def read_params(self, room: str) -> dict[str, Any]:
        # A room that appeared after we started (new contact or group) is read in full.
        return {"room": room, "since": self.cursors.get(room, 0)}

    def take(self, page: Page) -> list[Message]:
        """The messages to yield from a page read with ``read_params``; advances the cursor."""
        self.cursors[page.room] = page.cursor
        return [message for message in page.messages if not (self.incoming_only and not message.incoming)]
