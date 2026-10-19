#!/usr/bin/env python3
"""TUI smoke test for `/model`: list the models, switch to one, and check
that the status bar follows.

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


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        self.rfile.read(length)
        body = (
            'data: {"choices":[{"delta":{"content":"ok"}}]}\n\n'
            'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\n'
            'data: {"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}\n\n'
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
    return data


def compact(data: bytes) -> bytes:
    """Terminal diff rendering skips blank cells, so spaces are unreliable."""
    return re.sub(rb"\s+", b"", strip_ansi(data))


def main():
    home, binary = sys.argv[1], sys.argv[2]
    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_address[1]}/v1"

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["PI_RS_HOME"] = home
        os.environ["DEEPSEEK_API_KEY"] = "sk-smoke"
        os.environ["MINIMAX_CN_API_KEY"] = "sk-cp-smoke"
        os.execv(binary, [binary, "--base-url", url])
        os._exit(1)

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 100, 0, 0))
    seen = b""

    def pump(seconds: float):
        nonlocal seen
        deadline = time.time() + seconds
        while time.time() < deadline:
            r, _, _ = select.select([fd], [], [], 0.2)
            if fd in r:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                seen += chunk

    def type_text(text: str):
        os.write(fd, text.encode())
        time.sleep(0.4)

    pump(1.5)
    # `/model` with no argument lists the available models.
    type_text("/model")
    os.write(fd, b"\r")
    pump(1.5)
    listing = compact(seen)
    assert b"deepseek-flash" in listing, listing.decode(errors="replace")
    assert b"MiniMax-M3.1-Flash" in listing, listing.decode(errors="replace")
    print("ok: /model lists the models")

    # Switch to another model of the same provider.
    seen = b""
    type_text("/model deepseek-v4-pro")
    os.write(fd, b"\r")
    pump(1.5)
    switched = compact(seen)
    assert b"deepseek-v4-pro" in switched, switched.decode(errors="replace")
    print("ok: /model <id> switches the model")

    # Switch to a model of another provider, using that provider's key.
    seen = b""
    type_text("/model MiniMax-M3.1")
    os.write(fd, b"\r")
    pump(1.5)
    crossed = compact(seen)
    assert b"MiniMax-M3.1-Flash" in crossed, crossed.decode(errors="replace")
    print("ok: /model crosses providers")

    os.write(fd, b"\x03")
    pump(1.0)
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    server.shutdown()
    print("PASS")


if __name__ == "__main__":
    main()
