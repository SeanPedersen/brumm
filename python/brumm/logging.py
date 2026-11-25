"""``BrummHandler``: Python logging into a brumm room (e.g. ``me``, the bot's owner).

Records are batched: a background thread sends at most one message per ``interval`` seconds
(all records since the last one, cut to ``max_chars``), and at most ``max_per_minute``
messages a minute. What does not fit is counted and reported in the next message, so a log
storm becomes a few messages, never a flood. ``emit`` never blocks on the network.
"""

from __future__ import annotations

import logging
import queue
import threading
import time
from typing import Any

from .client import Client

DEFAULT_INTERVAL_SECONDS = 2.0
DEFAULT_MAX_PER_MINUTE = 10
#: Well below the 56 KiB message limit, so one message stays readable on a phone.
DEFAULT_MAX_CHARS = 4000
DEFAULT_QUEUE_RECORDS = 10_000
SECONDS_PER_MINUTE = 60.0


class BrummHandler(logging.Handler):
    def __init__(
        self,
        room: str = "me",
        *,
        client: Client | None = None,
        profile: str = "default",
        level: int = logging.NOTSET,
        interval: float = DEFAULT_INTERVAL_SECONDS,
        max_per_minute: int = DEFAULT_MAX_PER_MINUTE,
        max_chars: int = DEFAULT_MAX_CHARS,
        **client_options: Any,
    ) -> None:
        super().__init__(level)
        self.room = room
        self.interval = interval
        self.max_per_minute = max_per_minute
        self.max_chars = max_chars
        self._client = client or Client(profile, **client_options)
        self._owns_client = client is None
        self._records: queue.Queue[str] = queue.Queue(DEFAULT_QUEUE_RECORDS)
        self._dropped = 0
        self._sent_at: list[float] = []
        self._stop = threading.Event()
        self._worker = threading.Thread(target=self._run, name="brumm-log", daemon=True)
        self._worker.start()

    def emit(self, record: logging.LogRecord) -> None:
        try:
            line = self.format(record)
        except Exception:  # noqa: BLE001 - logging must never raise into the caller
            self.handleError(record)
            return
        try:
            self._records.put_nowait(line)
        except queue.Full:
            self._dropped += 1

    def flush(self) -> None:
        self._send_batch()

    def close(self) -> None:
        self._stop.set()
        self._worker.join(timeout=self.interval + 5)
        self._send_batch()
        if self._owns_client:
            self._client.close()
        super().close()

    def _run(self) -> None:
        while not self._stop.wait(self.interval):
            self._send_batch()

    def _allowed_now(self) -> bool:
        now = time.monotonic()
        self._sent_at = [at for at in self._sent_at if now - at < SECONDS_PER_MINUTE]
        return len(self._sent_at) < self.max_per_minute

    def _send_batch(self) -> None:
        if self._records.empty() and not self._dropped:
            return
        if not self._allowed_now():
            return  # the records wait in the queue; past its size they are counted as dropped
        lines: list[str] = []
        size = 0
        while True:
            try:
                line = self._records.get_nowait()
            except queue.Empty:
                break
            if size + len(line) + 1 > self.max_chars:
                self._dropped += 1
                continue
            lines.append(line)
            size += len(line) + 1
        if self._dropped:
            lines.append(f"… {self._dropped} more log records were dropped (rate limit)")
        if not lines:
            return
        self._sent_at.append(time.monotonic())
        try:
            self._client.send(self.room, "\n".join(lines), wait=None)
            self._dropped = 0
        except Exception:  # noqa: BLE001 - a broken log channel must not break the program
            self._dropped += len(lines)
