#!/usr/bin/env python3
"""Exercise /model and auth through a real PTY and two local HTTP providers.

Every successful switch is followed by a chat request. No real keys/endpoints.
"""
import fcntl
import http.server
import json
import os
import pathlib
import pty
import re
import select
import signal
import struct
import sys
import termios
import threading
import time


def compact(data):
    data = re.sub(rb"\x1b\[[0-9;?]*[a-zA-Z]", b"", data)
    data = re.sub(rb"\x1b\][^\x07]*\x07", b"", data)
    return re.sub(rb"\s+", b"", data)


class Provider:
    def __init__(self, name):
        self.name = name
        self.requests = []
        self.release = threading.Event()
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                owner.requests.append(("GET", self.path, self.headers.get("Authorization"), None))
                ok = self.headers.get("Authorization") != "Bearer rejected"
                body = b'{"data":[]}' if ok else b'{"error":{"message":"bad key"}}'
                self.send_response(200 if ok else 401)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["content-length"])))
                owner.requests.append(("POST", self.path, self.headers.get("Authorization"), body))
                prompt = body["messages"][-1]["content"]
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                if prompt == "hold":
                    self.wfile.write(b'data: {"choices":[{"delta":{"content":"holding"}}]}\n\n')
                    self.wfile.flush()
                    owner.release.wait(15)
                    return
                answer = f"answer-{name}-{len(owner.requests)}"
                data = (
                    "data: " + json.dumps({"choices": [{"delta": {"reasoning_content": "local reasoning", "content": answer}}]}) + "\n\n"
                    'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\n'
                    'data: {"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}\n\n'
                    "data: [DONE]\n\n"
                )
                self.wfile.write(data.encode())

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_port}/v1"

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()


class Terminal:
    def __init__(self, home, binary, args):
        self.output = b""
        self.status = None
        self.eof = False
        self.width = 120
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            for key in ["DEEPSEEK_API_KEY", "MINIMAX_CN_API_KEY", "OPENAI_API_KEY"]:
                # Empty real env entries prevent dotenv from importing real keys.
                os.environ[key] = ""
            os.environ.update(PI_RS_HOME=str(home), TERM="xterm-256color", SHELL="/bin/sh")
            os.execv(binary, [binary] + args)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        self.wait(lambda: b"\x1b[?1049h" in self.output, "alternate screen")

    def pump(self):
        if not self.eof and select.select([self.fd], [], [], 0.05)[0]:
            try:
                chunk = os.read(self.fd, 65536)
                if chunk:
                    self.output += chunk
                else:
                    self.eof = True
            except OSError:
                self.eof = True
        if self.eof:
            time.sleep(0.01)
        if self.status is None:
            done, status = os.waitpid(self.pid, os.WNOHANG)
            if done:
                self.status = status

    def wait(self, predicate, label):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            self.pump()
            if predicate():
                return
            if self.eof and self.status is not None:
                break
        raise AssertionError(f"timed out waiting for {label}")

    def settle(self):
        # Busy frames animate every 120 ms. A quiet interval means the final
        # Busy(false) event has been applied, so Enter will accept a new turn.
        deadline = time.monotonic() + 10
        changed = time.monotonic()
        while time.monotonic() < deadline:
            before = len(self.output)
            self.pump()
            if len(self.output) != before:
                changed = time.monotonic()
            elif time.monotonic() - changed > 0.3:
                return
        raise AssertionError("terminal did not become idle")

    def redraw(self):
        # ratatui emits only changed cells. Resize to request a complete frame
        # before asserting text, rather than treating raw diffs as a screen.
        self.width = 121 if self.width == 120 else 120
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, self.width, 0, 0))

    def command(self, text, expected):
        self.output = b""
        os.write(self.fd, text.encode() + b"\r")
        self.settle()
        self.redraw()
        self.wait(lambda: expected.encode() in compact(self.output), text)
        self.settle()

    def chat(self, provider, other, model, key, prompt):
        before = len(provider.requests)
        other_before = len(other.requests)
        self.output = b""
        os.write(self.fd, prompt.encode() + b"\r")
        self.wait(lambda: len(provider.requests) > before, "chat request")
        method, path, auth, body = provider.requests[-1]
        assert (method, path, auth) == ("POST", "/v1/chat/completions", f"Bearer {key}"), provider.requests[-1]
        assert body["model"] == model, body
        assert len(other.requests) == other_before, "request reached wrong provider"
        self.settle()
        self.redraw()
        expected = f"answer-{provider.name}-{before + 1}".encode()
        self.wait(lambda: expected in compact(self.output), "answer")
        # The completed reasoning header is rendered after the response ends.
        self.wait(lambda: "思考结果".encode() in compact(self.output), "completed response")
        self.settle()
        return body

    def close(self):
        try:
            if self.status is None:
                os.write(self.fd, b"\x03")
            self.wait(lambda: self.status is not None and self.eof, "clean exit and PTY EOF")
            assert os.waitstatus_to_exitcode(self.status) == 0
            assert b"session:" in self.output, "session line missing after EOF"
        finally:
            if self.status is None:
                os.kill(self.pid, signal.SIGKILL)
                _, self.status = os.waitpid(self.pid, 0)
            os.close(self.fd)


def credentials(home, mini):
    home.mkdir(parents=True, exist_ok=True)
    (home / "auth.json").write_text(json.dumps({
        "minimax-cn": {"type": "api_key", "key": "saved-mini", "base_url": mini.url}
    }))


def main():
    home, binary = pathlib.Path(sys.argv[1]), os.path.abspath(sys.argv[2])
    deep, mini = Provider("deep"), Provider("mini")
    terminal = None
    try:
        credentials(home, mini)
        terminal = Terminal(home, binary, ["--base-url", deep.url, "--api-key", "startup-deep"])
        terminal.command("/model", "MiniMax-M3.1-Flash")
        terminal.command("/model deepseek-v4-pro", "model:deepseek-v4-pro")
        terminal.chat(deep, mini, "deepseek-v4-pro", "startup-deep", "before switch")
        terminal.command("/model MiniMax-M3.1", "model:MiniMax-M3.1")
        body = terminal.chat(mini, deep, "MiniMax-M3.1-Flash", "saved-mini", "after switch")
        assert any(m.get("content") == "before switch" for m in body["messages"]), "history lost"
        assert body.get("reasoning_split") is True and "thinking" not in body

        # Login is modal: while verification runs, input cannot switch models.
        terminal.command("/login", "signintoMiniMax")
        terminal.command("rejected", "loginfailed")
        assert json.loads((home / "auth.json").read_text())["minimax-cn"]["key"] == "saved-mini"
        terminal.command("/login", "signintoMiniMax")
        terminal.command("new-mini", "loggedintominimax-cn")
        assert mini.requests[-1][:3] == ("GET", "/v1/models", "Bearer new-mini")
        terminal.chat(mini, deep, "MiniMax-M3.1-Flash", "new-mini", "after login")

        # Cancellation ends a held response and leaves the provider reusable.
        terminal.output = b""
        os.write(terminal.fd, b"hold\r")
        terminal.wait(lambda: b"holding" in compact(terminal.output), "held stream")
        os.write(terminal.fd, b"\x03")
        terminal.wait(lambda: b"interrupted" in compact(terminal.output), "cancel")
        terminal.settle()
        mini.release.set()
        terminal.chat(mini, deep, "MiniMax-M3.1-Flash", "new-mini", "after cancel")
        terminal.command("/logout", "loggedoutofminimax-cn")
        assert "minimax-cn" not in json.loads((home / "auth.json").read_text())
        count = len(mini.requests) + len(deep.requests)
        terminal.command("blocked", "type/login")
        assert len(mini.requests) + len(deep.requests) == count
        terminal.command("/model deepseek-flash", "model:deepseek-flash")
        terminal.chat(deep, mini, "deepseek-flash", "startup-deep", "back to startup")
        terminal.close()
        terminal = None
        print("ok: actual endpoints, wire models, credentials, history, login/logout and cancellation")

        # No startup key: a successful cross-provider switch must unlock chat.
        second_home = home / "no-startup-key"
        credentials(second_home, mini)
        terminal = Terminal(second_home, binary, ["--base-url", deep.url])
        terminal.command("/model MiniMax-M3.1", "model:MiniMax-M3.1")
        terminal.chat(mini, deep, "MiniMax-M3.1-Flash", "saved-mini", "logged in by switch")
        terminal.close()
        terminal = None
        print("ok: switching from a logged-out startup enables chat")
        print("PASS")
    except BaseException:
        if terminal is not None:
            pathlib.Path("/tmp/pi-rs-tui-model-dump.txt").write_bytes(terminal.output)
            if terminal.status is None:
                os.kill(terminal.pid, signal.SIGKILL)
                os.waitpid(terminal.pid, 0)
            try:
                os.close(terminal.fd)
            except OSError:
                pass
        raise
    finally:
        deep.close()
        mini.close()


if __name__ == "__main__":
    main()
