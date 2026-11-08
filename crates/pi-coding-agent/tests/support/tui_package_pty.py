"""Package resource startup smoke through a real PTY.

Installs a local package (settings `packages` entry) whose extension
registers a `/pkg-hello` command, starts pi-rs, and asserts the command runs
(package resources reach the extension runner). Then `/package list` output
is exercised through the CLI, not the TUI — here only the extension wiring is
under test.
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
pkg = root / "pkg"
(pkg / "extensions").mkdir(parents=True, exist_ok=True)
(pkg / "package.json").write_text(
    '{"name": "pty-pkg", "version": "1.0.0", "pi": {"extensions": ["hello.ts"]}}\n'
)
(pkg / "extensions" / "hello.ts").write_text(
    'export default function (pi) {\n'
    '\tpi.registerCommand("pkg-hello", {\n'
    '\t\tdescription: "package hello",\n'
    '\t\thandler: async (args, ctx) => { await ctx.ui.notify("package hello ok", "info"); },\n'
    '\t});\n'
    '}\n'
)
(agent / "settings.json").write_text(
    '{"packages": ["' + str(pkg).replace("\\", "\\\\") + '"]}\n'
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
                time.sleep(0.05)
                continue
    raise AssertionError(f"did not see {needle!r}; tail: {bytes(captured[-4000:])!r}")

captured = bytearray()
try:
    wait_for(captured, "type /help")
    captured.clear()
    os.write(master, "/pkg-hello\r".encode())
    wait_for(captured, "package hello ok")
    os.write(master, b"\x03")
    process.wait(timeout=10)
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)
