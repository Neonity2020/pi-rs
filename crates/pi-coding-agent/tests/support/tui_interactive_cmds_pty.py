import fcntl
import json
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time
from pathlib import Path

binary, root_arg = sys.argv[1:]
root = Path(root_arg)


def spawn(base, cwd):
    cwd.mkdir(parents=True, exist_ok=True)
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
    env = os.environ.copy()
    env.update({"PI_RS_AGENT_DIR": str(base / "agent"), "TERM": "xterm-256color"})
    env.pop("PI_RS_SESSION_DIR", None)
    process = subprocess.Popen([binary, "--no-mcp"], cwd=cwd, env=env,
                               stdin=slave, stdout=slave, stderr=slave,
                               start_new_session=True)
    os.close(slave)
    return master, process


ANSI = re.compile(rb"\x1b\[[0-9;?]*[a-zA-Z]")


def decoded(captured):
    """The byte stream with escape sequences replaced by spaces: ratatui's
    diff renderer skips unchanged cells (the blanks between words), so a
    cursor move stands in for the whitespace it did not re-emit."""
    return ANSI.sub(b" ", bytes(captured))


def wait_for(captured, process, master, needle):
    deadline = time.monotonic() + 15
    target = needle.encode() if isinstance(needle, str) else needle
    # Nudging the window size forces a full repaint so the needle becomes
    # visible even when a cursor move stood in for its whitespace.
    last_nudge = time.monotonic()
    nudge = False
    while time.monotonic() < deadline:
        if target in decoded(captured):
            return
        if process.poll() is not None:
            break
        if time.monotonic() - last_nudge > 1.5:
            last_nudge = time.monotonic()
            nudge = not nudge
            rows = 40 + (1 if nudge else 0)
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, 160, 0, 0))
        if select.select([master], [], [], 0.1)[0]:
            try:
                captured.extend(os.read(master, 65536))
            except OSError:
                break
    dump = os.environ.get("TUI_PTY_DEBUG_DUMP")
    if dump:
        with open(dump, "wb") as handle:
            handle.write(bytes(captured))
    raise AssertionError(f"did not see {needle!r}; output: {bytes(captured[-4000:])!r}")


def send(master, captured, text):
    captured.clear()
    os.write(master, text.encode())


def quit_cleanly(master, captured, process):
    os.write(master, b"\x03")
    # Keep draining the master while waiting: a pending full repaint (resize
    # nudges force one for the big doc blocks) can fill the pty output
    # buffer, and an app blocked in write never gets to the ctrl+c.
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if process.poll() is not None:
            break
        if select.select([master], [], [], 0.1)[0]:
            try:
                captured.extend(os.read(master, 65536))
            except OSError:
                break
    assert process.poll() is not None, "pi-rs did not exit after ctrl+c"
    assert process.returncode == 0


# --- /copy with no agent messages shows the upstream error text -------------
base = root / "copy-empty"
project = base / "project"
master, process = spawn(base, project)
captured = bytearray()
try:
    wait_for(captured, process, master, "type /help")
    send(master, captured, "/copy\r")
    wait_for(captured, process, master, "No agent messages to copy yet.")
    quit_cleanly(master, captured, process)
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)

# --- /changelog renders the What's New block from the repo CHANGELOG --------
base = root / "changelog"
project = base / "project"
master, process = spawn(base, project)
captured = bytearray()
try:
    wait_for(captured, process, master, "type /help")
    send(master, captured, "/changelog\r")
    wait_for(captured, process, master, "What's New")
    # Content comes from the embedded pi-rs CHANGELOG.md (round notes).
    wait_for(captured, process, master, "Session tree surface")
    send(master, captured, "/help\r")
    wait_for(captured, process, master, "Show changelog entries")
    quit_cleanly(master, captured, process)
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)

# --- /trust opens the decision selector and writes the store ----------------
base = root / "trust"
project = base / "project"
master, process = spawn(base, project)
captured = bytearray()
try:
    wait_for(captured, process, master, "type /help")
    send(master, captured, "/trust\r")
    wait_for(captured, process, master, "Project trust")
    wait_for(captured, process, master, "Saved decision: none")
    wait_for(captured, process, master, "Current session: untrusted")
    # Trust → Trust parent folder → Do not trust; Enter saves the decision.
    send(master, captured, "\x1b[B")
    send(master, captured, "\x1b[B")
    time.sleep(0.2)
    send(master, captured, "\r")
    wait_for(captured, process, master,
             "Saved trust decision: untrusted. Restart pi-rs for this to take effect.")
    store = base / "agent" / "trust.json"
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline and not store.is_file():
        time.sleep(0.05)
    assert store.is_file(), "the trust store was not written"
    body = store.read_text()
    assert ": false" in body, body
    assert str(project) in body, body
    quit_cleanly(master, captured, process)
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)
