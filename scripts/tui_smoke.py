#!/usr/bin/env python3
"""PTY smoke test for the pi TUI: start it, wait, send Ctrl+C, expect a clean exit."""
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
    if not sent and time.time() > deadline - 13:
        os.write(fd, b"\x03")  # Ctrl+C -> quit (idle, empty input)
        sent = True
    done, st = os.waitpid(pid, os.WNOHANG)
    if done:
        status = st
        break

if status is None:
    # The pty hit EOF (child exited and closed it): collect the status.
    try:
        _, status = os.waitpid(pid, 0)
    except ChildProcessError:
        status = None

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
print("TUI smoke test ok")
