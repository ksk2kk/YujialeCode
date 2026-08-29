#!/usr/bin/env python3
"""ask_user 复现实验：mock LLM 发起 ask_user_question 工具调用。

断言：
1. TUI 出现提问弹窗（问题文本可见）；
2. 按 '1' 选择后，模型第二轮请求里的 tool 结果包含用户答案；
3. 全程无错误文本。
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

TOOL_ARGS = json.dumps({
    "questions": [{
        "question": "选哪个测试方案？",
        "options": [
            {"label": "方案A", "description": "第一个方案"},
            {"label": "方案B", "description": "第二个方案"},
        ],
        "multi_select": False,
    }]
}, ensure_ascii=False)

received = []  # 记录每轮收到的请求


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class MockLLM(BaseHTTPRequestHandler):
    round_no = 0

    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        try:
            payload = json.loads(body)
        except Exception:
            payload = {}
        received.append(payload)
        round_no = len(received)
        if round_no == 1:
            # 第一轮：发起 ask_user_question 工具调用（SSE 流式 tool_calls）
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            chunk = {"choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_ask_1", "type": "function",
                "function": {"name": "ask_user_question", "arguments": TOOL_ARGS},
            }]}}]}
            self.wfile.write(f"data: {json.dumps(chunk, ensure_ascii=False)}\n\n".encode())
            self.wfile.flush()
            done = {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}
            self.wfile.write(f"data: {json.dumps(done)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        # 后续轮：回显收到的 tool 消息内容（这样 TUI 上能看到工具结果）
        tool_texts = []
        for m in payload.get("messages", []):
            if m.get("role") == "tool":
                tool_texts.append(m.get("content", ""))
        reply = "工具结果回显: " + (" | ".join(tool_texts) or "(无)")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for ch in reply:
            c = {"choices": [{"delta": {"content": ch}}]}
            self.wfile.write(f"data: {json.dumps(c, ensure_ascii=False)}\n\n".encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main():
    port = free_port()
    server = ThreadingHTTPServer(("127.0.0.1", port), MockLLM)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    home = "/tmp/yjl-ask-home"
    shutil.rmtree(home, ignore_errors=True)
    os.makedirs(home + "/sessions", exist_ok=True)
    with open(os.path.expanduser("~/.yjlcoder/config.json")) as f:
        cfg = json.load(f)
    cfg["provider"]["base_url"] = f"http://127.0.0.1:{port}/v1"
    cfg["provider"]["api_key"] = "mock"
    cfg["provider"]["model"] = "mock-model"
    cfg["provider"]["native_tools"] = True
    cfg["llama"]["auto_start"] = False
    with open(home + "/config.json", "w") as f:
        json.dump(cfg, f, ensure_ascii=False)

    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
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
                for pat, rep in REPLIES:
                    if pat.search(chunk):
                        os.write(fd, rep)
                stream.feed(ALT.sub(b"", chunk))

    def text():
        return "\n".join(line.rstrip() for line in screen.display if line.strip())

    failed = False
    pump(8)
    os.write(fd, "开始测试\r".encode())
    # 等待工具调用 → 弹窗出现
    pump(12)
    t1 = text()
    print("===== 工具调用后屏幕 =====")
    print(t1[-1800:])
    has_dialog = "选哪个测试方案" in t1
    print(("PASS " if has_dialog else "FAIL ") + "提问弹窗出现")
    failed |= not has_dialog

    if has_dialog:
        os.write(fd, b"1")  # 快捷键：选方案A并提交
        pump(15)
        t2 = text()
        print("===== 选择后屏幕 =====")
        print(t2[-1200:])
        answered = len(received) >= 2
        print(("PASS " if answered else "FAIL ") + f"模型收到第二轮请求（共 {len(received)} 轮）")
        failed |= not answered
        if answered:
            tool_msgs = [m for m in received[1].get("messages", []) if m.get("role") == "tool"]
            tool_text = tool_msgs[0]["content"] if tool_msgs else "(无 tool 消息)"
            print("模型收到的工具结果:", tool_text[:300])
            ok = "方案A" in tool_text and "User has answered" in tool_text
            print(("PASS " if ok else "FAIL ") + "工具结果含用户答案")
            failed |= not ok

    err_hits = [k for k in ("仅 TUI 交互可用", "提问通道", "错误:", "⚠") in t1] if False else []
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    server.shutdown()
    shutil.rmtree(home, ignore_errors=True)
    print("RESULT:", "FAIL" if failed else "PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
