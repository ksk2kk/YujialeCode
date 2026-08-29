#!/usr/bin/env python3
"""首启 onboarding 全流程 e2e：空配置 → 向导三卡 → 落盘 → 对话继续。"""
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
COLS, ROWS = 100, 40
ALT = re.compile(rb"\x1b\[\?1049[hl]|\x1b\[\?2026[hl]")
REPLIES = [
    (re.compile(rb"\x1b\[\?u\x1b\[c"), b"\x1b[?0u"),
    (re.compile(rb"\x1b\[c"), b"\x1b[?62;22c"),
    (re.compile(rb"\x1b\[>0?q"), b"\x1bP>|xterm(370)\x1b\\"),
    (re.compile(rb"\x1b\[6n"), b"\x1b[1;1R"),
]
reqs = []


class Mock(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        if self.path.endswith("/models"):
            out = json.dumps({"data": [{"id": "mock-a"}, {"id": "mock-b"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)
            return
        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        reqs.append(json.loads(self.rfile.read(n)))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for ch in "配置后对话正常":
            self.wfile.write(f"data: {json.dumps({'choices':[{'delta':{'content':ch}}]}, ensure_ascii=False)}\n\n".encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    srv = ThreadingHTTPServer(("127.0.0.1", port), Mock)
    threading.Thread(target=srv.serve_forever, daemon=True).start()

    home = "/tmp/yjl-onboard-home"
    shutil.rmtree(home, ignore_errors=True)
    os.makedirs(home + "/sessions", exist_ok=True)
    cfg = json.load(open(os.path.expanduser("~/.yjlcoder/config.json")))
    # 出厂默认三件套（触发 needs_onboarding）
    cfg["provider"]["base_url"] = "https://api.deepseek.com"
    cfg["provider"]["api_key"] = ""
    cfg["provider"]["model"] = "deepseek-v4-flash"
    cfg["llama"]["auto_start"] = False
    json.dump(cfg, open(home + "/config.json", "w"), ensure_ascii=False)

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

    def pump(sec):
        end = time.time() + sec
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.2)
            if r:
                try:
                    c = os.read(fd, 65536)
                except OSError:
                    return
                if not c:
                    return
                for p, rep in REPLIES:
                    if p.search(c):
                        os.write(fd, rep)
                stream.feed(ALT.sub(b"", c))

    def text():
        return "\n".join(l.rstrip() for l in screen.display if l.strip())

    failed = False

    def check(name, ok):
        nonlocal failed
        print(("PASS " if ok else "FAIL ") + name)
        failed |= not ok

    pump(10)
    os.write(fd, "你好\r".encode())
    pump(10)  # 向导通知 + 卡1
    t1 = text()
    check("无 x 登录页", not any(k in t1 for k in ("Login", "Sign in", "Approve in your browser")))
    check("首启自动弹向导（服务商卡）", "选择模型服务" in t1)

    # 卡1：z 进 Other 输入，输入 mock 地址，Enter 提交
    os.write(fd, b"z")
    pump(1)
    os.write(fd, f"http://127.0.0.1:{port}/v1\r".encode())
    pump(6)  # 本地地址 → 跳过 key 卡 → 模型卡（拉 /v1/models）
    t2 = text()
    check("模型卡出现且来自服务端实时列表", "选择主模型" in t2 and "mock-a" in t2 and "mock-b" in t2)

    # 卡3：快捷键 1 选 mock-a（选择并提交）
    os.write(fd, b"1")
    pump(12)  # 落盘 + 原消息继续发给 mock
    t3 = text()
    check("配置完成回显", "配置完成" in t3)
    check("原对话继续（mock 回复渲染）", "配置后对话正常" in t3)
    check("mock 收到对话请求", len(reqs) >= 1)

    saved = json.load(open(home + "/config.json"))
    check(
        f"配置落盘 base_url/model（{saved['provider']['base_url']} · {saved['provider']['model']}）",
        str(port) in saved["provider"]["base_url"] and saved["provider"]["model"] == "mock-a",
    )

    # /cfg show 总览（/config 被 TUI 内置设置面板占用）
    os.write(fd, b"/cfg show\r")
    pump(5)
    t4 = text()
    check("/cfg show 总览回显", "配置总览" in t4 and "mock-a" in t4)

    # /setup 重跑（先 Esc 清掉可能残留的面板）
    os.write(fd, b"\x1b"); pump(1)
    os.write(fd, b"\x1b"); pump(1)
    os.write(fd, b"/setup\r")
    pump(8)
    t5 = text()
    check("/setup 重新打开向导", "选择模型服务" in t5)
    os.write(fd, b"\x1b")  # Esc 退出输入
    pump(1)
    os.write(fd, b"\x1b")  # Esc 取消卡片
    pump(2)

    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    srv.shutdown()
    shutil.rmtree(home, ignore_errors=True)
    print("RESULT:", "FAIL" if failed else "PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
