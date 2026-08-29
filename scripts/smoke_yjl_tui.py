#!/usr/bin/env python3
"""PTY smoke v3：模拟真实终端（应答 kitty/DA1/XTVERSION 查询 + pyte 读屏）。

对照组：无 YJL_TUI → 走 grok 原生路径（预期出现登录/认证界面）。
桥接组：YJL_TUI=1 → 预期无登录页、直接进入主界面，可发消息。
"""
import os
import pty
import re
import select
import signal
import sys
import time

import pyte

BIN = "/home/ksk2kk/YujialeCode/vendor/grok-build/target/release/xai-grok-pager"
COLS, ROWS = 100, 32
ALT = re.compile(rb"\x1b\[\?1049[hl]|\x1b\[\?2026[hl]")

# 真实终端对常见查询的应答
REPLIES = [
    (re.compile(rb"\x1b\[\?u\x1b\[c"), b"\x1b[?0u"),            # kitty keyboard query → 不支持
    (re.compile(rb"\x1b\[c"), b"\x1b[?62;22c"),                  # Primary DA
    (re.compile(rb"\x1b\[>0?q"), b"\x1bP>|xterm(370)\x1b\\"),    # XTVERSION
    (re.compile(rb"\x1b\[6n"), b"\x1b[1;1R"),                    # CPR
]


class Terminal:
    def __init__(self, extra_env):
        env = dict(os.environ)
        env["TERM"] = "xterm-256color"
        env.update(extra_env)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execve(BIN, [BIN], env)
        import fcntl
        import struct
        import termios
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        self.raw = []

    def pump(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([self.fd], [], [], 0.2)
            if not ready:
                continue
            try:
                chunk = os.read(self.fd, 65536)
            except OSError:
                return
            if not chunk:
                return
            self.raw.append(chunk)
            # 对查询序列自动应答（应答要先于下一帧渲染，直接写回）
            for pattern, reply in REPLIES:
                if pattern.search(chunk):
                    os.write(self.fd, reply)
            self.stream.feed(ALT.sub(b"", chunk))

    def send(self, data):
        os.write(self.fd, data.encode() if isinstance(data, str) else data)

    def text(self):
        return "\n".join(line.rstrip() for line in self.screen.display if line.strip())

    def alive(self):
        try:
            return os.waitpid(self.pid, os.WNOHANG)[0] == 0
        except ChildProcessError:
            return False

    def close(self):
        try:
            os.kill(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            os.close(self.fd)
        except OSError:
            pass


def main():
    failed = False

    # ===== 对照组：原生路径（无 YJL_TUI）应出现认证界面 =====
    term = Terminal({})
    term.pump(10)
    text = term.text()
    print("===== 对照组（原生，无 YJL_TUI）=====")
    print(text[:1200] if text else "(空屏)")
    has_auth = any(k in text for k in ("Login", "login", "Sign in", "认证", "API key", "API Key", "x.ai"))
    print(("PASS " if has_auth else "WARN ") + "原生路径出现认证/登录界面（预期 true）")
    term.close()

    # ===== 桥接组 =====
    term = Terminal({"YJL_TUI": "1"})
    term.pump(10)
    text = term.text()
    print("===== 桥接组（YJL_TUI=1）启动屏 =====")
    print(text[:1500] if text else "(空屏)")
    no_login = not any(k in text for k in ("Login", "Sign in", "Select a login", "浏览器", "x.ai 账", "Open x.ai", "browser"))
    print(("PASS " if no_login else "FAIL ") + "无登录页")
    print(("PASS " if text.strip() else "FAIL ") + "主界面已渲染")

    # 发消息
    term.send("只回复两个字：收到\r")
    term.pump(30)
    text2 = term.text()
    print("===== 发消息后 =====")
    print(text2[-1200:] if text2 else "(空屏)")
    print(("PASS " if term.alive() else "FAIL ") + "发消息后进程存活")
    got_reply = any(k in text2 for k in ("收到", "⚠", "错误", "失败"))
    print(("PASS " if got_reply else "WARN ") + "出现回复/错误提示")
    failed |= not no_login

    term.close()
    print("RESULT:", "FAIL" if failed else "PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
