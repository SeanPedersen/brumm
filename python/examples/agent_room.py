"""Use case A: a coordinator agent in a project's agent room.

Each agent is its own profile (identity) and a member of a group you own. This coordinator
hands out tasks from its queue to whoever asks with "next", and acknowledges "done <task>":

    brumm-cli --profile coordinator init Coordinator
    brumm-cli --profile coordinator accept 'brumm://invite/…' --lock   # paired with you
    # in the app: create a group "agents" and invite Coordinator (accepted automatically)
    uv run examples/agent_room.py agents
"""

import sys

import brumm

ROOM = sys.argv[1] if len(sys.argv) > 1 else "agents"
tasks = ["write the changelog", "bump the version", "run the release checklist"]

with brumm.Client("coordinator") as client:
    client.send(ROOM, f"coordinator online, {len(tasks)} tasks queued. Say 'next' to take one.")
    for message in client.iter_messages([ROOM]):
        who = message.sender_name or "someone"
        words = message.text.strip().split(maxsplit=1)
        if not words:
            continue
        if words[0].lower() == "next":
            reply = f"{who}: {tasks.pop(0)}" if tasks else f"{who}: nothing left, thanks!"
            client.send(ROOM, reply, wait=None)
        elif words[0].lower() == "done" and len(words) == 2:
            client.send(ROOM, f"✓ {words[1]} ({who})", wait=None)
