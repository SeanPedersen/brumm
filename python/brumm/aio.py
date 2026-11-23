"""The asyncio client: ``async with await brumm.aio.connect(profile="bot") as client``.

Same methods as the blocking client, as coroutines; ``messages()`` is an async iterator.
One reader task routes answers (by id) and subscription events.
"""

from __future__ import annotations

import asyncio
import itertools
import json
import os
from collections.abc import AsyncIterator
from typing import Any

from . import _daemon
from ._protocol import DEFAULT_PROFILE, EVENT_METHOD, JSONRPC, Follower, send_params
from .errors import BrummError
from .types import Contact, Identity, Message, Page, Room, Sent

DEFAULT_TIMEOUT = 60.0
#: A file of up to 10 MiB travels base64-encoded on one line.
LINE_LIMIT = 16 * 1024 * 1024
#: Events not yet taken by ``messages()``; beyond that the daemon reports ``lagged``.
EVENT_BUFFER = 1024


async def connect(
    profile: str = DEFAULT_PROFILE,
    *,
    data_dir: str | os.PathLike[str] | None = None,
    socket: str | os.PathLike[str] | None = None,
    relays: list[str] | None = None,
    binary: str | os.PathLike[str] | None = None,
) -> AsyncClient:
    """Connects to the daemon, starting it if none runs."""
    settings = _daemon.Settings(data_dir=data_dir, socket=socket, relays=relays or [], binary=binary)
    path = await asyncio.to_thread(_daemon.ensure_daemon, settings)
    reader, writer = await asyncio.open_unix_connection(path, limit=LINE_LIMIT)
    return AsyncClient(profile, reader, writer)


class AsyncClient:
    def __init__(self, profile: str, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        self.profile = profile
        self._writer = writer
        self._ids = itertools.count(1)
        self._answers: dict[int, asyncio.Future[dict[str, Any]]] = {}
        self._events: asyncio.Queue[dict[str, Any]] = asyncio.Queue(EVENT_BUFFER)
        self._lagged = False
        self._reader = asyncio.create_task(self._read_loop(reader))

    async def close(self) -> None:
        self._reader.cancel()
        self._writer.close()
        try:
            await self._writer.wait_closed()
        except (ConnectionError, OSError):
            pass

    async def __aenter__(self) -> AsyncClient:
        return self

    async def __aexit__(self, *_: object) -> None:
        await self.close()

    async def _read_loop(self, reader: asyncio.StreamReader) -> None:
        try:
            while line := await reader.readline():
                message = json.loads(line)
                if message.get("method") == EVENT_METHOD:
                    try:
                        self._events.put_nowait(message["params"])
                    except asyncio.QueueFull:
                        # Dropped; ``messages()`` reads every room again from its cursor.
                        self._lagged = True
                    continue
                waiter = self._answers.pop(message.get("id"), None)
                if waiter is not None and not waiter.done():
                    waiter.set_result(message)
        finally:
            for waiter in self._answers.values():
                if not waiter.done():
                    waiter.set_exception(ConnectionError("the brumm daemon closed the connection"))

    async def call(self, method: str, **params: Any) -> Any:
        params.setdefault("profile", self.profile)
        request_id = next(self._ids)
        waiter: asyncio.Future[dict[str, Any]] = asyncio.get_running_loop().create_future()
        self._answers[request_id] = waiter
        line = json.dumps({"jsonrpc": JSONRPC, "id": request_id, "method": method, "params": params})
        self._writer.write(line.encode() + b"\n")
        await self._writer.drain()
        answer = await waiter
        if answer.get("error"):
            raise BrummError.from_json(answer["error"])
        return answer.get("result")

    async def identity(self) -> Identity | None:
        found = (await self.call("identity.show"))["identity"]
        return Identity.from_json(found) if found else None

    async def init(self, name: str) -> tuple[Identity, str]:
        result = await self.call("identity.init", name=name)
        return Identity.from_json(result["identity"]), result["seed_phrase"]

    async def invite(self) -> str:
        return (await self.call("invite.create"))["link"]

    async def accept(self, link: str, *, lock: bool = False) -> Contact:
        return Contact.from_json((await self.call("invite.accept", link=link, lock=lock))["contact"])

    async def rooms(self) -> list[Room]:
        return [Room.from_json(room) for room in (await self.call("rooms.list"))["rooms"]]

    async def send(
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
        params = send_params(room, text, path=path, data=data, name=name, id=id, wait=wait, timeout=timeout)
        return Sent.from_json(await self.call("message.send", **params))

    async def wait(self, room: str, id: str, status: str = "delivered", timeout: float = DEFAULT_TIMEOUT) -> str:
        result = await self.call("message.wait", room=room, id=id, status=status, timeout_ms=int(timeout * 1000))
        return result["status"]

    async def read(
        self, room: str, since: int | None = None, limit: int | None = None, *, last: int | None = None
    ) -> Page:
        params: dict[str, Any] = {"room": room, "since": since, "limit": limit}
        if last is not None:
            params["last"] = last
        return Page.from_json(await self.call("messages.read", **params))

    async def messages(
        self,
        rooms: list[str] | None = None,
        *,
        since: int | None = None,
        incoming_only: bool = True,
        timeout: float | None = None,
    ) -> AsyncIterator[Message]:
        """Messages as they arrive (see ``Client.iter_messages``)."""
        if since is not None and (rooms is None or len(rooms) != 1):
            raise ValueError("since needs exactly one room")
        await self.call("subscribe", rooms=rooms)
        follower = Follower(incoming_only)
        for room in rooms or [room.id for room in await self.rooms()]:
            start = await self.read(room, since=since, limit=0) if since is not None else await self.read(room, last=0)
            follower.start(start.room, since if since is not None else start.cursor)
        stale = list(follower.cursors) if since is not None else []
        while True:
            for room in stale:
                while True:
                    page = Page.from_json(await self.call("messages.read", **follower.read_params(room)))
                    for message in follower.take(page):
                        yield message
                    if not page.has_more:
                        break
            if self._lagged:
                self._lagged = False
                stale = list(follower.cursors)
                continue
            try:
                event = await asyncio.wait_for(self._events.get(), timeout)
            except TimeoutError:
                return
            stale = follower.stale_rooms(event["event"])
