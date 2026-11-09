"""`pi-rs config` selector through a real PTY (round 49).

Runs the selector over a fixture agent dir with one local package (an
extension and a skill), asserts the grouped list renders, presses space to
disable the first item (the settings file gains a `-` pattern, the checkbox
row rerenders dim), and Esc exits 0.
"""
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

binary, root_arg = sys.argv[1], sys.argv[2]
root = Path(root_arg)
agent_dir = root / "agent"
project = root / "project"
pkg = root / "fixture-pkg"
for directory in (agent_dir, project, pkg / "extensions", pkg / "skills" / "nested"):
    directory.mkdir(parents=True, exist_ok=True)
Path(pkg / "extensions" / "util.ts").write_text("export default () => {};\n")
Path(pkg / "skills" / "nested" / "SKILL.md").write_text("# nested skill\n")
settings = agent_dir / "settings.json"
settings.write_text(json.dumps({"packages": [str(pkg)]}) + "\n")

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
env = os.environ.copy()
env.update({"PI_RS_AGENT_DIR": str(agent_dir), "TERM": "xterm-256color"})
process = subprocess.Popen(
    [binary, "config"],
    cwd=project, env=env, stdin=slave, stdout=slave, stderr=slave,
    start_new_session=True)
os.close(slave)

ANSI = re.compile(rb"\x1b\[[0-9;?]*[a-zA-Z]")

def decoded(captured):
    return ANSI.sub(b" ", bytes(captured))

def wait_for(captured, needle, deadline_seconds=30):
    end = time.monotonic() + deadline_seconds
    target = needle.encode()
    while time.monotonic() < end:
        if target in decoded(captured):
            return
        if process.poll() is not None:
            break
        if select.select([master], [], [], 0.1)[0]:
            try:
                captured.extend(os.read(master, 65536))
            except OSError:
                # Kernel 7.0 raises transient OSErrors on master reads while
                # the child still runs; keep polling (pty harness port).
                time.sleep(0.05)
                continue
    raise AssertionError(f"did not see {needle!r}; tail: {bytes(captured[-3000:])!r}")

captured = bytearray()
try:
    wait_for(captured, "Global Resources")
    wait_for(captured, "Extensions")
    wait_for(captured, "util.ts")
    wait_for(captured, "nested")  # the SKILL.md display name
    assert b"[ ]" not in decoded(captured), "everything starts enabled"

    # Space disables the selected item (util.ts, the first item): the
    # settings file gains a `-extensions/util.ts` pattern on the package
    # entry and the row rerenders as a dim [ ].
    os.write(master, b" ")
    wait_for(captured, "[ ]")
    deadline = time.monotonic() + 10
    written = False
    while time.monotonic() < deadline and not written:
        if "-extensions/util.ts" in settings.read_text():
            written = True
            break
        time.sleep(0.1)
    assert written, "space never wrote the disable pattern"

    os.write(master, b"\x1b")  # Esc closes the selector
    code = process.wait(timeout=10)
    assert code == 0, f"selector exited {code}"
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)
