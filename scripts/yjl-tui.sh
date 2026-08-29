#!/usr/bin/env bash
# 启动 Grok Build TUI（一字不差搬运的 xai-grok-pager），默认由 YJLcoder agent 驱动。
# 登录层已移除：默认走 yjl-bridge，不出现 x.ai 登录/浏览器跳转。
# 如需 grok 原生模式（需 x.ai 登录）：YJL_NATIVE=1 scripts/yjl-tui.sh
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/vendor/grok-build/target/release/xai-grok-pager"
if [ ! -x "$BIN" ]; then
    echo "未找到 $BIN；先构建：" >&2
    echo "  cd $ROOT/vendor/grok-build && cargo build --release -p xai-grok-pager-bin" >&2
    exit 1
fi
exec "$BIN"
