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


def write_session(directory, cwd, messages):
    """A minimal v3 session: header + user message entries."""
    directory.mkdir(parents=True, exist_ok=True)
    lines = [json.dumps({"type": "session", "version": 3, "id": "saved",
                         "timestamp": "2026-01-01T00:00:00Z", "cwd": str(cwd)})]
    parent = None
    for index, text in enumerate(messages):
        entry = {"type": "message", "id": f"m{index}", "parentId": parent,
                 "timestamp": f"2026-01-01T00:00:{index + 1:02d}Z",
                 "message": {"role": "user", "content": text, "timestamp": index + 1}}
        lines.append(json.dumps(entry))
        parent = f"m{index}"
    (directory / "session.jsonl").write_text("\n".join(lines) + "\n")
    return directory / "session.jsonl"


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
    # ratatui's diff renderer skips unchanged cells, so a needle can be on
    # screen while its characters are absent from the byte stream (a cursor
    # move stands in for them). Nudging the window size forces a full repaint
    # so the whole screen is re-emitted and the needle becomes visible.
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


def resume_and_settle(master, captured, process, session_dir):
    """Resume a session and wait until the runtime replacement has fully
    settled: the editor title flips to "switching session…" while the
    replacement is pending and back to "prompt " once it completes."""
    captured.clear()
    os.write(master, ("/resume " + str(session_dir) + "\r").encode())
    wait_for(captured, process, master, "switching session")
    wait_for(captured, process, master, "prompt ")
    captured.clear()


# --- /clone: the branched copy lands next to the source with the lineage ----
base = root / "clone"
project = base / "project"
saved_file = write_session(base / "saved", project, ["clonable question"])
master, process = spawn(base, project)
captured = bytearray()
try:
    wait_for(captured, process, master, "type /help")
    resume_and_settle(master, captured, process, saved_file.parent)
    send(master, captured, "/clone\r")
    wait_for(captured, process, master, "Cloned to new session")
    deadline = time.monotonic() + 10
    clone = None
    while time.monotonic() < deadline and clone is None:
        for directory in saved_file.parent.parent.iterdir():
            if directory == saved_file.parent or not directory.is_dir():
                continue
            log = directory / "session.jsonl"
            if not log.is_file():
                continue
            header = json.loads(log.read_text().splitlines()[0])
            if header.get("parentSession") == str(saved_file):
                clone = log
                break
        time.sleep(0.05)
    assert clone is not None, "no branched copy with the parent lineage appeared"
    body = clone.read_text()
    assert "clonable question" in body, body
    os.write(master, b"\x03")
    process.wait(timeout=10)
    assert process.returncode == 0
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)

# --- /fork: branching from the first user message starts a fresh session ---
base = root / "fork"
project = base / "project"
saved_file = write_session(base / "saved", project, ["forkable question"])
master, process = spawn(base, project)
captured = bytearray()
try:
    wait_for(captured, process, master, "type /help")
    resume_and_settle(master, captured, process, saved_file.parent)
    send(master, captured, "/fork\r")
    wait_for(captured, process, master, "Fork from message")
    wait_for(captured, process, master, "forkable question")
    send(master, captured, "\r")
    wait_for(captured, process, master, "Forked to new session")
    deadline = time.monotonic() + 10
    forked = None
    while time.monotonic() < deadline and forked is None:
        for directory in saved_file.parent.parent.iterdir():
            if directory == saved_file.parent or not directory.is_dir():
                continue
            log = directory / "session.jsonl"
            if not log.is_file():
                continue
            header = json.loads(log.read_text().splitlines()[0])
            if header.get("parentSession") == str(saved_file):
                forked = log
                break
        time.sleep(0.05)
    assert forked is not None, "no fork target with the parent lineage appeared"
    # Fork before the first message: a fresh session, no replayed entries.
    body = forked.read_text().splitlines()
    assert not any('"role":"user"' in line for line in body[1:]), body
    os.write(master, b"\x03")
    process.wait(timeout=10)
    assert process.returncode == 0
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)

# --- /tree: navigating without a summary moves to the selected point -------
base = root / "tree"
project = base / "project"
saved_file = write_session(base / "saved", project, ["first question", "second question"])
master, process = spawn(base, project)
captured = bytearray()
try:
    wait_for(captured, process, master, "type /help")
    resume_and_settle(master, captured, process, saved_file.parent)
    send(master, captured, "/tree\r")
    wait_for(captured, process, master, "Session tree")
    wait_for(captured, process, master, "second question")
    send(master, captured, "\x1b[A")  # up: the first user message (non-leaf)
    send(master, captured, "\r")      # confirm → "Summarize branch?"
    wait_for(captured, process, master, "Summarize branch?")
    send(master, captured, "\r")      # "No summary"
    wait_for(captured, process, master, "Navigated to selected point")
    os.write(master, b"\x03")
    process.wait(timeout=10)
    assert process.returncode == 0
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)

# --- /import: confirm, copy into the storage, and switch -------------------
base = root / "import"
project = base / "project"
source = base / "incoming.jsonl"
write_session(base, project, ["imported question"])
# (write_session made base/session.jsonl; move it to the standalone name)
(base / "session.jsonl").rename(source)
master, process = spawn(base, project)
captured = bytearray()
try:
    wait_for(captured, process, master, "type /help")
    send(master, captured, "/import\r")
    wait_for(captured, process, master, "Usage: /import <path.jsonl>")
    send(master, captured, "/import " + str(source) + "\r")
    wait_for(captured, process, master, "Replace current session with")
    send(master, captured, "y\r")
    wait_for(captured, process, master, "Session imported from:")
    wait_for(captured, process, master, "imported question")
    # The copy lives in the sessions storage as its own directory.
    deadline = time.monotonic() + 10
    imported = None
    while time.monotonic() < deadline and imported is None:
        for directory in (base / "agent" / "sessions").glob("*"):
            log = directory / "session.jsonl"
            if log.is_file() and "imported question" in log.read_text():
                imported = log
                break
        time.sleep(0.05)
    assert imported is not None, "the imported log was not copied into the storage"
    os.write(master, b"\x03")
    process.wait(timeout=10)
    assert process.returncode == 0
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)

# --- /export: a .jsonl argument writes the raw branch ----------------------
base = root / "export"
project = base / "project"
write_session(base / "saved", project, ["exportable question"])
target = base / "out" / "branch.jsonl"
master, process = spawn(base, project)
captured = bytearray()
try:
    wait_for(captured, process, master, "type /help")
    resume_and_settle(master, captured, process, base / "saved")
    send(master, captured, "/export " + str(target) + "\r")
    wait_for(captured, process, master, "Session exported to:")
    assert target.is_file(), "the export target exists"
    lines = target.read_text().splitlines()
    assert json.loads(lines[0])["type"] == "session"
    assert any("exportable question" in line for line in lines[1:]), lines
    os.write(master, b"\x03")
    process.wait(timeout=10)
    assert process.returncode == 0
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)
