"""The blocking client: ``brumm.Client(profile="bot")``.

Connecting reuses the running daemon or starts one (``brumm-cli daemon start``), so many
imports and processes share it. Thread-safe: calls from several threads share one connection;
whichever thread reads routes answers and events to whom they belong.
"""

from __future__ import annotations

import collections
import itertools
import os
import threading
import time
from collections.abc import Iterator
from typing import Any

from . import _daemon
from ._connection import Connection
from ._protocol import DEFAULT_PROFILE, EVENT_METHOD, JSONRPC, Follower, send_params
from .errors import BrummError
from .types import Contact, Identity, Invitation, Page, Room, Sent

DEFAULT_TIMEOUT = 60.0
#: Events kept for ``next_event``; beyond that they are dropped and ``lagged`` is set.
EVENT_BUFFER = 1024


class Client:
    def __init__(
        self,
        profile: str = DEFAULT_PROFILE,
        *,
        data_dir: str | os.PathLike[str] | None = None,
        socket: str | os.PathLike[str] | None = None,
        relays: list[str] | None = None,
        binary: str | os.PathLike[str] | None = None,
    ) -> None:
        self.profile = profile
        settings = _daemon.Settings(data_dir=data_dir, socket=socket, relays=relays or [], binary=binary)
        self.socket_path = _daemon.ensure_daemon(settings)
        self._connection = Connection(self.socket_path)
        self._ids = itertools.count(1)
        self._write_lock = threading.Lock()
        self._state = threading.Condition()
        self._reading = False
        self._answers: dict[int, dict[str, Any]] = {}
        self._events: collections.deque[dict[str, Any]] = collections.deque()
        self.lagged = False
        """Events were dropped because nobody took them; read again from your cursors."""

    def close(self) -> None:
        self._connection.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    # ── transport ───────────────────────────────────────────────────────────

    def call(self, method: str, **params: Any) -> Any:
        """A raw protocol call; ``profile`` defaults to this client's."""
        params.setdefault("profile", self.profile)
        request_id = next(self._ids)
        with self._write_lock:
            self._connection.write({"jsonrpc": JSONRPC, "id": request_id, "method": method, "params": params})
        answer = self._pump(lambda: self._answers.pop(request_id, None), timeout=None)
        assert answer is not None
        if answer.get("error"):
            raise BrummError.from_json(answer["error"])
        return answer.get("result")

    def next_event(self, timeout: float | None = None) -> dict[str, Any] | None:
        """The next subscription notification (``subscription``, ``profile``, ``event``)."""
        return self._pump(lambda: self._events.popleft() if self._events else None, timeout)

    def _pump(self, take: Any, timeout: float | None) -> Any:
        deadline = None if timeout is None else time.monotonic() + timeout
        with self._state:
            while True:
                found = take()
                if found is not None:
                    return found
                remaining = None if deadline is None else deadline - time.monotonic()
                if remaining is not None and remaining <= 0:
                    return None
                if self._reading:
                    self._state.wait(remaining)
                    continue
                self._reading = True
                self._state.release()
                try:
                    message = self._connection.read(remaining)
                finally:
                    self._state.acquire()
                    self._reading = False
                    self._state.notify_all()
                if message is None:
                    continue
                if message.get("method") == EVENT_METHOD:
                    if len(self._events) < EVENT_BUFFER:
                        self._events.append(message["params"])
                    else:
                        self.lagged = True
                elif isinstance(message.get("id"), int):
                    self._answers[message["id"]] = message

    # ── identity and contacts ──────────────────────────────────────────────

    def identity(self) -> Identity | None:
        found = self.call("identity.show")["identity"]
        return Identity.from_json(found) if found else None

    def init(self, name: str) -> tuple[Identity, str]:
        """Creates the profile's identity; returns it and its seed phrase (keep it safe)."""
        result = self.call("identity.init", name=name)
        return Identity.from_json(result["identity"]), result["seed_phrase"]

    def restore(self, phrase: str, name: str) -> Identity:
        return Identity.from_json(self.call("identity.restore", phrase=phrase, name=name)["identity"])

    def invite(self) -> str:
        """A single-use invite link (works while the daemon has this profile open)."""
        return self.call("invite.create")["link"]

    def accept(self, link: str, *, lock: bool = False) -> Contact:
        return Contact.from_json(self.call("invite.accept", link=link, lock=lock)["contact"])

    def contacts(self) -> list[Contact]:
        return [Contact.from_json(contact) for contact in self.call("contacts.list")["contacts"]]

    def rooms(self) -> list[Room]:
        return [Room.from_json(room) for room in self.call("rooms.list")["rooms"]]

    def invitations(self) -> list[Invitation]:
        return [Invitation.from_json(invitation) for invitation in self.call("invitations.list")["invitations"]]

    # ── messages ───────────────────────────────────────────────────────────

    def send(
        self,
        room: str,
        text: str | None = None,
        *,
        path: str | os.PathLike[str] | None = None,
        data: bytes | None = None,
        name: str | None = None,
        id: str | None = None,
        wait: str | None = "sent",
        timeout: float = DEFAULT_TIMEOUT,
    ) -> Sent:
        """Sends text, a file at ``path`` or ``data`` named ``name``. ``id`` (base32, 16 bytes)
        makes a retry a no-op. ``wait``: None, ``"sent"`` (in the mailbox) or ``"delivered"``."""
        params = send_params(room, text, path=path, data=data, name=name, id=id, wait=wait, timeout=timeout)
        return Sent.from_json(self.call("message.send", **params))

    def wait(self, room: str, id: str, status: str = "delivered", timeout: float = DEFAULT_TIMEOUT) -> str:
        return self.call("message.wait", room=room, id=id, status=status, timeout_ms=int(timeout * 1000))["status"]

    def read(
        self, room: str, since: int | None = None, limit: int | None = None, *, last: int | None = None
    ) -> Page:
        """Messages after cursor ``since`` (from the first), or the ``last`` ones, by arrival."""
        params: dict[str, Any] = {"room": room, "since": since, "limit": limit}
        if last is not None:
            params["last"] = last
        return Page.from_json(self.call("messages.read", **params))

    def iter_messages(
        self,
        rooms: list[str] | None = None,
        *,
        since: int | None = None,
        incoming_only: bool = True,
        timeout: float | None = None,
    ) -> Iterator[Any]:
        """Messages as they arrive (all rooms if ``rooms`` is None), each with its ``cursor``.

        With ``since`` (one room only), what arrived after that cursor comes first. Ends after
        ``timeout`` seconds without any event, if given.
        """
        if since is not None and (rooms is None or len(rooms) != 1):
            raise ValueError("since needs exactly one room")
        self.call("subscribe", rooms=rooms)
        follower = Follower(incoming_only)
        for room in rooms or [room.id for room in self.rooms()]:
            start = self.read(room, since=since, limit=0) if since is not None else self.read(room, last=0)
            follower.start(start.room, since if since is not None else start.cursor)
        stale = list(follower.cursors) if since is not None else []
        while True:
            for room in stale:
                while True:
                    page = Page.from_json(self.call("messages.read", **follower.read_params(room)))
                    yield from follower.take(page)
                    if not page.has_more:
                        break
            if self.lagged:
                self.lagged = False
                stale = list(follower.cursors)
                continue
            event = self.next_event(timeout)
            if event is None:
                return
            stale = follower.stale_rooms(event["event"])

    # ── groups and policy ──────────────────────────────────────────────────

    def create_group(self, name: str, members: list[str], *, announcement: bool = False) -> Room:
        return Room.from_json(
            self.call("group.create", name=name, members=members, announcement=announcement)["room"]
        )

    def invite_to_group(self, room: str, users: list[str]) -> None:
        self.call("group.invite", room=room, users=users)

    def accept_group(self, room: str) -> None:
        self.call("group.accept", room=room)

    def decline_group(self, room: str) -> None:
        self.call("group.decline", room=room)

    def leave_group(self, room: str) -> None:
        self.call("group.leave", room=room)

    def policy(self) -> dict[str, Any]:
        return self.call("policy.get")["policy"]

    def set_policy(self, **changes: Any) -> dict[str, Any]:
        """E.g. ``set_policy(locked=True)`` or ``set_policy(allow=[user_key])``."""
        policy = {**self.policy(), **changes}
        return self.call("policy.set", policy=policy)["policy"]

    def daemon_status(self) -> dict[str, Any]:
        return self.call("daemon.status")

    def stop_daemon(self) -> None:
        """Stops the shared daemon (it drains open profiles first)."""
        self.call("daemon.stop")
