import fcntl
import json
import os
import pty
import select
import shutil
import struct
import subprocess
import sys
import termios
import time
from pathlib import Path

binary, root_arg = sys.argv[1:]
root = Path(root_arg)
server = root / "server.py"
server.write_text("""import json, sys, time
from pathlib import Path
Path(sys.argv[1]).write_text('started')
for line in sys.stdin:
    message = json.loads(line)
    if 'id' not in message: continue
    method = message.get('method')
    if method == 'initialize': time.sleep(1)
    result = {'protocolVersion':'2025-06-18','capabilities':{},'serverInfo':{'name':'test','version':'1'}} if method == 'initialize' else {'tools': []}
    print(json.dumps({'jsonrpc':'2.0','id':message['id'],'result':result}), flush=True)
""")

for choice in ("y", "n", "escape", "disabled", "missing"):
    base = root / choice
    source = base / "source"
    target = base / "target-project"
    agent = base / "agent"
    session = base / "saved-session"
    for directory in (source, target / ".pi", agent, session):
        directory.mkdir(parents=True)
    marker = target / "started"
    (target / ".pi" / "mcp.json").write_text(json.dumps({"mcpServers": {"test": {
        "command": "python3", "args": [str(server), str(marker)]
    }}}))
    (target / ".pi" / "settings.json").write_text(json.dumps({"defaultTools": ["grep"]}))
    header = {"type": "session", "version": 3, "id": "test", "timestamp": "2026-01-01T00:00:00Z", "cwd": str(target)}
    (session / "session.jsonl").write_text(json.dumps(header) + "\n")
    if choice == "missing":
        shutil.rmtree(target)
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 160, 0, 0))
    env = os.environ.copy()
    env.update({"PI_RS_AGENT_DIR": str(agent), "TERM": "xterm-256color"})
    env.pop("PI_RS_SESSION_DIR", None)
    args = [binary] + (["--no-mcp"] if choice == "disabled" else [])
    process = subprocess.Popen(args, cwd=source, env=env, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
    os.close(slave)
    captured = bytearray()

    def wait_for(needle):
        deadline = time.monotonic() + 10
        # ratatui's diff renderer skips unchanged cells, so a needle can be on
        # screen while its characters are absent from the byte stream. Nudging
        # the window size forces a full repaint so the screen is re-emitted.
        last_nudge = time.monotonic()
        nudge = False
        while time.monotonic() < deadline:
            if needle in captured:
                return
            if process.poll() is not None:
                break
            if time.monotonic() - last_nudge > 1.5:
                last_nudge = time.monotonic()
                nudge = not nudge
                rows = 32 + (1 if nudge else 0)
                fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, 160, 0, 0))
            if select.select([master], [], [], 0.1)[0]:
                try:
                    captured.extend(os.read(master, 65536))
                except OSError:
                    # Kernel 7.0 raises transient OSErrors on master reads while

                    # the child still runs; keep polling — the deadline and the

                    # process poll own the exit decision (pty harness port).

                    time.sleep(0.05)

                    continue

        raise AssertionError(f"{choice}: did not see {needle!r}; output: {bytes(captured[-5000:])!r}")

    def wait_for_switch():
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if len((session / "session.jsonl").read_text().splitlines()) > 1:
                return
            if select.select([master], [], [], 0.1)[0]:
                captured.extend(os.read(master, 65536))
        raise AssertionError(f"{choice}: target session was not opened")

    try:
        wait_for(b"type /help")
        os.write(master, ("/resume " + str(session) + "\r").encode())
        if choice not in ("disabled", "missing"):
            wait_for(b"Project trust")
            captured.clear()
            os.write(master, {"y": b"y", "n": b"n", "escape": b"\x1b"}[choice])
        if choice == "y":
            os.write(master, b"/tools\r")
            wait_for(b"Session switch in progress")
            wait_for_switch()
            deadline = time.monotonic() + 5
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert marker.exists(), "trusted MCP server did not start"
            decisions = json.loads((agent / "trust.json").read_text())
            assert decisions[str(target.resolve())] is True
        elif choice in ("n", "disabled"):
            wait_for_switch()
            assert not marker.exists(), "untrusted MCP server started"
            assert not (agent / "trust.json").exists()
            if choice == "disabled":
                assert b"Project trust" not in captured
        elif choice == "escape":
            wait_for(b"Session switch cancelled")
            assert not marker.exists()
            assert not (agent / "trust.json").exists()
            assert len((session / "session.jsonl").read_text().splitlines()) == 1
        else:
            wait_for(b"Session switch failed")
        if choice not in ("escape", "missing"):
            wait_for("session →".encode())
            if choice == "y":
                wait_for(b"prompt ")
            captured.clear()
            os.write(master, b"\r" if choice == "y" else b"/tools\r")
            wait_for(b"active tools: grep")
        elif choice == "missing":
            captured.clear()
            os.write(master, b"/tools\r")
            wait_for(b"active tools: read, write, edit, bash")
        os.write(master, b"\x03")
        process.wait(timeout=10)
        assert process.returncode == 0, f"{choice}: exit {process.returncode}"
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)

# The oldest session must remain searchable after more than 100 newer ones.
choice = "picker"
base = root / choice
source = base / "source"
agent = base / "agent"
source.mkdir(parents=True)
for index in range(105):
    session_dir = agent / "sessions" / f"session-{index:03}"
    session_dir.mkdir(parents=True)
    path = session_dir / "session.jsonl"
    header = {"type":"session", "version":3, "id":f"id-{index}", "timestamp":"2026-01-01T00:00:00Z", "cwd":str(source)}
    first = {"type":"message", "id":"m1", "parentId":None, "timestamp":"2026-01-01T00:00:01Z", "message":{"role":"user", "content":"oldest hidden target" if index == 0 else f"ordinary session {index}", "timestamp":1}}
    second = {"type":"message", "id":"m2", "parentId":"m1", "timestamp":"2026-01-01T00:00:02Z", "message":{"role":"user", "content":"x" * 300 + " buried-needle" if index == 0 else "ordinary", "timestamp":2}}
    path.write_text("\n".join(map(json.dumps, (header, first, second))) + "\n")
    os.utime(path, (index + 1, index + 1))
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 160, 0, 0))
env = os.environ.copy()
env.update({"PI_RS_AGENT_DIR": str(agent), "TERM": "xterm-256color"})
env.pop("PI_RS_SESSION_DIR", None)
process = subprocess.Popen([binary, "--no-mcp"], cwd=source, env=env, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
os.close(slave)
captured = bytearray()
try:
    wait_for(b"type /help")
    os.write(master, b"/resume\r")
    wait_for(b"Resume session")
    captured.clear()
    os.write(master, b"buried-needle")
    wait_for(b"oldest hidden target")
    captured.clear()
    os.write(master, b"\r")
    wait_for("session →".encode())
    assert b"session-000/session.jsonl" in captured
    os.write(master, b"\x03")
    process.wait(timeout=10)
    assert process.returncode == 0
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)
