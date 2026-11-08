"""`/mcp` slash command smoke through a real PTY.

Runs pi-rs with an agent dir holding one disabled OAuth server, types `/mcp`
(status: disabled), then `/mcp logout docs` (no stored credentials), and
quits. Asserts through decoded frame text like the other TUI smokes.
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

binary, root_arg = sys.argv[1:3]
root = Path(root_arg)
project = root / "project"
project.mkdir(parents=True, exist_ok=True)
agent = root / "agent"
agent.mkdir(parents=True, exist_ok=True)
# Enabled but unroutable: startup refuses fast; the status shows the
# failure and login/logout run their real candidate paths.
(agent / "mcp.json").write_text(
    '{"mcpServers": {"docs": {"url": "http://127.0.0.1:1/mcp"}}}\n'
)

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
env = os.environ.copy()
env.update({"PI_RS_AGENT_DIR": str(agent), "TERM": "xterm-256color"})
env.pop("PI_RS_SESSION_DIR", None)
process = subprocess.Popen(
    [binary],
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
    captured.clear()
    os.write(master, "/mcp\r".encode())
    wait_for(captured, "MCP servers:")
    wait_for(captured, "docs: failed")
    captured.clear()
    os.write(master, "/mcp login docs\r".encode())
    wait_for(captured, "failed to connect")
    captured.clear()
    os.write(master, "/mcp logout docs\r".encode())
    wait_for(captured, "No stored credentials for MCP server")
    os.write(master, b"\x03")
    process.wait(timeout=10)
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)
