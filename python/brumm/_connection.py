"""A blocking NDJSON connection to the daemon (Unix socket, or a named pipe on Windows).

Lines are buffered here, not by a file object, so a read timeout never cuts a line in half.
"""

from __future__ import annotations

import json
import select
import socket
import sys
from typing import Any

RECV_BYTES = 64 * 1024


class Connection:
    def __init__(self, path: str) -> None:
        self._buffer = bytearray()
        if sys.platform == "win32":
            # Named pipes have no select(): reads block (timeouts are not supported there).
            self._pipe = open(path, "r+b", buffering=0)  # noqa: SIM115 - closed in close()
            self._socket = None
        else:
            self._pipe = None
            self._socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self._socket.connect(path)

    def close(self) -> None:
        if self._socket is not None:
            self._socket.close()
        if self._pipe is not None:
            self._pipe.close()

    def write(self, message: dict[str, Any]) -> None:
        line = (json.dumps(message, separators=(",", ":")) + "\n").encode()
        if self._socket is not None:
            self._socket.sendall(line)
        else:
            assert self._pipe is not None
            self._pipe.write(line)

    def read(self, timeout: float | None = None) -> dict[str, Any] | None:
        """The next message; ``None`` if ``timeout`` passed first. Raises at the end of stream."""
        while b"\n" not in self._buffer:
            chunk = self._receive(timeout)
            if chunk is None:
                return None
            if not chunk:
                raise ConnectionError("the brumm daemon closed the connection")
            self._buffer += chunk
        line, _, rest = bytes(self._buffer).partition(b"\n")
        self._buffer = bytearray(rest)
        return json.loads(line)

    def _receive(self, timeout: float | None) -> bytes | None:
        if self._socket is None:
            assert self._pipe is not None
            return self._pipe.read(RECV_BYTES)
        if timeout is not None:
            ready, _, _ = select.select([self._socket], [], [], max(timeout, 0))
            if not ready:
                return None
        return self._socket.recv(RECV_BYTES)
