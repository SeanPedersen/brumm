"""Find the ``brumm-cli`` binary and make sure its daemon runs.

Starting is delegated to ``brumm-cli daemon start``: it connects to a running daemon or starts
one (race-safe through the daemon lock), so many imports and processes share one daemon.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import sysconfig
from dataclasses import dataclass, field
from pathlib import Path

from .errors import BrummError

BINARY_NAME = "brumm-cli.exe" if sys.platform == "win32" else "brumm-cli"
BINARY_ENV = "BRUMM_BIN"
START_TIMEOUT_SECONDS = 30.0


@dataclass(frozen=True)
class Settings:
    """Where the daemon lives; ``None`` fields use brumm-cli's defaults (and its env vars)."""

    data_dir: str | os.PathLike[str] | None = None
    socket: str | os.PathLike[str] | None = None
    relays: list[str] = field(default_factory=list)
    binary: str | os.PathLike[str] | None = None

    def global_args(self) -> list[str]:
        args: list[str] = []
        if self.data_dir is not None:
            args += ["--data-dir", os.fspath(self.data_dir)]
        if self.socket is not None:
            args += ["--socket", os.fspath(self.socket)]
        for relay in self.relays:
            args += ["--relay", relay]
        return args


def find_binary(explicit: str | os.PathLike[str] | None = None) -> str:
    """``explicit``, then ``$BRUMM_BIN``, then the one the wheel installed, then ``PATH``."""
    candidates = [explicit, os.environ.get(BINARY_ENV)]
    scripts = sysconfig.get_path("scripts")
    if scripts:
        candidates.append(Path(scripts) / BINARY_NAME)
    candidates.append(shutil.which(BINARY_NAME))
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return os.fspath(candidate)
    raise BrummError(-32000, f"{BINARY_NAME} not found: install the brumm wheel or set {BINARY_ENV}")


def ensure_daemon(settings: Settings) -> str:
    """Starts the daemon unless one runs; returns its socket path."""
    binary = find_binary(settings.binary)
    command = [binary, *settings.global_args(), "--json", "daemon", "start"]
    try:
        done = subprocess.run(
            command,
            capture_output=True,
            text=True,
            timeout=START_TIMEOUT_SECONDS,
            check=False,
            stdin=subprocess.DEVNULL,
        )
    except subprocess.TimeoutExpired as err:
        raise BrummError(-32004, "the brumm daemon did not start in time") from err
    if done.returncode != 0:
        raise BrummError(-32000, f"could not start the brumm daemon: {done.stderr.strip()}")
    status = json.loads(done.stdout.strip().splitlines()[-1])
    return status["socket"]
