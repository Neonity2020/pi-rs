#!/usr/bin/env python3
"""Full TUI smoke test: local mock API + pty; type a prompt, see the answer
rendered, then quit with Ctrl+C.

Needs a real terminal size on the pty, otherwise ratatui draws nothing.
"""
import fcntl
import http.server
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import threading
import time

ANSWER = "Hello from mock"


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        self.rfile.read(length)
        body = (
            'data: {"choices":[{"delta":{"reasoning_content":"secret reasoning"}}]}\n\n'
            f'data: {{"choices":[{{"delta":{{"content":"{ANSWER}"}}}}]}}\n\n'
            'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\n'
            'data: {"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"prompt_cache_hit_tokens":0}}\n\n'
            "data: [DONE]\n\n"
        )
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body.encode())

    def log_message(self, *args):
        pass


def strip_ansi(data: bytes) -> bytes:
    data = re.sub(rb"\x1b\[[0-9;?]*[a-zA-Z]", b"", data)
    data = re.sub(rb"\x1b\][^\x07]*\x07", b"", data)
    data = re.sub(rb"\x1b[()][0-9A-Za-z]", b"", data)
    data = re.sub(rb"\x1b[=>]", b"", data)
    return data


def compact(data: bytes) -> bytes:
    """Terminal diff rendering skips blank cells, so spaces are unreliable."""
    return re.sub(rb"\s+", b"", strip_ansi(data))


def main():
    home, binary = sys.argv[1], sys.argv[2]
    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base_url = f"http://127.0.0.1:{server.server_address[1]}"

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["PI_RS_HOME"] = home
        os.environ["DEEPSEEK_API_KEY"] = "fake"
        os.execv(binary, [binary, "--base-url", base_url])

    # The child starts with a 0x0 pty unless we set a size: ratatui would
    # then render nothing at all.
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))

    output = b""
    start = time.time()
    deadline = start + 25
    sent_input = False
    quit_attempts = 0
    status = None

    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            output += chunk

        now = time.time()
        if not sent_input and now - start > 2:
            os.write(fd, b"hi there\r")
            sent_input = True

        visible = compact(output)
        answer_seen = ANSWER.replace(" ", "").encode() in visible
        if sent_input and quit_attempts < 2 and (answer_seen or now - start > 12):
            if answer_seen:
                time.sleep(0.8)  # let the turn finish and the final frame render
            try:
                os.write(fd, b"\x03")  # interrupt if busy, quit if idle
            except OSError:
                pass
            quit_attempts += 1
            time.sleep(0.5)

        done, st = os.waitpid(pid, os.WNOHANG)
        if done:
            status = st
            break

    if status is None:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        status = 1

    code = os.waitstatus_to_exitcode(status)
    text = strip_ansi(output).decode(errors="replace")
    flat = re.sub(r"\s+", "", text)
    ok = True
    if code != 0:
        print(f"FAIL: exit={code}")
        ok = False
    if ANSWER.replace(" ", "") not in flat:
        print("FAIL: answer never rendered")
        ok = False
    if "hithere" not in flat:
        print("FAIL: user message never rendered")
        ok = False
    if "Thinking" not in flat:
        print("FAIL: collapsed thinking header missing")
        ok = False
    if "secret" in flat:
        print("FAIL: reasoning was not collapsed")
        ok = False
    if "session:" not in text:
        print("FAIL: session line missing on exit")
        ok = False
    if not ok:
        with open("/tmp/pi-tui-chat-dump.txt", "wb") as f:
            f.write(output)
        print("full output dumped to /tmp/pi-tui-chat-dump.txt")
        sys.exit(1)
    print("TUI chat smoke test ok")


if __name__ == "__main__":
    main()
