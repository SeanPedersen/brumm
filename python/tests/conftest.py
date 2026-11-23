"""Fixtures: a fresh data root per test (under the cargo target dir, not /tmp) with its own
daemon, started by the first client and stopped at the end."""

from __future__ import annotations

import os
import time
from collections.abc import Iterator
from pathlib import Path

import pytest

import brumm

REPO = Path(__file__).resolve().parents[2]
#: Nothing listens here: messages stay local (no Nostr relays in tests).
NO_RELAY = "ws://127.0.0.1:9"


@pytest.fixture(scope="session", autouse=True)
def binary() -> str:
    path = os.environ.get("BRUMM_BIN") or str(REPO / "target" / "debug" / "brumm-cli")
    if not Path(path).is_file():
        pytest.skip(f"build brumm-cli first (cargo build -p brumm-cli) or set BRUMM_BIN; missing {path}")
    os.environ["BRUMM_BIN"] = path
    os.environ["BRUMM_RELAYS"] = NO_RELAY
    return path


@pytest.fixture
def data_dir(request: pytest.FixtureRequest) -> Iterator[Path]:
    root = REPO / "target" / "pytest" / f"{request.node.name}-{time.time_ns()}"
    root.mkdir(parents=True)
    yield root
    try:
        with brumm.Client(data_dir=root) as client:
            client.stop_daemon()
    except brumm.BrummError:
        pass


@pytest.fixture
def client(data_dir: Path) -> Iterator[brumm.Client]:
    with brumm.Client("p", data_dir=data_dir) as client:
        client.init("P")
        yield client
