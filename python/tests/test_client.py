"""The Python package against a real daemon."""

from __future__ import annotations

import asyncio
import logging
import subprocess
import sys
import threading
from pathlib import Path

import pytest

import brumm
import brumm.aio
from brumm.logging import BrummHandler

SHARED_DAEMON_SCRIPT = """
import sys, brumm
with brumm.Client("p", data_dir=sys.argv[1]) as client:
    client.send("notes", f"from process {sys.argv[2]}", wait=None)
    print(client.daemon_status()["pid"])
"""


def test_send_wait_and_read(client: brumm.Client) -> None:
    sent = client.send("notes", "hello from python", wait="delivered")
    assert sent.status == "delivered"
    assert client.wait("notes", sent.id) == "delivered"
    page = client.read("notes")
    assert [message.text for message in page.messages] == ["hello from python"]
    assert page.messages[0].cursor == page.cursor
    assert client.identity().name == "P"
    assert [room.kind for room in client.rooms()] == ["notes"]


def test_a_retry_with_the_same_id_sends_once(client: brumm.Client) -> None:
    message_id = "aaaaaaaaaaaaaaaaaaaaaaaaaa"
    for _ in range(3):
        assert client.send("notes", "once", id=message_id, wait=None).id == message_id
    assert len(client.read("notes").messages) == 1


def test_files_and_errors(client: brumm.Client, tmp_path_factory: pytest.TempPathFactory, data_dir: Path) -> None:
    report = data_dir / "report.txt"
    report.write_text("all green")
    client.send("notes", path=report, wait=None)
    client.send("notes", data=b"\x00\x01", name="blob.bin", wait=None)
    files = [message.file.name for message in client.read("notes").messages if message.file]
    assert files == ["report.txt", "blob.bin"]
    with pytest.raises(brumm.BrummError) as missing:
        client.send("nobody", "hi")
    assert missing.value.code == brumm.errors.NOT_FOUND
    with pytest.raises(ValueError):
        client.send("notes", "text", data=b"and data")


def test_cursor_reads_in_pages(client: brumm.Client) -> None:
    for index in range(5):
        client.send("notes", f"n{index}", wait=None)
    texts, cursor = [], None
    while True:
        page = client.read("notes", since=cursor, limit=2)
        texts += [message.text for message in page.messages]
        cursor = page.cursor
        if not page.has_more:
            break
    assert texts == [f"n{index}" for index in range(5)]
    # Resuming from a message's own cursor continues right after it.
    first = client.read("notes", limit=1).messages[0]
    assert client.read("notes", since=first.cursor, limit=1).messages[0].text == "n1"


def test_iter_messages_sees_messages_from_another_client(client: brumm.Client, data_dir: Path) -> None:
    # Take the cursor first: whatever the writer sends after it is seen, however fast.
    cursor = client.read("notes", last=0).cursor

    def write() -> None:
        with brumm.Client("p", data_dir=data_dir) as other:
            for index in range(3):
                other.send("notes", f"live {index}", wait=None)

    writer = threading.Thread(target=write)
    writer.start()
    seen = []
    for message in client.iter_messages(["notes"], since=cursor, incoming_only=False, timeout=10):
        seen.append(message.text)
        if len(seen) == 3:
            break
    writer.join()
    assert seen == ["live 0", "live 1", "live 2"]


def test_two_processes_share_one_daemon(client: brumm.Client, data_dir: Path) -> None:
    runs = [
        subprocess.run(
            [sys.executable, "-c", SHARED_DAEMON_SCRIPT, str(data_dir), str(index)],
            capture_output=True,
            text=True,
            check=True,
            timeout=60,
        )
        for index in range(2)
    ]
    pids = {int(run.stdout.strip()) for run in runs}
    assert pids == {client.daemon_status()["pid"]}
    texts = sorted(message.text for message in client.read("notes").messages)
    assert texts == ["from process 0", "from process 1"]


def test_asyncio_client(client: brumm.Client, data_dir: Path) -> None:
    async def scenario() -> list[str]:
        async with await brumm.aio.connect("p", data_dir=data_dir) as aio:
            stream = aio.messages(["notes"], incoming_only=False, timeout=10)
            sender = asyncio.create_task(send_later(aio))
            seen = []
            async for message in stream:
                seen.append(message.text)
                if len(seen) == 2:
                    break
            await sender
            page = await aio.read("notes", last=1)
            assert page.messages[0].text in {"async 0", "async 1"}
            return seen

    async def send_later(aio: brumm.aio.AsyncClient) -> None:
        await asyncio.sleep(0.3)
        await asyncio.gather(*(aio.send("notes", f"async {index}", wait=None) for index in range(2)))

    seen = asyncio.run(scenario())
    assert sorted(seen) == ["async 0", "async 1"]


def test_logging_handler_batches_a_log_storm(client: brumm.Client) -> None:
    records = 500
    handler = BrummHandler("notes", client=client, interval=0.2, max_per_minute=3, max_chars=1000)
    logger = logging.getLogger("brumm-test-storm")
    logger.addHandler(handler)
    logger.setLevel(logging.INFO)
    for index in range(records):
        logger.info("record %d", index)
    handler.close()
    logger.removeHandler(handler)
    messages = client.read("notes").messages
    assert 1 <= len(messages) <= 3
    assert "record 0" in messages[0].text
    assert any("dropped" in message.text for message in messages)
    assert all(len(message.text) <= 1100 for message in messages)
