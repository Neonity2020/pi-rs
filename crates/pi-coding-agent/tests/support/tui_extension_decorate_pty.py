"""Interactive ui.* decoration round trip through a real PTY (round 43).

Runs pi-rs with the decorations extension, types `/decorate`, and asserts the
three fire-and-forget calls land: the footer grows a status line ("watching
files"), the editor buffer is replaced ("prefilled text"), and the terminal
window title is set via OSC 0 ("pi-ext-title").
"""
import fcntl
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

binary, root_arg, extension = sys.argv[1:4]
root = Path(root_arg)
project = root / "project"
project.mkdir(parents=True, exist_ok=True)

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
env = os.environ.copy()
env.update({"PI_RS_AGENT_DIR": str(root / "agent"), "TERM": "xterm-256color"})
env.pop("PI_RS_SESSION_DIR", None)
process = subprocess.Popen(
    [binary, "--no-mcp", "-e", extension],
    cwd=project, env=env, stdin=slave, stdout=slave, stderr=slave,
    start_new_session=True)
os.close(slave)

# Strip CSI sequences only: the OSC-0 window title (\x1b]0;...\x07) must
# survive decoding for the title assertion.
ANSI = re.compile(rb"\x1b\[[0-9;?]*[a-zA-Z]")

def decoded(captured):
    return ANSI.sub(b" ", bytes(captured))

def wait_for(captured, needle, deadline_seconds=30):
    end = time.monotonic() + deadline_seconds
    target = needle.encode()
    last_nudge = time.monotonic()
    nudge = False
    while time.monotonic() < end:
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
                # Kernel 7.0 raises transient OSErrors on master reads while

                # the child still runs; keep polling — the deadline and the

                # process poll own the exit decision (pty harness port).

                time.sleep(0.05)

                continue

    raise AssertionError(f"did not see {needle!r}; tail: {bytes(captured[-4000:])!r}")

captured = bytearray()
try:
    wait_for(captured, "type /help")
    captured.clear()
    os.write(master, "/decorate\r".encode())
    # The status text joins the footer as a third line.
    wait_for(captured, "watching files")
    # The editor buffer was replaced.
    wait_for(captured, "prefilled text")
    # The OSC-0 window title survived the CSI-only strip.
    assert b"pi-ext-title" in decoded(captured), "window title never set"
    os.write(master, b"\x03")
    process.wait(timeout=10)
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)
