"""Plain views of the protocol's JSON (see ``protocol.md``); ids are base32 strings."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class Identity:
    user: str
    name: str
    fingerprint: str

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> Identity:
        return cls(user=data["user"], name=data["name"], fingerprint=data["fingerprint"])


@dataclass(frozen=True)
class Contact:
    user: str
    name: str
    fingerprint: str
    room: str

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> Contact:
        return cls(user=data["user"], name=data["name"], fingerprint=data["fingerprint"], room=data["room"])


@dataclass(frozen=True)
class Room:
    id: str
    kind: str
    """``notes``, ``direct`` or ``group``."""
    name: str
    group: dict[str, Any] | None = None

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> Room:
        return cls(id=data["id"], kind=data["kind"], name=data["name"], group=data.get("group"))


@dataclass(frozen=True)
class FileInfo:
    name: str
    size: int


@dataclass(frozen=True)
class Message:
    id: str
    room: str
    direction: str
    """``in`` or ``out``."""
    text: str
    sent_at_ms: int
    status: str
    file: FileInfo | None = None
    sender: str | None = None
    """Groups: the author's user key (incoming messages)."""
    sender_name: str | None = None
    system: bool = False
    cursor: int | None = None
    """In reads: read on from here to continue right after this message."""

    @property
    def incoming(self) -> bool:
        return self.direction == "in"

    @classmethod
    def from_json(cls, data: dict[str, Any], cursor: int | None = None) -> Message:
        file = data.get("file")
        sender = data.get("sender") or {}
        return cls(
            id=data["id"],
            room=data["room"],
            direction=data["direction"],
            text=data.get("text", ""),
            sent_at_ms=data["sent_at_ms"],
            status=data["status"],
            file=FileInfo(file["name"], file["size"]) if file else None,
            sender=sender.get("user"),
            sender_name=sender.get("name"),
            system=bool(data.get("system", False)),
            cursor=data.get("cursor", cursor),
        )


@dataclass(frozen=True)
class Page:
    room: str
    messages: list[Message]
    cursor: int
    has_more: bool

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> Page:
        return cls(
            room=data["room"],
            messages=[Message.from_json(message) for message in data["messages"]],
            cursor=data["cursor"],
            has_more=data["has_more"],
        )


@dataclass(frozen=True)
class Sent:
    id: str
    room: str
    status: str

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> Sent:
        return cls(id=data["id"], room=data["room"], status=data["status"])


@dataclass(frozen=True)
class Invitation:
    room: str
    name: str
    inviter: str
    inviter_name: str
    members: int
    stage: str

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> Invitation:
        return cls(**{key: data[key] for key in ("room", "name", "inviter", "inviter_name", "members", "stage")})
