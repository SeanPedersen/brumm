"""Errors raised by the brumm client; ``code`` is the protocol's JSON-RPC error code."""

from __future__ import annotations

from typing import Any

PARSE_ERROR = -32700
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
FAILED = -32000
NOT_FOUND = -32001
AMBIGUOUS = -32002
REFUSED = -32003
TIMEOUT = -32004
BUSY = -32005


class BrummError(Exception):
    def __init__(self, code: int, message: str, data: Any = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.data = data

    @classmethod
    def from_json(cls, error: dict[str, Any]) -> BrummError:
        code = int(error.get("code", FAILED))
        kind = BrummTimeout if code == TIMEOUT else BrummRefused if code == REFUSED else cls
        return kind(code, str(error.get("message", "")), error.get("data"))


class BrummTimeout(BrummError):
    """Waited too long (e.g. for Delivered); ``data["status"]`` says how far it got."""


class BrummRefused(BrummError):
    """Refused by the profile's policy (lock mode)."""
