#!/usr/bin/env python3
"""PTY smoke test for the pi-rs TUI: start it, wait, send Ctrl+C, expect a clean exit."""
import os
import pty
import select
import signal
import sys
import time

pid, fd = pty.fork()
if pid == 0:
    os.environ["PI_RS_HOME"] = sys.argv[1]
    os.environ["DEEPSEEK_API_KEY"] = "fake"
    os.execv(sys.argv[2], [sys.argv[2]])

output = b""
deadline = time.time() + 15
sent = False
status = None
while time.time() < deadline:
    r, _, _ = select.select([fd], [], [], 0.3)
    if r:
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        output += chunk
    if not sent and b"\x1b[?1049h" in output:
        os.write(fd, b"\x03")  # Ctrl+C -> quit (idle, empty input)
        sent = True
    if status is None:
        done, st = os.waitpid(pid, os.WNOHANG)
        if done:
            status = st
    # Reap once, but keep draining until PTY EOF (the session line can lag).

if status is None:
    # EOF can arrive just before process exit. Reap with a bounded grace
    # period; a blocking wait here would defeat the test deadline.
    reap_deadline = min(deadline, time.time() + 1)
    while status is None and time.time() < reap_deadline:
        done, st = os.waitpid(pid, os.WNOHANG)
        if done:
            status = st
        else:
            time.sleep(0.01)

if status is None:
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    print("TIMEOUT: TUI did not exit after Ctrl+C")
    print(output[-500:].decode(errors="replace"))
    sys.exit(1)

code = os.waitstatus_to_exitcode(status)
text = output.decode(errors="replace")
print(f"exit={code}")
if code != 0:
    print(text[-800:])
    sys.exit(1)
# The alternate screen should have been entered and left, and the session
# path printed on exit.
if "session:" not in text:
    print("MISSING session line on exit")
    print(text[-800:])
    sys.exit(1)
if b"\x1b[?1049h" not in output or b"\x1b[?1049l" not in output:
    print("MISSING alternate-screen enter/leave on exit")
    sys.exit(1)
print("TUI smoke test ok")
