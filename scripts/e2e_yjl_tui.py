#!/usr/bin/env python3
"""端到端冒烟：mock OpenAI SSE 服务 + YJLCODER_HOME 沙盒 + 桥接 TUI（无环境变量，默认即桥）。

验证：发消息 → 桥 → YJLcoder Agent → mock LLM 流式回复 → TUI 渲染出回复文本，
全程无登录页。
"""
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pyte

BIN = "/home/ksk2kk/YujialeCode/vendor/grok-build/target/release/xai-grok-pager"
COLS, ROWS = 100, 32
ALT = re.compile(rb"\x1b\[\?1049[hl]|\x1b\[\?2026[hl]")

REPLIES = [
    (re.compile(rb"\x1b\[\?u\x1b\[c"), b"\x1b[?0u"),
    (re.compile(rb"\x1b\[c"), b"\x1b[?62;22c"),
    (re.compile(rb"\x1b\[>0?q"), b"\x1bP>|xterm(370)\x1b\\"),
    (re.compile(rb"\x1b\[6n"), b"\x1b[1;1R"),
]


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


REPLY_TEXT = "收到，桥接链路正常。"


class MockLLM(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        try:
            payload = json.loads(body)
        except Exception:
            payload = {}
        stream = bool(payload.get("stream"))
        reply = REPLY_TEXT
        messages = payload.get("messages", [])
        # 简单回显校验：用户最后一条消息里包含"名字"就回不同内容
        if messages and "你是谁" in json.dumps(messages, ensure_ascii=False):
            reply = "我是 YJLcoder。"
        if not stream:
            data = {
                "choices": [{"message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}],
            }
            out = json.dumps(data).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for char in reply:
            chunk = {
                "choices": [{"delta": {"content": char}}],
            }
            self.wfile.write(f"data: {json.dumps(chunk, ensure_ascii=False)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.04)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main():
    port = free_port()
    server = ThreadingHTTPServer(("127.0.0.1", port), MockLLM)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    home = "/tmp/yjl-smoke-home"
    shutil.rmtree(home, ignore_errors=True)
    os.makedirs(home + "/sessions", exist_ok=True)
    with open(os.path.expanduser("~/.yjlcoder/config.json")) as f:
        cfg = json.load(f)
    cfg["provider"]["base_url"] = f"http://127.0.0.1:{port}/v1"
    cfg["provider"]["api_key"] = "mock-key"
    cfg["provider"]["model"] = "mock-model"
    cfg["llama"]["auto_start"] = False
    with open(home + "/config.json", "w") as f:
        json.dump(cfg, f, ensure_ascii=False)

    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    # 不设任何环境变量：默认即桥接（YJLcoder agent），登录层不可达
    env["YJLCODER_HOME"] = home
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(BIN, [BIN], env)
        os._exit(1)
    import fcntl
    import struct
    import termios
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)

    def pump(seconds):
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    return
                if not chunk:
                    return
                for pattern, reply in REPLIES:
                    if pattern.search(chunk):
                        os.write(fd, reply)
                stream.feed(ALT.sub(b"", chunk))

    def text():
        return "\n".join(line.rstrip() for line in screen.display if line.strip())

    failed = False
    pump(8)
    start_text = text()
    no_login = not any(k in start_text for k in ("Login", "Sign in", "browser", "Approve"))
    print(("PASS " if no_login else "FAIL ") + "无登录页")
    failed |= not no_login

    os.write(fd, "你是谁\r".encode())
    pump(20)
    final_text = text()
    print("===== 回合后屏幕 =====")
    print(final_text[-1500:])
    got = "YJLcoder" in final_text
    print(("PASS " if got else "FAIL ") + "模型流式回复渲染到 TUI（找到 'YJLcoder'）")
    failed |= not got
    alive = True
    try:
        os.waitpid(pid, os.WNOHANG)
    except ChildProcessError:
        alive = False
    print(("PASS " if alive else "FAIL ") + "进程存活")

    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    server.shutdown()
    print("RESULT:", "FAIL" if failed else "PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
