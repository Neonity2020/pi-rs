"""Interactive extension dialog round trip through a real PTY.

Runs pi-rs with the dialogs extension, types `/ask`, and asserts the
`ui.select` request renders as a TUI dialog (title + options); Enter answers
it through the bridge registry and the handler's notify surfaces the choice
("picked: vanilla").
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
                break
    raise AssertionError(f"did not see {needle!r}; tail: {bytes(captured[-4000:])!r}")

captured = bytearray()
try:
    wait_for(captured, "type /help")
    # The extension command opens the ui.select dialog.
    captured.clear()
    os.write(master, "/ask\r".encode())
    wait_for(captured, "Pick a flavor")
    wait_for(captured, "vanilla")
    # Enter answers with the highlighted option through the bridge registry;
    # the handler's notify lands as a chat notice.
    captured.clear()
    os.write(master, b"\r")
    wait_for(captured, "picked: vanilla")
    os.write(master, b"\x03")
    process.wait(timeout=10)
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)
